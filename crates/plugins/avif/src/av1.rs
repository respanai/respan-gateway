//! One AV1 frame decoded with rav1d (the Rust port of dav1d), through its
//! dav1d API, the only one it has. This module is the crate's only unsafe
//! code: each call is as dav1d documents it, and every picture and buffer is
//! released before returning.
#![allow(unsafe_code)]

use std::mem::MaybeUninit;
use std::ptr::NonNull;

use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::{
    DAV1D_PIXEL_LAYOUT_I400, DAV1D_PIXEL_LAYOUT_I420, DAV1D_PIXEL_LAYOUT_I422,
};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{
    dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings, dav1d_get_picture,
    dav1d_open, dav1d_picture_unref, dav1d_send_data,
};

use crate::container::Error;

/// A decoded frame: its planes at full sample depth.
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub bits: u8,
    /// Chroma is subsampled across (`.0`) and down (`.1`); `None` for monochrome.
    pub chroma: Option<(bool, bool)>,
    /// Y, then U and V (empty for monochrome), row by row.
    pub planes: [Vec<u16>; 3],
    /// The sequence header's CICP matrix and range, unless the container says.
    pub matrix: u16,
    pub full_range: bool,
}

impl Frame {
    pub fn chroma_width(&self) -> usize {
        match self.chroma {
            Some((true, _)) => self.width.div_ceil(2),
            _ => self.width,
        }
    }
}

const EAGAIN: i32 = -(libc::EAGAIN);

/// Decodes the one frame `obus` codes; `max_pixels` bounds its size.
pub fn decode(obus: &[u8], max_pixels: u32) -> Result<Frame, Error> {
    if obus.is_empty() {
        return Err("an empty AV1 item".into());
    }
    let mut settings = MaybeUninit::<Dav1dSettings>::uninit();
    // SAFETY: dav1d_default_settings writes every field of the settings it is given.
    let mut settings = unsafe {
        dav1d_default_settings(NonNull::new_unchecked(settings.as_mut_ptr()));
        settings.assume_init()
    };
    settings.n_threads = 1;
    settings.max_frame_delay = 1;
    settings.frame_size_limit = max_pixels;
    // No logger: a damaged image is our error to report, not lines on stderr. The logger is a cookie
    // and a callback, both options of pointers, and its type is private, so it is cleared to zeros, None.
    // SAFETY: all-zero bytes are `None` for both fields.
    unsafe { std::ptr::write_bytes(std::ptr::addr_of_mut!(settings.logger), 0, 1) };
    let mut context: Option<Dav1dContext> = None;
    // SAFETY: both pointers are to live locals; a context it opens is closed below.
    let opened = unsafe {
        dav1d_open(
            Some(NonNull::from(&mut context)),
            Some(NonNull::from(&mut settings)),
        )
    };
    if opened.0 != 0 {
        return Err(format!("the AV1 decoder did not open ({})", opened.0));
    }
    let mut data = Dav1dData::default();
    // SAFETY: data_create allocates `obus.len()` bytes at `data.data`, or returns null.
    let buffer = unsafe { dav1d_data_create(Some(NonNull::from(&mut data)), obus.len()) };
    let result = if buffer.is_null() {
        Err("the AV1 decoder could not allocate the frame".to_string())
    } else {
        // SAFETY: `buffer` is `obus.len()` writable bytes of its own allocation.
        unsafe { std::ptr::copy_nonoverlapping(obus.as_ptr(), buffer, obus.len()) };
        picture(context, &mut data)
    };
    // SAFETY: unref of a sent or empty data is a no-op; the context is from dav1d_open.
    unsafe {
        dav1d_data_unref(Some(NonNull::from(&mut data)));
        dav1d_close(Some(NonNull::from(&mut context)));
    }
    result
}

fn picture(context: Option<Dav1dContext>, data: &mut Dav1dData) -> Result<Frame, Error> {
    for _ in 0..64 {
        if data.sz > 0 {
            // SAFETY: the context is open and `data` is the buffer it was given.
            let sent = unsafe { dav1d_send_data(context, Some(NonNull::from(&mut *data))) };
            if sent.0 != 0 && sent.0 != EAGAIN {
                return Err(format!("the AV1 data is invalid ({})", sent.0));
            }
        }
        let mut picture = Dav1dPicture::default();
        // SAFETY: the context is open; on success the picture is released after copying.
        let got = unsafe { dav1d_get_picture(context, Some(NonNull::from(&mut picture))) };
        if got.0 == 0 {
            let frame = copy(&picture);
            // SAFETY: the picture came from dav1d_get_picture and is released once.
            unsafe { dav1d_picture_unref(Some(NonNull::from(&mut picture))) };
            return frame;
        }
        if got.0 != EAGAIN {
            return Err(format!("the AV1 frame did not decode ({})", got.0));
        }
        if data.sz == 0 {
            break;
        }
    }
    Err("the AV1 item holds no frame".into())
}

fn copy(p: &Dav1dPicture) -> Result<Frame, Error> {
    let (width, height) = (
        usize::try_from(p.p.w).unwrap_or(0),
        usize::try_from(p.p.h).unwrap_or(0),
    );
    let bits = u8::try_from(p.p.bpc).unwrap_or(0);
    if width == 0 || height == 0 || !matches!(bits, 8 | 10 | 12) {
        return Err(format!("a {width}x{height} frame of {bits} bits"));
    }
    let chroma = match p.p.layout {
        DAV1D_PIXEL_LAYOUT_I400 => None,
        DAV1D_PIXEL_LAYOUT_I420 => Some((true, true)),
        DAV1D_PIXEL_LAYOUT_I422 => Some((true, false)),
        _ => Some((false, false)),
    };
    let (matrix, full_range) = match p.seq_hdr {
        // SAFETY: a decoded picture's sequence header lives as long as the picture.
        Some(h) => unsafe {
            let h = h.as_ref();
            (h.mtrx as u16, h.color_range != 0)
        },
        None => (2, false),
    };
    let wide = bits > 8;
    let plane = |i: usize, w: usize, h: usize| -> Result<Vec<u16>, Error> {
        let base = p.data[i]
            .ok_or("a frame without its planes")?
            .as_ptr()
            .cast::<u8>()
            .cast_const();
        let stride = p.stride[usize::from(i > 0)];
        let mut out = Vec::with_capacity(w * h);
        for y in 0..h {
            // SAFETY: a plane holds `h` rows `stride` bytes apart, each at least `w` samples.
            let row = unsafe { base.offset(stride * y as isize) };
            if wide {
                // SAFETY: as above, with 16-bit samples.
                let row = unsafe { std::slice::from_raw_parts(row.cast::<u16>(), w) };
                out.extend_from_slice(row);
            } else {
                // SAFETY: as above, with 8-bit samples.
                let row = unsafe { std::slice::from_raw_parts(row, w) };
                out.extend(row.iter().map(|&s| u16::from(s)));
            }
        }
        Ok(out)
    };
    let mut frame = Frame {
        width,
        height,
        bits,
        chroma,
        planes: [plane(0, width, height)?, Vec::new(), Vec::new()],
        matrix,
        full_range,
    };
    if let Some((across, down)) = chroma {
        let cw = if across { width.div_ceil(2) } else { width };
        let ch = if down { height.div_ceil(2) } else { height };
        frame.planes[1] = plane(1, cw, ch)?;
        frame.planes[2] = plane(2, cw, ch)?;
    }
    Ok(frame)
}
