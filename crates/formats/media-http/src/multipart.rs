//! `multipart/form-data` (RFC 7578), read from a whole body a client sent
//! and written to a provider. A client's body is untrusted: reading it never
//! panics, holds no more than the body, and refuses what it cannot read
//! one way with a 400.

use futures::stream;
use respan_gateway_core::Fault;
use respan_gateway_http::bytes::{BufMut, Bytes, BytesMut};
use respan_gateway_http::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};

/// The most parts a body may have.
const MAX_PARTS: usize = 1024;

/// One part of a form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Part {
    /// Its field name.
    pub name: String,
    /// The file name it was sent as, if it is a file.
    pub filename: Option<String>,
    /// Its `Content-Type`, if it has one.
    pub content_type: Option<String>,
    /// Its content.
    pub body: Bytes,
}

/// The boundary of a `multipart/form-data` content type, if it is one.
pub(crate) fn boundary(content_type: &str) -> Option<String> {
    let boundary = multer::parse_boundary(content_type).ok()?;
    // RFC 2046 5.1.1: 1 to 70 characters.
    (1..=70).contains(&boundary.len()).then_some(boundary)
}

/// The parts of `body`, delimited by `boundary`. The body is whole, so
/// reading it never waits.
pub(crate) fn parse(boundary: &str, body: &Bytes) -> Result<Vec<Part>, Fault> {
    let body = stream::once(async { Ok::<_, std::convert::Infallible>(body.clone()) });
    let mut form = multer::Multipart::new(body, boundary);
    futures::executor::block_on(async {
        let mut parts = Vec::new();
        while let Some(field) = form.next_field().await.map_err(unreadable)? {
            if parts.len() == MAX_PARTS {
                return Err(bad("has too many parts"));
            }
            let headers = field.headers();
            let one = |name| {
                let value = headers.get(name).map(|v| v.to_str().map(str::to_owned));
                value
                    .transpose()
                    .map_err(|_| bad("has headers that are not text"))
            };
            let disposition = one(CONTENT_DISPOSITION)?
                .ok_or_else(|| bad("has a part with no Content-Disposition"))?;
            let content_type = one(CONTENT_TYPE)?;
            let (name, filename) = disposed(&disposition)?;
            let body = field.bytes().await.map_err(unreadable)?;
            parts.push(Part {
                name,
                filename,
                content_type,
                body,
            });
        }
        Ok(parts)
    })
}

/// The field name and file name of a part's `Content-Disposition`.
fn disposed(disposition: &str) -> Result<(String, Option<String>), Fault> {
    let mut params = split_params(disposition).into_iter();
    if !params
        .next()
        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("form-data"))
    {
        return Err(bad("has a part that is not form-data"));
    }
    let params: Vec<String> = params.collect();
    let named = |key: &str| params.iter().find_map(|p| param(p, key));
    let name = named("name").ok_or_else(|| bad("has a part with no name"))?;
    // RFC 7578 4.2 deprecates `filename*`, but clients still send it.
    let filename = named("filename*")
        .and_then(|f| extended(&f))
        .or_else(|| named("filename"));
    Ok((name, filename))
}

/// A 400 for a body multer could not read.
fn unreadable(e: multer::Error) -> Fault {
    bad("cannot be read").with_source(e)
}

/// A header value split at each `;` outside quotes.
fn split_params(value: &str) -> Vec<String> {
    let (mut out, mut current, mut quoted, mut escaped) = (Vec::new(), String::new(), false, false);
    for c in value.chars() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => {
                out.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    out.push(current);
    out
}

/// The value of parameter `p` if it is `key`'s, unquoted.
fn param(p: &str, key: &str) -> Option<String> {
    let (name, value) = p.split_once('=')?;
    if !name.trim().eq_ignore_ascii_case(key) {
        return None;
    }
    let value = value.trim();
    let Some(quoted) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return Some(value.to_owned());
    };
    let (mut out, mut escaped) = (String::new(), false);
    for c in quoted.chars() {
        match c {
            '\\' if !escaped => escaped = true,
            c => {
                out.push(c);
                escaped = false;
            }
        }
    }
    Some(out)
}

