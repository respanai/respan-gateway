//! A still AVIF as an 8-bit PNG: each AV1 frame decoded, its YUV turned to
//! RGB by the matrix and range the file states, grid tiles laid out, alpha
//! applied, then its rotation and mirror.

use crate::av1::{self, Frame};
use crate::container::{self, Coded, Error, Nclx, Transform};

/// An 8-bit RGBA image.
struct Rgba {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

/// The PNG of the AVIF `file`. `max_pixels` bounds the image and each frame.
pub fn avif_to_png(file: &[u8], max_pixels: u32) -> Result<Vec<u8>, Error> {
    let still = container::parse(file)?;
    let mut image = picture(&still.color, still.nclx, max_pixels, rgb)?;
    if let Some(alpha) = &still.alpha {
        let a = picture(alpha, None, max_pixels, |f, _| gray(f))?;
        if (a.width, a.height) != (image.width, image.height) {
            return Err(format!(
                "alpha is {}x{}, the image {}x{}",
                a.width, a.height, image.width, image.height
            ));
        }
        for (px, a) in image
            .pixels
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(a.pixels.as_chunks::<4>().0.iter())
        {
            px[3] = a[0];
        }
    }
    for t in &still.transforms {
        image = transform(image, *t);
    }
    png(&image)
}

/// `coded` decoded and laid out, each frame converted by `convert`.
fn picture(
    coded: &Coded,
    nclx: Option<Nclx>,
    max_pixels: u32,
    convert: impl Fn(&Frame, Option<Nclx>) -> Vec<u8>,
) -> Result<Rgba, Error> {
    match coded {
        Coded::Frame(obus) => {
            let f = av1::decode(obus, max_pixels)?;
            Ok(Rgba {
                width: f.width,
                height: f.height,
                pixels: convert(&f, nclx),
            })
        }
        Coded::Grid {
            rows,
            columns,
            width,
            height,
            tiles,
        } => {
            let (width, height) = (*width, *height);
            if width == 0 || height == 0 || width.saturating_mul(height) > max_pixels as usize {
                return Err(format!("a {width}x{height} grid is over the size limit"));
            }
            let mut pixels = vec![0u8; width * height * 4];
            let mut tile_size = None;
            for (i, obus) in tiles.iter().enumerate() {
                let f = av1::decode(obus, max_pixels)?;
                let (tw, th) = *tile_size.get_or_insert((f.width, f.height));
                if (f.width, f.height) != (tw, th) {
                    return Err("grid tiles of different sizes".into());
                }
                if tw * columns < width || th * rows < height {
                    return Err("grid tiles too small to cover the image".into());
                }
                let tile = convert(&f, nclx);
                let (x0, y0) = ((i % columns) * tw, (i / columns) * th);
                for y in 0..th {
                    let dy = y0 + y;
                    if dy >= height || x0 >= width {
                        continue;
                    }
                    let w = tw.min(width - x0);
                    let from = &tile[y * tw * 4..(y * tw + w) * 4];
                    pixels[(dy * width + x0) * 4..(dy * width + x0 + w) * 4].copy_from_slice(from);
                }
            }
            Ok(Rgba {
                width,
                height,
                pixels,
            })
        }
    }
}

/// A sample as a fraction: luma (or a monochrome or alpha plane) from 0 to
/// 1, chroma from -0.5 to 0.5, by the bit depth and range.
fn unit(sample: u16, bits: u8, full: bool, chroma: bool) -> f32 {
    let max = f32::from((1u16 << bits) - 1);
    let scale = f32::from(1u16 << (bits - 8));
    let s = f32::from(sample);
    match (full, chroma) {
        (true, false) => s / max,
        (true, true) => (s - scale * 128.0) / max,
        (false, false) => (s - 16.0 * scale) / (219.0 * scale),
        (false, true) => (s - 128.0 * scale) / (224.0 * scale),
    }
}

fn byte(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// The frame as RGBA, opaque.
fn rgb(f: &Frame, nclx: Option<Nclx>) -> Vec<u8> {
    let (matrix, full) = nclx.map_or((f.matrix, f.full_range), |n| (n.matrix, n.full_range));
    let mut out = Vec::with_capacity(f.width * f.height * 4);
    let Some((across, down)) = f.chroma else {
        for &y in &f.planes[0] {
            let v = byte(unit(y, f.bits, full, false));
            out.extend_from_slice(&[v, v, v, 255]);
        }
        return out;
    };
    // Kr and Kb of the CICP matrix; BT.601 for unspecified and the rest.
    let (kr, kb) = match matrix {
        1 => (0.2126, 0.0722),
        4 => (0.30, 0.11),
        7 => (0.212, 0.087),
        9 | 10 => (0.2627, 0.0593),
        _ => (0.299, 0.114),
    };
    let kg = 1.0 - kr - kb;
    let cw = f.chroma_width();
    for row in 0..f.height {
        let crow = if down { row / 2 } else { row };
        for col in 0..f.width {
            let ccol = if across { col / 2 } else { col };
            let y = f.planes[0][row * f.width + col];
            let (u, v) = (f.planes[1][crow * cw + ccol], f.planes[2][crow * cw + ccol]);
            let (r, g, b) = match matrix {
                // Identity: the planes are G, B and R.
                0 => (
                    unit(v, f.bits, full, false),
                    unit(y, f.bits, full, false),
                    unit(u, f.bits, full, false),
                ),
                // YCgCo: U is Cg, V is Co.
                8 => {
                    let (y, cg, co) = (
                        unit(y, f.bits, full, false),
                        unit(u, f.bits, full, true),
                        unit(v, f.bits, full, true),
                    );
                    let t = y - cg;
                    (t + co, y + cg, t - co)
                }
                _ => {
                    let (y, cb, cr) = (
                        unit(y, f.bits, full, false),
                        unit(u, f.bits, full, true),
                        unit(v, f.bits, full, true),
                    );
                    let r = y + 2.0 * (1.0 - kr) * cr;
                    let b = y + 2.0 * (1.0 - kb) * cb;
                    (r, (y - kr * r - kb * b) / kg, b)
                }
            };
            out.extend_from_slice(&[byte(r), byte(g), byte(b), 255]);
        }
    }
    out
}

/// The frame's luma as gray RGBA: an alpha plane.
fn gray(f: &Frame) -> Vec<u8> {
    let mut out = Vec::with_capacity(f.width * f.height * 4);
    for &y in &f.planes[0] {
        let v = byte(unit(y, f.bits, f.full_range, false));
        out.extend_from_slice(&[v, v, v, 255]);
    }
    out
}

fn transform(image: Rgba, t: Transform) -> Rgba {
    let (w, h) = (image.width, image.height);
    let at = |x: usize, y: usize| &image.pixels[(y * w + x) * 4..(y * w + x) * 4 + 4];
    let mut pixels = Vec::with_capacity(image.pixels.len());
    let (nw, nh) = match t {
        Transform::Rotate(1 | 3) => (h, w),
        _ => (w, h),
    };
    for y in 0..nh {
        for x in 0..nw {
            // The source of each destination pixel.
            let (sx, sy) = match t {
                Transform::Rotate(1) => (w - 1 - y, x), // a quarter turn anticlockwise
                Transform::Rotate(2) => (w - 1 - x, h - 1 - y),
                Transform::Rotate(3) => (y, h - 1 - x),
                Transform::Mirror(0) => (w - 1 - x, y),
                Transform::Mirror(_) => (x, h - 1 - y),
                Transform::Rotate(_) => (x, y),
            };
            pixels.extend_from_slice(at(sx, sy));
        }
    }
    Rgba {
        width: nw,
        height: nh,
        pixels,
    }
}

fn png(image: &Rgba) -> Result<Vec<u8>, Error> {
    let opaque = image.pixels.as_chunks::<4>().0.iter().all(|p| p[3] == 255);
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, image.width as u32, image.height as u32);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let pixels: Vec<u8>;
    let data = if opaque {
        encoder.set_color(png::ColorType::Rgb);
        pixels = image
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        &pixels
    } else {
        encoder.set_color(png::ColorType::Rgba);
        &image.pixels
    };
    let mut writer = encoder.write_header().map_err(|e| format!("PNG: {e}"))?;
    writer
        .write_image_data(data)
        .map_err(|e| format!("PNG: {e}"))?;
    writer.finish().map_err(|e| format!("PNG: {e}"))?;
    Ok(out)
}
