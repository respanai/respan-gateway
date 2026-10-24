//! How a request's path is matched and forwarded. Matching reads a path
//! liberally, as any front end might, so a spelling cannot dodge a match;
//! forwarding keeps the client's bytes but drops what matching dropped, so
//! the provider never gets a structure matching did not see.

use std::borrow::Cow;

use http::request::Parts as RequestHead;
use respan_gateway_core::Fault;

/// `s` with its `%XX` escapes decoded, a bad UTF-8 sequence replaced.
pub(crate) fn decoded(s: &str) -> Cow<'_, str> {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy()
}

/// A path as every front end reads it for matching: percent escapes
/// decoded (a `%2F` too, as a front end might read it a slash, so matching
/// is the liberal reading), `;` parameters dropped, dot-segments removed
/// (RFC 3986 5.2.4) and empty segments collapsed, and a backslash read as a
/// slash (as the outbound client reads it), so `/v1/chat/./completions`,
/// `/v1//chat/completions` and `/x/../v1/chat/completions/` all spell
/// `/v1/chat/completions`.
pub fn clean_path(path: &str) -> String {
    // Match what is forwarded (see `forwarded_path`), so the provider never
    // gets a structure matching did not see.
    let path = &forwarded_path(path);
    let mut kept: Vec<&str> = Vec::new();
    let decoded = decoded(path);
    // The outbound client reads a backslash as a slash, so a match must too.
    for segment in decoded.split(['/', '\\']) {
        let segment = segment.split(';').next().unwrap_or_default();
        match segment {
            "" | "." => {}
            ".." => drop(kept.pop()),
            _ => kept.push(segment),
        }
    }
    // Case-insensitive routers (ASP.NET Core, Express) read `/V1/Chat` as
    // `/v1/chat`: matching is the liberal reading.
    kept.iter()
        .fold(String::new(), |out, s| out + "/" + s)
        .to_ascii_lowercase()
}

/// A request's path as a relay sends it on: the segments of the path
/// [`clean_path`] matched, each byte for byte as the client spelled it (a
/// vendor's `%2F`, `:` or `;v=1` mean something), with only what matching
/// drops gone: empty and dot-segments (however escaped, or given `;`
/// parameters), a `..` taking the segment before it, and a backslash a
/// slash. An escaped `%2F` stays inside its segment here, where matching
/// may read it a slash: matching reads at least what the provider does, so
/// the provider never gets a structure the router did not look at.
pub fn forwarded_path(path: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    for raw in path.split(['/', '\\']) {
        let name = raw.split(';').next().unwrap_or_default();
        match &*decoded(name) {
            "" | "." => {}
            ".." => drop(kept.pop()),
            _ => kept.push(raw),
        }
    }
    match kept.join("/") {
        joined if joined.is_empty() => "/".to_owned(),
        joined => format!("/{joined}"),
    }
}

/// `text` escaped as a path segment or query value: all but RFC 3986's
/// unreserved characters, so a model id such as an ARN stays one segment.
pub fn escape(text: &str) -> String {
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
    const UNRESERVED: &percent_encoding::AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    utf8_percent_encode(text, UNRESERVED).to_string()
}

/// Whether a request's path, in `uri`, ends in `suffix`, however it is
/// spelled (see [`clean_path`]).
pub fn path_ends_with(uri: &http::Uri, suffix: &str) -> bool {
    let (path, suffix) = (clean_path(uri.path()), clean_path(suffix));
    // Suffix-pattern routers (Spring MVC's `useSuffixPatternMatch`, Rails'
    // `(.:format)`) read `/chat/completions.json` as `/chat/completions`.
    let (dir, last) = path.rsplit_once('/').unwrap_or_default();
    let bare = match last.split_once('.') {
        Some((name, _)) => format!("{dir}/{name}"),
        None => path.clone(),
    };
    path.ends_with(&suffix) || bare.ends_with(&suffix)
}

/// Set a call's path, relative to the transport, keeping its query. `Err`
/// with `internal` if `path` is no path.
pub fn set_path(head: &mut RequestHead, path: &str) -> Result<(), Fault> {
    let uri = match head.uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_owned(),
    };
    head.uri = uri.parse().map_err(|e| {
        Fault::internal(format!("a call's path {path:?} is no path")).with_source(e)
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(method: http::Method, uri: &str) -> RequestHead {
        let req = http::Request::builder().method(method).uri(uri).body(());
        req.unwrap_or_default().into_parts().0
    }

    fn posts_to(head: &RequestHead, suffix: &str) -> bool {
        head.method == http::Method::POST && path_ends_with(&head.uri, suffix)
    }

    #[test]
    fn a_path_is_forwarded_as_spelled_minus_what_matching_drops() {
        for (sent, forwarded) in [
            ("/a/dir%2Ffile:x@y;v=1/%FF", "/a/dir%2Ffile:x@y;v=1/%FF"),
            ("/a//b/./c\\d/", "/a/b/c/d"),
            ("/a/b/%2e%2E/c", "/a/c"),
            ("/a/b/.;x/../c", "/a/c"),
            ("/../..", "/"),
        ] {
            assert_eq!(forwarded_path(sent), forwarded, "{sent}");
        }
    }

    #[test]
    fn a_path_is_matched_however_it_is_spelled() {
        for path in [
            "/v1/chat/completions",
            "/v1/chat/completions/",
            "/v1/chat/completions//?x=1",
            "/v1/chat/completions%2F",
            "/v1/chat/%63ompletions",
            "/v1%2Fchat/completions",
        ] {
            let head = head(http::Method::POST, path);
            assert!(posts_to(&head, "/chat/completions"), "{path}");
        }
        // What a front end would resolve to the chat path is chat, or a
        // passthrough served after it would relay the call past chat plugins.
        for path in [
            "/v1/chat//completions",
            "/v1/chat/./completions",
            "/v1/chat/x/../completions",
            "/x/../v1/chat/completions",
            "/v1/chat/completions/.",
            "/v1/chat/%2e/completions",
            "/v1/chat/completions;jsessionid=1",
            "/v1/chat/x/..;/completions",
            "/../../v1/chat/completions",
        ] {
            let head = head(http::Method::POST, path);
            assert!(posts_to(&head, "/chat/completions"), "{path}");
        }
        for path in [
            "/v1/chat/completionsx",
            "/v1/chat/x/completions",
            "/v1/chat/completions/../x",
        ] {
            let other = head(http::Method::POST, path);
            assert!(!posts_to(&other, "/chat/completions"), "{path}");
        }
        let get = head(http::Method::GET, "/v1/chat/completions");
        assert!(!posts_to(&get, "/chat/completions"));
    }

    #[test]
    fn an_escaped_model_id_is_one_segment_that_decodes_back() {
        for id in [
            "anthropic.claude-haiku-4-5-20251001-v1:0",
            "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.amazon.nova-lite-v1:0",
            "a b?c#d%e",
        ] {
            let escaped = escape(id);
            assert!(!escaped.contains(['/', ':', '?', '#', ' ']), "{escaped}");
            assert_eq!(decoded(&escaped), id);
        }
        assert_eq!(escape("cohere.embed-v4_0~x"), "cohere.embed-v4_0~x");
    }

    #[test]
    fn only_well_formed_escapes_decode() {
        assert_eq!(decoded("%2e%2E%zz%4"), "..%zz%4");
        assert_eq!(decoded("%FF"), "\u{FFFD}");
    }
}
