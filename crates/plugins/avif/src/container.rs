//! The AVIF container (HEIF over ISO-BMFF), as much of it as a still image
//! needs: the primary item, coded as one AV1 frame or a grid of them, its
//! alpha item, and the transforms and colour description it carries.
//! Sequences (`avis` with no still primary item) are not read.

use std::collections::HashMap;

/// Why a file is not an image this reads.
pub type Error = String;

/// Colour as `colr` (`nclx`) states it: the CICP matrix and range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nclx {
    pub matrix: u16,
    pub full_range: bool,
}

/// One coded picture: a single AV1 frame, or a grid of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Coded {
    Frame(Vec<u8>),
    Grid {
        rows: usize,
        columns: usize,
        width: usize,
        height: usize,
        /// Row by row, each one AV1 frame.
        tiles: Vec<Vec<u8>>,
    },
}

/// What turns the coded picture into the image, in the order to apply them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    /// Turns anticlockwise by this many quarter turns.
    Rotate(u8),
    /// Mirrors about the vertical axis (0, left and right swap) or the
    /// horizontal one (1, top and bottom swap).
    Mirror(u8),
}

/// A still image of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Still {
    pub color: Coded,
    pub alpha: Option<Coded>,
    pub nclx: Option<Nclx>,
    pub transforms: Vec<Transform>,
}

/// Whether `bytes` start as an AVIF file: an `ftyp` naming `avif` or `avis`.
pub fn is_avif(bytes: &[u8]) -> bool {
    let Some(size) = bytes.get(0..4) else {
        return false;
    };
    if bytes.get(4..8) != Some(b"ftyp") {
        return false;
    }
    let size = u32::from_be_bytes([size[0], size[1], size[2], size[3]]) as usize;
    let end = size.clamp(8, bytes.len());
    let brands = &bytes[8.min(end)..end];
    brands
        .as_chunks::<4>()
        .0
        .iter()
        .enumerate()
        .any(|(i, b)| i != 1 && (b == b"avif" || b == b"avis")) // [1] is the minor version
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.bytes.len());
        let end = end.ok_or_else(|| "the file ends inside a box".to_string())?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, Error> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, Error> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// An unsigned integer of `n` bytes (0, 1, 2, 4 or 8).
    fn uint(&mut self, n: u8) -> Result<u64, Error> {
        Ok(match n {
            0 => 0,
            1 => u64::from(self.u8()?),
            2 => u64::from(self.u16()?),
            4 => u64::from(self.u32()?),
            8 => {
                let b = self.take(8)?;
                u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
            }
            _ => return Err(format!("a field of {n} bytes")),
        })
    }

    /// A full box's version and flags.
    fn full(&mut self) -> Result<(u8, u32), Error> {
        let v = self.u32()?;
        Ok(((v >> 24) as u8, v & 0x00ff_ffff))
    }

    fn rest(&mut self) -> &'a [u8] {
        let out = &self.bytes[self.at..];
        self.at = self.bytes.len();
        out
    }

    fn is_empty(&self) -> bool {
        self.at >= self.bytes.len()
    }
}

/// Boxes, each its type and payload.
type Boxes<'a> = Vec<([u8; 4], &'a [u8])>;

/// The boxes of `bytes`.
fn boxes(bytes: &[u8]) -> Result<Boxes<'_>, Error> {
    let mut r = Reader::new(bytes);
    let mut out = Vec::new();
    while !r.is_empty() {
        let start = r.at;
        let size = r.u32()? as u64;
        let kind = r.take(4)?;
        let kind = [kind[0], kind[1], kind[2], kind[3]];
        let size = match size {
            0 => (bytes.len() - start) as u64,
            1 => r.uint(8)?,
            n => n,
        };
        let header = (r.at - start) as u64;
        let body = size
            .checked_sub(header)
            .ok_or_else(|| format!("box {} is smaller than its header", fourcc(&kind)))?;
        out.push((
            kind,
            r.take(usize::try_from(body).map_err(|_| "a box too large")?)?,
        ));
    }
    Ok(out)
}

fn fourcc(kind: &[u8]) -> String {
    String::from_utf8_lossy(kind).into_owned()
}

fn find<'a>(boxes: &[([u8; 4], &'a [u8])], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes.iter().find(|(k, _)| k == kind).map(|(_, b)| *b)
}

struct Extent {
    offset: u64,
    length: u64,
}

struct Location {
    in_idat: bool,
    base: u64,
    extents: Vec<Extent>,
}

#[derive(Clone)]
enum Property {
    Nclx(Nclx),
    Alpha,
    Transform(Transform),
    Other,
}

struct Meta<'a> {
    file: &'a [u8],
    idat: &'a [u8],
    primary: u32,
    types: HashMap<u32, [u8; 4]>,
    locations: HashMap<u32, Location>,
    /// (type, from, to) for every reference.
    references: Vec<([u8; 4], u32, Vec<u32>)>,
    properties: HashMap<u32, Vec<Property>>,
}