/// An RFC 8187 value, `UTF-8''name%20escaped`, decoded.
fn extended(value: &str) -> Option<String> {
    let (charset, rest) = value.split_once('\'')?;
    let (_, encoded) = rest.split_once('\'')?;
    charset.eq_ignore_ascii_case("utf-8").then(|| {
        percent_encoding::percent_decode_str(encoded)
            .decode_utf8_lossy()
            .into_owned()
    })
}

/// A 400 for a body that `what`.
fn bad(what: &str) -> Fault {
    Fault::bad_request(format!("the multipart body {what}"))
}

/// A random boundary, which no content holds but by a chance of 2^-122.
pub(crate) fn random_boundary() -> String {
    format!("respan-{}", uuid::Uuid::new_v4().simple())
}

/// Writes a form.
#[derive(Debug)]
pub(crate) struct Writer {
    boundary: String,
    body: BytesMut,
}

impl Writer {
    /// A form delimited by `boundary`.
    pub fn with_boundary(boundary: String) -> Self {
        Self {
            boundary,
            body: BytesMut::new(),
        }
    }

    /// Its content type, with its boundary.
    pub fn content_type(&self) -> String {
        format!("multipart/form-data; boundary={}", self.boundary)
    }

    /// Add the text field `name`.
    pub fn text(&mut self, name: &str, value: &str) {
        self.head(name, None, None);
        self.body.put_slice(value.as_bytes());
        self.body.put_slice(b"\r\n");
    }

    /// Add the file `filename` of type `content_type` as field `name`.
    pub fn file(&mut self, name: &str, filename: &str, content_type: &str, content: &[u8]) {
        self.head(name, Some(filename), Some(content_type));
        self.body.put_slice(content);
        self.body.put_slice(b"\r\n");
    }

    fn head(&mut self, name: &str, filename: Option<&str>, content_type: Option<&str>) {
        let mut head = format!(
            "--{}\r\nContent-Disposition: form-data; name=\"{}\"",
            self.boundary,
            escape(name)
        );
        if let Some(filename) = filename {
            head.push_str(&format!("; filename=\"{}\"", escape(filename)));
        }
        if let Some(content_type) = content_type {
            head.push_str(&format!("\r\nContent-Type: {}", line(content_type)));
        }
        head.push_str("\r\n\r\n");
        self.body.put_slice(head.as_bytes());
    }

    /// The whole body, closed.
    pub fn finish(mut self) -> Bytes {
        self.body
            .put_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        self.body.freeze()
    }
}

