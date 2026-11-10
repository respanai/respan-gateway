use super::*;
use respan_gateway_core::NativeFields;
use serde_json::json;

// Made with ImageMagick (libheif and its AV1 encoder); each `.ref.png` is
// ImageMagick's own decode of the file beside it.
const G420: &[u8] = include_bytes!("../tests/fixtures/g420.avif"); // 16x12, 8-bit 4:2:0
const A444: &[u8] = include_bytes!("../tests/fixtures/a444.avif"); // 9x7, 10-bit 4:4:4, half alpha
const GRAY: &[u8] = include_bytes!("../tests/fixtures/gray.avif"); // 8x8, 8-bit monochrome
const H422: &[u8] = include_bytes!("../tests/fixtures/h422.avif"); // 12x8, 12-bit 4:2:2

/// (width, height, channels, pixels) of a PNG.
fn read_png(bytes: &[u8]) -> (u32, u32, usize, Vec<u8>) {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes.to_vec()));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().expect("png");
    let mut buf = vec![0; reader.output_buffer_size().expect("size")];
    let info = reader.next_frame(&mut buf).expect("frame");
    buf.truncate(info.buffer_size());
    let channels = buf.len() / (info.width * info.height) as usize;
    (info.width, info.height, channels, buf)
}

/// Each channel within `tolerance` of the reference, as RGB(A).
fn assert_close(avif: &[u8], reference: &str, tolerance: i32) {
    let ours = read_png(&avif_to_png(avif, MAX_PIXELS).expect("converts"));
    let path = format!("{}/tests/fixtures/{reference}", env!("CARGO_MANIFEST_DIR"));
    let theirs = read_png(&std::fs::read(path).expect("reference"));
    assert_eq!((ours.0, ours.1), (theirs.0, theirs.1), "size");
    let rgba = |(_, _, n, px): &(u32, u32, usize, Vec<u8>)| -> Vec<[i32; 4]> {
        px.chunks_exact(*n)
            .map(|p| match n {
                1 => [p[0].into(), p[0].into(), p[0].into(), 255],
                2 => [p[0].into(), p[0].into(), p[0].into(), p[1].into()],
                3 => [p[0].into(), p[1].into(), p[2].into(), 255],
                _ => [p[0].into(), p[1].into(), p[2].into(), p[3].into()],
            })
            .collect()
    };
    let (a, b) = (rgba(&ours), rgba(&theirs));
    let worst = a
        .iter()
        .zip(&b)
        .flat_map(|(a, b)| (0..4).map(move |i| (a[i] - b[i]).abs()))
        .max();
    assert!(
        worst.unwrap_or(0) <= tolerance,
        "{reference}: off by {worst:?}"
    );
}

#[test]
fn converts_like_libheif() {
    assert_close(G420, "g420.ref.png", 3);
    assert_close(A444, "a444.ref.png", 3);
    assert_close(GRAY, "gray.ref.png", 3);
    assert_close(H422, "h422.ref.png", 3);
}

#[test]
fn keeps_alpha() {
    let (_, _, channels, px) = read_png(&avif_to_png(A444, MAX_PIXELS).expect("converts"));
    assert_eq!(channels, 4);
    assert!(
        px.as_chunks::<4>()
            .0
            .iter()
            .all(|p| (120..=136).contains(&p[3]))
    );
}

#[test]
fn sniffs_by_bytes() {
    assert!(container::is_avif(G420));
    assert!(!container::is_avif(b"\x89PNG\r\n\x1a\n"));
    assert!(starts_avif(&BASE64.encode(G420)));
}

#[test]
fn refuses_over_the_limit_and_damaged_files() {
    assert!(avif_to_png(G420, 100).is_err());
    assert!(avif_to_png(&G420[..G420.len() / 2], MAX_PIXELS).is_err());
}

fn request() -> Input {
    let avif = BASE64.encode(G420);
    let mut input = Input::new(
        json!([{"role": "user", "content": [{"type": "input_image", "image_url": format!("data:image/avif;base64,{avif}")}]}]),
        Vec::new(),
    );
    let fields = json!({
        "images": [format!("data:application/octet-stream;base64,{avif}"), avif.clone(), "data:image/png;base64,iVBORw0KGgo="],
        "assets": {"door": {"type": "image", "format": "jpeg", "data_base64": avif}},
    });
    let Value::Object(fields) = fields else {
        unreachable!()
    };
    input.native = Some(NativeFields::new("decisions", fields));
    input
}