/// The still image `file` holds.
pub fn parse(file: &[u8]) -> Result<Still, Error> {
    let top = boxes(file)?;
    let meta = find(&top, b"meta").ok_or("no meta box: an image sequence, not a still image")?;
    let meta = read_meta(file, meta)?;
    let mut primary = meta.primary;
    // A tone-mapped image (`tmap`) is drawn from its base image, its first input.
    if meta.types.get(&primary) == Some(b"tmap") {
        primary = meta
            .inputs(primary)
            .first()
            .copied()
            .ok_or("the tone-mapped primary item has no base image")?;
    }
    let color = meta.coded(primary)?;
    let alpha = meta
        .references
        .iter()
        .filter(|(kind, _, to)| kind == b"auxl" && to.contains(&primary))
        .map(|(_, from, _)| *from)
        .find(|from| {
            (meta.properties.get(from))
                .is_some_and(|p| p.iter().any(|p| matches!(p, Property::Alpha)))
        })
        .map(|item| meta.coded(item))
        .transpose()?;
    let props = meta.properties.get(&primary).cloned().unwrap_or_default();
    let nclx = props.iter().find_map(|p| match p {
        Property::Nclx(n) => Some(*n),
        _ => None,
    });
    let transforms = props
        .iter()
        .filter_map(|p| match p {
            Property::Transform(t) => Some(*t),
            _ => None,
        })
        .collect();
    Ok(Still {
        color,
        alpha,
        nclx,
        transforms,
    })
}

fn read_meta<'a>(file: &'a [u8], meta: &'a [u8]) -> Result<Meta<'a>, Error> {
    let mut r = Reader::new(meta);
    r.full()?;
    let children = boxes(r.rest())?;
    let primary = {
        let mut r = Reader::new(find(&children, b"pitm").ok_or("no primary item")?);
        let (v, _) = r.full()?;
        if v == 0 {
            u32::from(r.u16()?)
        } else {
            r.u32()?
        }
    };
    let mut m = Meta {
        file,
        idat: find(&children, b"idat").unwrap_or_default(),
        primary,
        types: HashMap::new(),
        locations: HashMap::new(),
        references: Vec::new(),
        properties: HashMap::new(),
    };
    if let Some(iinf) = find(&children, b"iinf") {
        let mut r = Reader::new(iinf);
        let (v, _) = r.full()?;
        if v == 0 {
            r.u16()?;
        } else {
            r.u32()?;
        }
        for (kind, infe) in boxes(r.rest())? {
            if &kind != b"infe" {
                continue;
            }
            let mut r = Reader::new(infe);
            let (v, _) = r.full()?;
            if v < 2 {
                continue;
            }
            let id = if v == 2 {
                u32::from(r.u16()?)
            } else {
                r.u32()?
            };
            r.u16()?; // protection index
            let t = r.take(4)?;
            m.types.insert(id, [t[0], t[1], t[2], t[3]]);
        }
    }
    let iloc = find(&children, b"iloc").ok_or("no item locations")?;
    let mut r = Reader::new(iloc);
    let (v, _) = r.full()?;
    let sizes = r.u8()?;
    let (offset_size, length_size) = (sizes >> 4, sizes & 0xf);
    let sizes = r.u8()?;
    let base_size = sizes >> 4;
    let index_size = if v == 1 || v == 2 { sizes & 0xf } else { 0 };
    let count = if v < 2 { u32::from(r.u16()?) } else { r.u32()? };
    for _ in 0..count {
        let id = if v < 2 { u32::from(r.u16()?) } else { r.u32()? };
        let method = if v == 1 || v == 2 { r.u16()? & 0xf } else { 0 };
        r.u16()?; // data reference index
        let base = r.uint(base_size)?;
        let n = r.u16()?;
        let mut extents = Vec::with_capacity(usize::from(n));
        for _ in 0..n {
            r.uint(index_size)?;
            let offset = r.uint(offset_size)?;
            let length = r.uint(length_size)?;
            extents.push(Extent { offset, length });
        }
        if method <= 1 {
            m.locations.insert(
                id,
                Location {
                    in_idat: method == 1,
                    base,
                    extents,
                },
            );
        }
    }
    if let Some(iref) = find(&children, b"iref") {
        let mut r = Reader::new(iref);
        let (v, _) = r.full()?;
        for (kind, body) in boxes(r.rest())? {
            let mut r = Reader::new(body);
            let id = |r: &mut Reader| -> Result<u32, Error> {
                if v == 0 {
                    Ok(u32::from(r.u16()?))
                } else {
                    r.u32()
                }
            };
            let from = id(&mut r)?;
            let n = r.u16()?;
            let to = (0..n).map(|_| id(&mut r)).collect::<Result<Vec<_>, _>>()?;
            m.references.push((kind, from, to));
        }
    }
    if let Some(iprp) = find(&children, b"iprp") {
        let iprp = boxes(iprp)?;
        let ipco = boxes(find(&iprp, b"ipco").ok_or("no item properties")?)?;
        let ipco: Vec<Property> = ipco.iter().map(|(k, b)| property(k, b)).collect();
        for (kind, ipma) in &iprp {
            if kind != b"ipma" {
                continue;
            }
            let mut r = Reader::new(ipma);
            let (v, flags) = r.full()?;
            let n = r.u32()?;
            for _ in 0..n {
                let id = if v < 1 { u32::from(r.u16()?) } else { r.u32()? };
                let k = r.u8()?;
                let list = m.properties.entry(id).or_default();
                for _ in 0..k {
                    let index = if flags & 1 == 1 {
                        usize::from(r.u16()? & 0x7fff)
                    } else {
                        usize::from(r.u8()? & 0x7f)
                    };
                    if let Some(p) = index.checked_sub(1).and_then(|i| ipco.get(i)) {
                        list.push(p.clone());
                    }
                }
            }
        }
    }
    Ok(m)
}