/// A name as a quoted string holds it, as browsers escape one (HTML's
/// multipart/form-data encoding): `"`, CR and LF percent-escaped.
fn escape(name: &str) -> String {
    name.replace('"', "%22")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// A header value with no line ends, so it cannot start another header.
fn line(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::FaultKind;

    fn parts(body: &'static [u8]) -> Result<Vec<Part>, Fault> {
        parse("XyZ", &Bytes::from_static(body))
    }

    #[test]
    fn a_form_is_read_as_written() {
        let mut form = Writer::with_boundary("XyZ".into());
        form.text("prompt", "a \"cat\"\r\n--XyQ");
        form.file("image[]", "c\"at.png", "image/png", b"\x89PNG\r\n\r\n--Xy");
        let content_type = form.content_type();
        let body = form.finish();
        let boundary = boundary(&content_type);
        assert_eq!(boundary.as_deref(), Some("XyZ"));
        let read = parse("XyZ", &body).expect("a form");
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].name, "prompt");
        assert_eq!(&read[0].body[..], b"a \"cat\"\r\n--XyQ");
        assert_eq!(read[1].name, "image[]");
        assert_eq!(read[1].filename.as_deref(), Some("c%22at.png"));
        assert_eq!(read[1].content_type.as_deref(), Some("image/png"));
        assert_eq!(&read[1].body[..], b"\x89PNG\r\n\r\n--Xy");
    }

    #[test]
    fn a_boundary_is_read_however_it_is_quoted() {
        for (content_type, want) in [
            ("multipart/form-data; boundary=abc", Some("abc")),
            ("Multipart/Form-Data;boundary=\"a b;c\"", Some("a b;c")),
            ("multipart/form-data; charset=utf-8; BOUNDARY=x", Some("x")),
            ("multipart/form-data", None),
            ("multipart/form-data; boundary=", None),
            ("multipart/mixed; boundary=abc", None),
            ("application/json", None),
        ] {
            assert_eq!(boundary(content_type).as_deref(), want, "{content_type}");
        }
        let long = format!("multipart/form-data; boundary={}", "x".repeat(71));
        assert_eq!(boundary(&long), None);
    }

    #[test]
    fn a_preamble_padding_and_extended_file_names_are_read() {
        let read = parts(
            b"preamble\r\n--XyZ  \r\ncontent-disposition: form-data; name=file; \
              filename=\"a.wav\"; filename*=UTF-8''%C3%A9t%C3%A9.wav\r\n\r\nRIFF\r\n--XyZ--\r\nepilogue",
        )
        .expect("a form");
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].filename.as_deref(), Some("été.wav"));
        assert_eq!(&read[0].body[..], b"RIFF");
    }

    #[test]
    fn what_cannot_be_read_one_way_is_refused() {
        for body in [
            &b""[..],
            b"--XyZ",
            b"--XyZ\r\n",
            b"--XyZ\r\nContent-Disposition: form-data; name=a\r\n\r\nno end",
            b"--XyZ\r\nContent-Disposition: form-data; name=a\r\n",
            b"--XyZ\r\nContent-Type: text/plain\r\n\r\nx\r\n--XyZ--",
            b"--XyZ\r\nContent-Disposition: attachment; name=a\r\n\r\nx\r\n--XyZ--",
            b"--XyZ\r\nContent-Disposition: form-data\r\n\r\nx\r\n--XyZ--",
            b"--XyZ\r\nno colon\r\n\r\nx\r\n--XyZ--",
            b"--XyZ\r\nContent-Disposition: form-data; name=a\r\n\r\nx\r\n--XyZjunk",
            b"--XyZ\r\nContent-Disposition: form-data; name=\xff\r\n\r\nx\r\n--XyZ--",
        ] {
            let read = parts(body);
            assert!(
                read.as_ref().is_err_and(|f| f.is(FaultKind::Client)),
                "{:?}: {read:?}",
                String::from_utf8_lossy(body)
            );
        }
        assert_eq!(parts(b"--XyZ--\r\n").map(|p| p.len()).ok(), Some(0));
        // A header given twice is read as its last: the form is written
        // anew for the provider, so no reader downstream reads the first.
        let twice = parts(
            b"--XyZ\r\nContent-Disposition: form-data; name=a\r\n\
              Content-Disposition: form-data; name=b\r\n\r\nx\r\n--XyZ--",
        );
        assert_eq!(twice.map(|p| p[0].name.clone()).ok().as_deref(), Some("b"));
    }

    #[test]
    fn every_cut_of_a_form_is_read_or_refused_without_panicking() {
        let mut form = Writer::with_boundary("XyZ".into());
        form.text("a", "1");
        form.file("f", "x.bin", "application/octet-stream", b"\r\n-\r\n--X");
        let body = form.finish();
        for end in 0..body.len() {
            for start in 0..end.min(8) {
                let _ = parse("XyZ", &body.slice(start..end));
            }
        }
        assert_eq!(parse("XyZ", &body).map(|p| p.len()).ok(), Some(2));
    }

    #[test]
    fn a_header_cannot_end_its_line_early() {
        let mut form = Writer::with_boundary("b".into());
        form.file("x\r\nX-Evil: 1", "y\nz", "image/png\r\nX-Evil: 1", b"");
        let read = parse("b", &form.finish()).expect("a form");
        assert_eq!(read[0].name, "x%0D%0AX-Evil: 1");
        assert_eq!(
            read[0].content_type.as_deref(),
            Some("image/png  X-Evil: 1")
        );
    }
}
