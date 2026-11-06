//! Windows resources for the executables, written as a `.res` file by the build script
//! and passed to the MSVC linker: the application icon (drawn here, a small treemap) and
//! the version information shown in the file properties. No resource compiler needed.

use std::path::Path;

const LANG_EN_US: u16 = 0x0409;
const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;

/// Icon sizes; the largest is stored as PNG, the others as 32-bit DIBs.
const SIZES: [u32; 7] = [16, 20, 24, 32, 48, 64, 256];

pub struct Version<'a> {
    pub file_description: &'a str,
    pub internal_name: &'a str,
    pub original_filename: &'a str,
    pub product_name: &'a str,
    pub version: &'a str,
    pub copyright: &'a str,
}

/// Write `out` holding the icon (group 1) and the version resource.
pub fn write(out: &Path, v: &Version) -> std::io::Result<()> {
    let mut res = Vec::new();
    entry(&mut res, 0, 0, 0, &[]); // the empty first entry every .res file starts with
    let mut group = Vec::new();
    group.extend_from_slice(&[0, 0, 1, 0]);
    group.extend_from_slice(&(SIZES.len() as u16).to_le_bytes());
    for (i, &s) in SIZES.iter().enumerate() {
        let px = draw(s);
        let data = if s >= 256 { png(s, &px) } else { dib(s, &px) };
        let id = i as u16 + 1;
        entry(&mut res, RT_ICON, id, 0x1010, &data);
        let b = if s >= 256 { 0 } else { s as u8 };
        group.extend_from_slice(&[b, b, 0, 0]);
        group.extend_from_slice(&1u16.to_le_bytes());
        group.extend_from_slice(&32u16.to_le_bytes());
        group.extend_from_slice(&(data.len() as u32).to_le_bytes());
        group.extend_from_slice(&id.to_le_bytes());
    }
    entry(&mut res, RT_GROUP_ICON, 1, 0x1030, &group);
    entry(&mut res, RT_VERSION, 1, 0x0030, &version_info(v));
    std::fs::write(out, res)
}

/// One resource: header (numeric type and name), data, padding to 4 bytes.
fn entry(res: &mut Vec<u8>, ty: u16, name: u16, flags: u16, data: &[u8]) {
    res.extend_from_slice(&(data.len() as u32).to_le_bytes());
    res.extend_from_slice(&32u32.to_le_bytes());
    res.extend_from_slice(&[0xFF, 0xFF]);
    res.extend_from_slice(&ty.to_le_bytes());
    res.extend_from_slice(&[0xFF, 0xFF]);
    res.extend_from_slice(&name.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes()); // data version
    res.extend_from_slice(&flags.to_le_bytes());
    res.extend_from_slice(&(if ty == 0 { 0 } else { LANG_EN_US }).to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes()); // version
    res.extend_from_slice(&0u32.to_le_bytes()); // characteristics
    res.extend_from_slice(data);
    while !res.len().is_multiple_of(4) {
        res.push(0);
    }
}

// ------------------------------------------------------------------------------- icon

/// RGBA pixels, top row first: five tiles of a treemap with transparent gaps.
fn draw(size: u32) -> Vec<[u8; 4]> {
    // (x0, y0, x1, y1, colour) in a unit square
    const TILES: [(f32, f32, f32, f32, [u8; 3]); 5] = [
        (0.0, 0.0, 0.58, 1.0, [0xC2, 0x50, 0x1F]),
        (0.58, 0.0, 1.0, 0.52, [0x3D, 0x7D, 0xCA]),
        (0.58, 0.52, 0.82, 1.0, [0x4B, 0xA3, 0x5A]),
        (0.82, 0.52, 1.0, 0.76, [0xE3, 0xB3, 0x41]),
        (0.82, 0.76, 1.0, 1.0, [0x8E, 0x5C, 0xC2]),
    ];
    let n = size as f32;
    let margin = (n / 16.0).floor().max(0.0);
    let gap = (n / 24.0).round().max(1.0);
    let inner = n - 2.0 * margin;
    let mut px = vec![[0u8; 4]; (size * size) as usize];
    for &(x0, y0, x1, y1, c) in &TILES {
        let l = (margin + x0 * inner + if x0 > 0.0 { gap / 2.0 } else { 0.0 }).round() as i32;
        let r = (margin + x1 * inner - if x1 < 1.0 { gap / 2.0 } else { 0.0 }).round() as i32;
        let t = (margin + y0 * inner + if y0 > 0.0 { gap / 2.0 } else { 0.0 }).round() as i32;
        let b = (margin + y1 * inner - if y1 < 1.0 { gap / 2.0 } else { 0.0 }).round() as i32;
        for y in t..b {
            // Light from the top: a soft vertical shade inside each tile.
            let k = 1.12 - 0.24 * (y - t) as f32 / (b - t).max(1) as f32;
            let shade = |v: u8| (v as f32 * k).clamp(0.0, 255.0) as u8;
            for x in l..r {
                px[(y as u32 * size + x as u32) as usize] = [shade(c[0]), shade(c[1]), shade(c[2]), 255];
            }
        }
    }
    px
}

/// Icon image as BITMAPINFOHEADER + BGRA rows bottom-up + AND mask.
fn dib(size: u32, px: &[[u8; 4]]) -> Vec<u8> {
    let mut d = Vec::new();
    for v in [40u32, size, size * 2] {
        d.extend_from_slice(&v.to_le_bytes());
    }
    d.extend_from_slice(&1u16.to_le_bytes());
    d.extend_from_slice(&32u16.to_le_bytes());
    d.extend_from_slice(&[0u8; 24]);
    for y in (0..size).rev() {
        for x in 0..size {
            let [r, g, b, a] = px[(y * size + x) as usize];
            d.extend_from_slice(&[b, g, r, a]);
        }
    }
    let mask_row = (size as usize).div_ceil(32) * 4;
    d.resize(d.len() + mask_row * size as usize, 0);
    d
}