fn property(kind: &[u8; 4], body: &[u8]) -> Property {
    let mut r = Reader::new(body);
    let read = |r: &mut Reader| -> Result<Property, Error> {
        Ok(match kind {
            b"colr" if r.take(4)? == b"nclx" => {
                r.u16()?; // primaries
                r.u16()?; // transfer
                let matrix = r.u16()?;
                let full_range = r.u8()? & 0x80 != 0;
                Property::Nclx(Nclx { matrix, full_range })
            }
            b"auxC" => {
                r.full()?;
                let urn = r.rest();
                let urn = urn.split(|&b| b == 0).next().unwrap_or_default();
                if urn == b"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha"
                    || urn == b"urn:mpeg:hevc:2015:auxid:1"
                {
                    Property::Alpha
                } else {
                    Property::Other
                }
            }
            b"irot" => Property::Transform(Transform::Rotate(r.u8()? & 3)),
            b"imir" => Property::Transform(Transform::Mirror(r.u8()? & 1)),
            _ => Property::Other,
        })
    };
    read(&mut r).unwrap_or(Property::Other)
}

impl Meta<'_> {
    /// The items `item` is drawn from (`dimg`), in order.
    fn inputs(&self, item: u32) -> Vec<u32> {
        self.references
            .iter()
            .filter(|(kind, from, _)| kind == b"dimg" && *from == item)
            .flat_map(|(_, _, to)| to.iter().copied())
            .collect()
    }

    fn data(&self, item: u32) -> Result<Vec<u8>, Error> {
        let loc = self
            .locations
            .get(&item)
            .ok_or_else(|| format!("item {item} has no data"))?;
        let source = if loc.in_idat { self.idat } else { self.file };
        let mut out = Vec::new();
        for e in &loc.extents {
            let start = loc
                .base
                .checked_add(e.offset)
                .and_then(|s| usize::try_from(s).ok());
            let start = start
                .filter(|&s| s <= source.len())
                .ok_or_else(|| format!("item {item} points past the file"))?;
            let end = if e.length == 0 {
                source.len()
            } else {
                usize::try_from(e.length)
                    .ok()
                    .and_then(|l| start.checked_add(l))
                    .filter(|&end| end <= source.len())
                    .ok_or_else(|| format!("item {item} runs past the file"))?
            };
            out.extend_from_slice(&source[start..end]);
        }
        Ok(out)
    }

    fn coded(&self, item: u32) -> Result<Coded, Error> {
        match self.types.get(&item) {
            Some(b"av01") => Ok(Coded::Frame(self.data(item)?)),
            Some(b"grid") => {
                let data = self.data(item)?;
                let mut r = Reader::new(&data);
                r.u8()?; // version
                let flags = r.u8()?;
                let rows = usize::from(r.u8()?) + 1;
                let columns = usize::from(r.u8()?) + 1;
                let size = if flags & 1 == 1 { 4 } else { 2 };
                let width = r.uint(size)? as usize;
                let height = r.uint(size)? as usize;
                let inputs = self.inputs(item);
                if inputs.len() != rows * columns {
                    return Err(format!(
                        "a {rows}x{columns} grid with {} tiles",
                        inputs.len()
                    ));
                }
                let tiles = inputs
                    .into_iter()
                    .map(|tile| match self.types.get(&tile) {
                        Some(b"av01") => self.data(tile),
                        other => Err(format!(
                            "a grid tile of type {}",
                            other.map_or("none".into(), |t| fourcc(t))
                        )),
                    })
                    .collect::<Result<_, _>>()?;
                Ok(Coded::Grid {
                    rows,
                    columns,
                    width,
                    height,
                    tiles,
                })
            }
            Some(other) => Err(format!("the image is coded as {}, not AV1", fourcc(other))),
            None => Err(format!("item {item} has no type")),
        }
    }
}
