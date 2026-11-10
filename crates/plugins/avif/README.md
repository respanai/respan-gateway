# respan-gateway-plugin-avif

`AvifToPng` turns AVIF images in a decision request into PNG before the call,
for a provider that reads PNG and JPEG but not AVIF, such as span-02. Register
it on that provider alone:

```rust,ignore
use respan_gateway_plugin_avif::AvifToPng;

let span_02 = Decisions
    .provider("span-02".into(), connect)
    .plugin(AvifToPng::default());
```

An image is AVIF by its bytes (an `ftyp` box naming `avif`), whatever its
label says. It converts:

- a base64 `data:` URL anywhere in the request (the state, the questions, and
  fields the kind has no place for, such as System One's `images`);
- a bare base64 item of `images`;
- an object's `data_base64` (System One's `assets`), whose `format` becomes `png`.

Images at a URL, and every other image, pass as sent.

Decoding is pure Rust: [rav1d](https://github.com/memorysafety/rav1d), the
port of dav1d, built without its assembly, so no nasm and no C toolchain. It
runs off the async runtime. A still image is read: one AV1 frame or a grid of
them, with alpha, rotation and mirror, 8, 10 or 12 bits, any chroma layout,
converted by the matrix and range the file states. A 12-megapixel photo
takes about half a second.

An AVIF it cannot read (an image sequence, a damaged file, or one over
`MAX_PIXELS`, 40 million, or `AvifToPng::with_max_pixels`) refuses the request
as a bad request that says which image and why. It does not send the
provider a file it cannot read.