fn png(size: u32, px: &[[u8; 4]]) -> Vec<u8> {
    // Each row filtered "Up": flat tiles become runs of zeros, which deflate well.
    let stride = size as usize * 4;
    let mut raw = Vec::with_capacity((stride + 1) * size as usize);
    let mut prev = vec![0u8; stride];
    for y in 0..size as usize {
        let row: Vec<u8> = px[y * size as usize..(y + 1) * size as usize].iter().flatten().copied().collect();
        raw.push(2);
        raw.extend(row.iter().zip(&prev).map(|(a, b)| a.wrapping_sub(*b)));
        prev = row;
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&size.to_be_bytes());
    ihdr.extend_from_slice(&size.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(ty);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(b: &[u8]) -> u32 {
    let mut c = !0u32;
    for &x in b {
        c ^= x as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

/// LSB-first bit writer for deflate.
struct Bits {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl Bits {
    fn put(&mut self, v: u32, n: u32) {
        self.acc |= v << self.n;
        self.n += n;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }
    /// A Huffman code, which deflate stores most significant bit first.
    fn code(&mut self, code: u32, len: u32) {
        self.put(code.reverse_bits() >> (32 - len), len);
    }
    fn literal(&mut self, v: u32) {
        match v {
            0..=143 => self.code(0x30 + v, 8),
            144..=255 => self.code(0x190 + v - 144, 9),
            256..=279 => self.code(v - 256, 7),
            _ => self.code(0xC0 + v - 280, 8),
        }
    }
    /// A match of `len` (3..=258) bytes at distance 1.
    fn run(&mut self, len: u32) {
        const BASE: [u32; 29] =
            [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
        const EXTRA: [u32; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
        let i = BASE.iter().rposition(|&b| b <= len).unwrap_or(0);
        self.literal(257 + i as u32);
        self.put(len - BASE[i], EXTRA[i]);
        self.code(0, 5); // distance code 0: distance 1
    }
}

/// zlib stream: one fixed-Huffman block with run-length matches (distance 1).
fn zlib(data: &[u8]) -> Vec<u8> {
    let mut w = Bits { out: vec![0x78, 0x01], acc: 0, n: 0 };
    w.put(1, 1); // final block
    w.put(1, 2); // fixed Huffman codes
    let mut i = 0;
    while i < data.len() {
        let mut run = 0;
        if i > 0 {
            while run < 258 && i + run < data.len() && data[i + run] == data[i - 1] {
                run += 1;
            }
        }
        if run >= 3 {
            w.run(run as u32);
            i += run;
        } else {
            w.literal(data[i] as u32);
            i += 1;
        }
    }
    w.literal(256);
    if w.n > 0 {
        w.out.push(w.acc as u8);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    w.out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    w.out
}

// ---------------------------------------------------------------------------- version

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16().chain([0]).flat_map(|c| c.to_le_bytes()).collect()
}

fn pad4(b: &mut Vec<u8>) {
    while !b.len().is_multiple_of(4) {
        b.push(0);
    }
}

/// One node of the VS_VERSIONINFO tree: length, value length, type, key, value, children.
fn node(key: &str, value: &[u8], value_len: u16, text: bool, children: &[Vec<u8>]) -> Vec<u8> {
    let mut b = vec![0, 0];
    b.extend_from_slice(&value_len.to_le_bytes());
    b.extend_from_slice(&(text as u16).to_le_bytes());
    b.extend_from_slice(&utf16z(key));
    pad4(&mut b);
    b.extend_from_slice(value);
    for c in children {
        pad4(&mut b);
        b.extend_from_slice(c);
    }
    let len = b.len() as u16;
    b[0..2].copy_from_slice(&len.to_le_bytes());
    b
}

fn version_info(v: &Version) -> Vec<u8> {
    let parts: Vec<u16> = v.version.split('.').filter_map(|p| p.parse().ok()).chain([0, 0, 0, 0]).take(4).collect();
    let ms = (parts[0] as u32) << 16 | parts[1] as u32;
    let ls = (parts[2] as u32) << 16 | parts[3] as u32;
    let mut fixed = Vec::new();
    for x in [0xFEEF_04BDu32, 0x0001_0000, ms, ls, ms, ls, 0x3F, 0, 0x0004_0004, 1, 0, 0, 0] {
        fixed.extend_from_slice(&x.to_le_bytes());
    }
    let strings: Vec<Vec<u8>> = [
        ("FileDescription", v.file_description),
        ("FileVersion", v.version),
        ("InternalName", v.internal_name),
        ("LegalCopyright", v.copyright),
        ("OriginalFilename", v.original_filename),
        ("ProductName", v.product_name),
        ("ProductVersion", v.version),
    ]
    .iter()
    .map(|(k, s)| {
        let val = utf16z(s);
        node(k, &val, (val.len() / 2) as u16, true, &[])
    })
    .collect();
    let table = node("040904B0", &[], 0, true, &strings);
    let sfi = node("StringFileInfo", &[], 0, true, &[table]);
    let var = node("Translation", &[0x09, 0x04, 0xB0, 0x04], 4, false, &[]);
    let vfi = node("VarFileInfo", &[], 0, true, &[var]);
    node("VS_VERSION_INFO", &fixed, fixed.len() as u16, false, &[sfi, vfi])
}