#[test]
fn rewrites_every_avif_and_nothing_else() {
    let mut input = request();
    let found = avif_strings(&input);
    assert_eq!(
        found.len(),
        3,
        "the bare base64 counts once, wherever it is"
    );
    let converted = convert(found, MAX_PIXELS).expect("converts");
    rewrite(&mut input, &converted);
    let png = |s: &str| {
        BASE64
            .decode(s)
            .map(|b| b.starts_with(b"\x89PNG"))
            .unwrap_or(false)
    };
    let state = input.state[0]["content"][0]["image_url"]
        .as_str()
        .expect("url");
    assert!(state.starts_with(PNG_DATA_URL) && png(&state[PNG_DATA_URL.len()..]));
    let fields = &input.native.as_ref().expect("native").fields;
    let images = fields["images"].as_array().expect("images");
    assert!(
        images[0]
            .as_str()
            .is_some_and(|s| s.starts_with(PNG_DATA_URL))
    );
    assert!(images[1].as_str().is_some_and(png));
    assert_eq!(images[2], "data:image/png;base64,iVBORw0KGgo=");
    let door = &fields["assets"]["door"];
    assert_eq!(door["format"], "png");
    assert!(door["data_base64"].as_str().is_some_and(png));
}

#[test]
fn a_request_without_avif_is_left_alone() {
    let input = Input::new(json!("is the door open?"), Vec::new());
    assert!(avif_strings(&input).is_empty());
}

#[test]
fn an_unreadable_avif_is_a_bad_request() {
    let mut damaged = G420.to_vec();
    damaged.truncate(64);
    let s = BASE64.encode(&damaged);
    let fault = convert(vec![(s, Carrier::Bare)], MAX_PIXELS).expect_err("refuses");
    assert!(
        fault.message().contains("AVIF image 1"),
        "{}",
        fault.message()
    );
}

/// Through a gateway: a System One request with AVIF images reaches the
/// provider with PNGs, and its answer comes back.
#[tokio::test]
async fn a_call_is_sent_its_images_as_png() {
    use respan_gateway_core::futures::{FutureExt, StreamExt};
    use respan_gateway_core::{Gateway, Provider};
    use respan_gateway_decision::Output;
    use respan_gateway_decision_http::Decisions;
    use respan_gateway_http::{Body, http};
    use std::sync::Mutex;

    async fn read(body: Body) -> Vec<u8> {
        let chunks: Vec<_> = body.collect().await;
        chunks
            .into_iter()
            .flat_map(|c| c.map(|c| c.to_vec()).unwrap_or_default())
            .collect()
    }
    let sent = Arc::new(Mutex::new(Value::Null));
    let seen = Arc::clone(&sent);
    let connect = respan_gateway_http::connect(
        move |request: http::Request<Body>| {
            let seen = Arc::clone(&seen);
            async move {
                let body = read(request.into_body()).await;
                *seen.lock().expect("lock") = serde_json::from_slice(&body).unwrap_or_default();
                let reply =
                    json!({"model": "span-02", "answers": {"open": {"type": "noul", "noul": 0.9}}});
                let mut response = http::Response::new(Body::full(reply.to_string()));
                let json = http::HeaderValue::from_static("application/json");
                response
                    .headers_mut()
                    .insert(http::header::CONTENT_TYPE, json);
                Ok((response, None))
            }
            .boxed()
        },
        1 << 20,
    );
    let gateway = Gateway::new()
        .client::<Input, Output, _>(Decisions)
        .and_then(|g| {
            g.provider(Provider::new("span-02", Decisions, connect).plugin(AvifToPng::default()))
        })
        .and_then(|g| g.target("span-02"))
        .expect("a gateway");
    let avif = BASE64.encode(G420);
    let body = json!({
        "model": "span-02",
        "state": "is the door open?",
        "images": [format!("data:image/avif;base64,{avif}")],
        "assets": {"door": {"type": "image", "format": "jpeg", "data_base64": avif}},
        "questions": {"open": {"type": "noul", "instructions": "Is the door open?"}},
    });
    let mut request = http::Request::new(Body::full(body.to_string()));
    *request.method_mut() = http::Method::POST;
    *request.uri_mut() = http::Uri::from_static("/v1/systemone");
    let (response, _) = respan_gateway_http::handle(&gateway, request, 1 << 20).await;
    let status = response.status();
    let answer: Value =
        serde_json::from_slice(&read(response.into_body()).await).unwrap_or_default();
    gateway.idle().await;
    assert_eq!(status, http::StatusCode::OK, "{answer}");
    assert_eq!(answer["answers"]["open"]["noul"], 0.9, "{answer}");
    let sent = sent.lock().expect("lock").clone();
    let image = sent["images"][0].as_str().expect("an image");
    assert!(image.starts_with(PNG_DATA_URL), "{image:.40}");
    assert_eq!(sent["assets"]["door"]["format"], "png");
    let png = BASE64
        .decode(
            sent["assets"]["door"]["data_base64"]
                .as_str()
                .expect("data"),
        )
        .expect("base64");
    assert_eq!(read_png(&png).0, 16);
}
