//! # ft-treemap
//!
//! * [`squarify`] - the squarified layout (Bruls, Huizing, van Wijk): rows are grown
//!   along the shorter side while the worst aspect ratio keeps improving.
//! * [`Treemap::build`] - lays out a subtree of an [`ft_index::Index`] into a pixel
//!   rectangle. Large directories get a frame with a header band for their label; the
//!   recursion stops where a tile gets smaller than a pixel-area threshold, so the work is
//!   bounded by the window size, not by the number of files.
//! * [`Treemap::render`] - cushion shading (van Wijk & van de Wetering) into a pixel buffer
//!   plus an owner map (pixel -> node) for O(1) hit testing. Rendering is split into
//!   horizontal bands, one per thread, each writing only its own rows.

use std::thread;

use ft_index::{Index, NONE, nflags};

/// Lay out `sizes` (sorted descending, all > 0, in area units) in the rectangle
/// `x, y, w, h`. Pushes one `[x0, y0, x1, y1]` per size, in input order.
pub fn squarify(sizes: &[f64], mut x: f64, mut y: f64, mut w: f64, mut h: f64, out: &mut Vec<[f64; 4]>) {
    let mut i = 0;
    while i < sizes.len() {
        let short = w.min(h);
        if short <= 0.0 {
            out.extend(std::iter::repeat_n([x, y, x, y], sizes.len() - i));
            return;
        }
        let mut j = i;
        let mut sum = 0.0;
        let mut best = f64::INFINITY;
        while j < sizes.len() {
            let s = sum + sizes[j];
            let side = s / short;
            let worst = (sizes[i] / (side * side)).max(side * side / sizes[j]);
            if worst > best {
                break;
            }
            best = worst;
            sum = s;
            j += 1;
        }
        if w >= h {
            let cw = sum / h;
            let mut cy = y;
            for &s in &sizes[i..j] {
                let hh = s / cw;
                out.push([x, cy, x + cw, cy + hh]);
                cy += hh;
            }
            x += cw;
            w -= cw;
        } else {
            let rh = sum / w;
            let mut cx = x;
            for &s in &sizes[i..j] {
                let ww = s / rh;
                out.push([cx, y, cx + ww, y + rh]);
                cx += ww;
            }
            y += rh;
            h -= rh;
        }
        i = j;
    }
}

/// A drawn leaf: a file, or a directory too small to subdivide.
#[derive(Clone, Copy, Debug)]
pub struct Tile {
    pub node: u32,
    pub rect: [f32; 4],
    pub depth: u16,
    /// Cushion surface coefficients: z = a,x² + b,x + c,y² + d,y.
    surf: [f32; 4],
}

/// A subdivided directory. With `header`, its top band (and 1 px border) is drawn in the
/// frame colour and holds the directory's label.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub node: u32,
    pub rect: [f32; 4],
    pub depth: u16,
    pub header: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Do not subdivide tiles smaller than this many square pixels.
    pub min_area: f64,
    /// Cushion height of the top level and its decay per level.
    pub height: f32,
    pub falloff: f32,
    /// Height of a directory's label band in pixels; 0 disables frames.
    pub header: f32,
}

impl Default for Options {
    fn default() -> Self {
        Options { min_area: 6.0, height: 0.5, falloff: 0.75, header: 0.0 }
    }
}

/// Colours for the frame parts, `0x00RRGGBB`.
#[derive(Clone, Copy, Debug)]
pub struct Style {
    pub background: u32,
    pub border: u32,
    /// Header band colour by depth (cycled).
    pub header: [u32; 3],
}

impl Default for Style {
    fn default() -> Self {
        Style { background: 0x202020, border: 0x101010, header: [0x3a3a3a, 0x333333, 0x2c2c2c] }
    }
}

#[derive(Default)]
pub struct Treemap {
    pub width: u32,
    pub height: u32,
    pub header: f32,
    pub tiles: Vec<Tile>,
    pub frames: Vec<Frame>,
}

fn add_ridge(x0: f32, x1: f32, h: f32, s: &mut [f32; 2]) {
    let w = x1 - x0;
    if w > 0.0 {
        s[1] += 4.0 * h * (x1 + x0) / w;
        s[0] -= 4.0 * h / w;
    }
}

impl Treemap {
    /// Lay out the subtree of `root` into `width × height` pixels, area ∝ `weight[node]`.
    /// Secondary hard links get no area (they are not part of their parent's total).
    pub fn build(ix: &Index, weight: &[u64], root: u32, width: u32, height: u32, o: Options) -> Treemap {
        let mut tm = Treemap { width, height, header: o.header, tiles: Vec::new(), frames: Vec::new() };
        if width == 0 || height == 0 || ix.is_empty() {
            return tm;
        }
        let mut kids = Vec::new();
        tm.place(ix, weight, root, [0.0, 0.0, width as f64, height as f64], 0, [0.0; 4], &o, &mut kids);
        tm
    }

    #[allow(clippy::too_many_arguments)]
    fn place(
        &mut self,
        ix: &Index,
        weight: &[u64],
        node: u32,
        r: [f64; 4],
        depth: u16,
        surf: [f32; 4],
        o: &Options,
        kids: &mut Vec<(u64, u32)>,
    ) {
        let h = o.height * o.falloff.powi(depth as i32);
        let (mut sx, mut sy) = ([surf[0], surf[1]], [surf[2], surf[3]]);
        add_ridge(r[0] as f32, r[2] as f32, h, &mut sx);
        add_ridge(r[1] as f32, r[3] as f32, h, &mut sy);
        let surf = [sx[0], sx[1], sy[0], sy[1]];
        let rf = [r[0] as f32, r[1] as f32, r[2] as f32, r[3] as f32];
        let (w, hgt) = (r[2] - r[0], r[3] - r[1]);
        let total = weight[node as usize];
        if !ix.is_dir(node) || w * hgt < o.min_area || total == 0 {
            self.tiles.push(Tile { node, rect: rf, depth, surf });
            return;
        }
        // Frame with a label band when there is room for it and for content below.
        let hb = o.header as f64;
        let header = hb > 0.0 && hgt >= hb * 2.5 && w >= hb * 3.0;
        let inner = if header { [r[0] + 1.0, r[1] + hb, r[2] - 1.0, r[3] - 1.0] } else { r };
        self.frames.push(Frame { node, rect: rf, depth, header });

        let base = kids.len();
        kids.extend(
            ix.children(node)
                .filter(|&c| ix.flags[c as usize] & nflags::DUP_LINK == 0 && weight[c as usize] > 0)
                .map(|c| (weight[c as usize], c)),
        );
        kids[base..].sort_unstable_by_key(|k| std::cmp::Reverse(k.0));
        let sum: u64 = kids[base..].iter().map(|k| k.0).sum();
        let (iw, ih) = (inner[2] - inner[0], inner[3] - inner[1]);
        if sum == 0 || iw <= 0.0 || ih <= 0.0 {
            kids.truncate(base);
            self.tiles.push(Tile { node, rect: rf, depth, surf });
            return;
        }
        let scale = iw * ih / sum as f64;
        let sizes: Vec<f64> = kids[base..].iter().map(|k| k.0 as f64 * scale).collect();
        let nodes: Vec<u32> = kids[base..].iter().map(|k| k.1).collect();
        kids.truncate(base);
        let mut rects = Vec::with_capacity(sizes.len());
        squarify(&sizes, inner[0], inner[1], iw, ih, &mut rects);
        // Children too small to see merge into one remainder tile of the directory.
        let mut rest: Option<[f64; 4]> = None;
        for (&c, cr) in nodes.iter().zip(&rects) {
            if (cr[2] - cr[0]) * (cr[3] - cr[1]) < 1.0 {
                rest = Some(match rest {
                    None => *cr,
                    Some(a) => [a[0].min(cr[0]), a[1].min(cr[1]), a[2].max(cr[2]), a[3].max(cr[3])],
                });
                continue;
            }
            self.place(ix, weight, c, *cr, depth + 1, surf, o, kids);
        }
        if let Some(a) = rest {
            let rf = [a[0] as f32, a[1] as f32, a[2] as f32, a[3] as f32];
            self.tiles.push(Tile { node, rect: rf, depth: depth + 1, surf });
        }
    }

    /// Render into `pixels` (`0x00RRGGBB`, row-major, top-down) and record the owning
    /// node of each pixel in `owner`. Tiles for which `highlight` is false are dimmed.
    pub fn render(
        &self,
        color: &(dyn Fn(u32) -> [u8; 3] + Sync),
        highlight: Option<&(dyn Fn(u32) -> bool + Sync)>,
        style: &Style,
        pixels: &mut [u32],
        owner: &mut [u32],
    ) {
        let (w, h) = (self.width as usize, self.height as usize);
        if w == 0 || h == 0 {
            return;
        }
        assert!(pixels.len() >= w * h && owner.len() >= w * h);
        let threads = thread::available_parallelism().map_or(1, |n| n.get()).min(16).min(h.div_ceil(32)).max(1);
        let band = h.div_ceil(threads);
        thread::scope(|s| {
            for (i, (px, ow)) in pixels[..w * h].chunks_mut(band * w).zip(owner[..w * h].chunks_mut(band * w)).enumerate() {
                let y0 = i * band;
                s.spawn(move || self.render_band(color, highlight, style, y0, px, ow));
            }
        });
    }

    fn render_band(
        &self,
        color: &(dyn Fn(u32) -> [u8; 3] + Sync),
        highlight: Option<&(dyn Fn(u32) -> bool + Sync)>,
        style: &Style,
        y0: usize,
        px: &mut [u32],
        ow: &mut [u32],
    ) {
        let w = self.width as usize;
        let rows = px.len() / w;
        let y1 = y0 + rows;
        px.fill(style.background);
        ow.fill(NONE);
        let clip = |r: &[f32; 4]| -> Option<(usize, usize, usize, usize)> {
            let x0 = (r[0].round().max(0.0) as usize).min(w);
            let x1 = (r[2].round().max(0.0) as usize).min(w);
            let ry0 = (r[1].round().max(0.0) as usize).max(y0);
            let ry1 = (r[3].round().max(0.0) as usize).min(y1);
            (x0 < x1 && ry0 < ry1).then_some((x0, x1, ry0, ry1))
        };
        // Frames first (pre-order: parents before children), then the leaves on top.
        for f in self.frames.iter().filter(|f| f.header) {
            let Some((x0, x1, fy0, fy1)) = clip(&f.rect) else { continue };
            let head_end = (f.rect[1] + self.header).round() as usize;
            let hc = style.header[f.depth as usize % style.header.len()];
            for y in fy0..fy1 {
                let row = (y - y0) * w;
                let c = if y < head_end { hc } else { style.border };
                px[row + x0..row + x1].fill(c);
                ow[row + x0..row + x1].fill(f.node);
            }
        }
        const L: [f32; 3] = [-0.0976, -0.1952, 0.9759];
        const AMBIENT: f32 = 0.35;
        const DIFFUSE: f32 = 0.65;
        for t in &self.tiles {
            let Some((x0, x1, ty0, ty1)) = clip(&t.rect) else { continue };
            let [r, g, b] = color(t.node);
            let dim = highlight.is_some_and(|f| !f(t.node));
            let k0 = if dim { 0.28 } else { 1.0 };
            let s = t.surf;
            for y in ty0..ty1 {
                let ny = -(2.0 * s[2] * (y as f32 + 0.5) + s[3]);
                let row = (y - y0) * w;
                for x in x0..x1 {
                    let nx = -(2.0 * s[0] * (x as f32 + 0.5) + s[1]);
                    let cos = (nx * L[0] + ny * L[1] + L[2]) / (nx * nx + ny * ny + 1.0).sqrt();
                    let k = (AMBIENT + DIFFUSE * cos.max(0.0)) * k0;
                    let c = |v: u8| ((v as f32 * k).min(255.0)) as u32;
                    px[row + x] = c(r) << 16 | c(g) << 8 | c(b);
                    ow[row + x] = t.node;
                }
            }
        }
    }

    /// The laid-out rectangle of `node`, if it is visible.
    pub fn rect_of(&self, node: u32) -> Option<[f32; 4]> {
        self.frames
            .iter()
            .map(|f| (f.node, f.rect))
            .chain(self.tiles.iter().map(|t| (t.node, t.rect)))
            .find(|&(n, _)| n == node)
            .map(|(_, r)| r)
    }

    /// Frames that carry a label band.
    pub fn headers(&self) -> impl Iterator<Item = &Frame> {
        self.frames.iter().filter(|f| f.header)
    }
}

/// Distinct colours for the largest file types (by rank), vivid hues.
pub const TYPE_PALETTE: [[u8; 3]; 24] = [
    [0xE6, 0xD2, 0x2E],
    [0x2E, 0xB8, 0x3B],
    [0x9B, 0x4D, 0xE8],
    [0xE8, 0x3B, 0x4C],
    [0x2F, 0x8C, 0xF0],
    [0xF0, 0x6E, 0xD6],
    [0x24, 0xD2, 0xC0],
    [0xF2, 0x8A, 0x1E],
    [0x7B, 0x8C, 0xFF],
    [0x86, 0xE0, 0x3A],
    [0xC2, 0x2E, 0x8E],
    [0x3C, 0xC8, 0xF0],
    [0xB0, 0x7A, 0x3C],
    [0xFF, 0xB0, 0xC0],
    [0x6A, 0xA8, 0x5E],
    [0xD8, 0xA0, 0xFF],
    [0xA8, 0x2A, 0x2A],
    [0x2A, 0x5C, 0xA8],
    [0xC8, 0xC8, 0x64],
    [0x64, 0xC8, 0x96],
    [0xE0, 0x80, 0x80],
    [0x80, 0x80, 0xE0],
    [0x9A, 0x6A, 0x2A],
    [0x4A, 0x9A, 0x9A],
];

/// Colour of a file type by its size rank: the top ranks get the palette, the long tail a
/// muted colour derived from the rank, files without an extension grey.
pub fn type_color(rank: u16, no_ext: bool) -> [u8; 3] {
    if no_ext {
        return [0x8C, 0x8C, 0x96];
    }
    match TYPE_PALETTE.get(rank as usize) {
        Some(&c) => c,
        None => {
            let h = (rank as u32).wrapping_mul(2_654_435_761);
            [0x50 + (h & 0x3F) as u8, 0x50 + (h >> 8 & 0x3F) as u8, 0x50 + (h >> 16 & 0x3F) as u8]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ft_ntfs::{NameEntry, Rec, rflags};

    fn overlap(a: &[f64; 4], b: &[f64; 4]) -> f64 {
        let w = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
        let h = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
        w * h
    }

    #[test]
    fn squarify_conserves_area_and_tiles_without_overlap() {
        let mut sizes = vec![6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0];
        let total: f64 = sizes.iter().sum();
        let (w, h) = (6.0, 4.0);
        let scale = w * h / total;
        sizes.iter_mut().for_each(|s| *s *= scale);
        let mut out = Vec::new();
        squarify(&sizes, 0.0, 0.0, w, h, &mut out);
        assert_eq!(out.len(), sizes.len());
        for (r, &s) in out.iter().zip(&sizes) {
            let a = (r[2] - r[0]) * (r[3] - r[1]);
            assert!((a - s).abs() < 1e-9, "area {a} vs {s}");
            assert!(r[0] >= -1e-9 && r[1] >= -1e-9 && r[2] <= w + 1e-9 && r[3] <= h + 1e-9);
        }
        for i in 0..out.len() {
            for j in i + 1..out.len() {
                assert!(overlap(&out[i], &out[j]) < 1e-9);
            }
        }
    }

    #[test]
    fn squarify_keeps_aspect_ratios_reasonable() {
        let sizes: Vec<f64> = (1..=50).rev().map(|i| i as f64 * 100.0 / 1275.0 * 100.0).collect();
        let mut out = Vec::new();
        squarify(&sizes, 0.0, 0.0, 100.0, 100.0, &mut out);
        let worst = out
            .iter()
            .map(|r| {
                let (w, h) = (r[2] - r[0], r[3] - r[1]);
                w.max(h) / w.min(h)
            })
            .fold(0.0, f64::max);
        assert!(worst < 4.0, "worst aspect ratio {worst}");
    }

    fn index() -> Index {
        let mut recs = vec![Rec::default(); 40];
        let mut names = Vec::new();
        let mut arena = Vec::new();
        let mut add = |r: u32, parent: u32, name: &str, dir: bool, size: u64, recs: &mut Vec<Rec>| {
            let flags = rflags::IN_USE | if dir { rflags::DIR } else { 0 };
            recs[r as usize] = Rec { size, alloc: size, flags, seq: 1, ..Default::default() };
            names.push(NameEntry { record: r, parent, off: arena.len() as u32, len: name.len() as u16, parent_seq: 1 });
            arena.extend_from_slice(name.as_bytes());
        };
        add(5, 5, ".", true, 0, &mut recs);
        add(16, 5, "big", true, 0, &mut recs);
        add(17, 16, "a.mkv", false, 6000, &mut recs);
        add(18, 16, "b.mkv", false, 2000, &mut recs);
        add(19, 5, "c.txt", false, 2000, &mut recs);
        Index::build(&recs, &names, &arena, "C:").unwrap().0
    }

    fn render(tm: &Treemap, hl: Option<&(dyn Fn(u32) -> bool + Sync)>) -> (Vec<u32>, Vec<u32>) {
        let n = (tm.width * tm.height) as usize;
        let (mut px, mut own) = (vec![0u32; n], vec![0u32; n]);
        tm.render(&|_| [200, 100, 50], hl, &Style::default(), &mut px, &mut own);
        (px, own)
    }

    #[test]
    fn layout_areas_follow_sizes_and_pixels_are_owned() {
        let ix = index();
        let tm = Treemap::build(&ix, &ix.size, 0, 100, 100, Options::default());
        let area = |n: &str| {
            let node = ix.find(n).unwrap();
            let r = tm.rect_of(node).unwrap();
            ((r[2] - r[0]) * (r[3] - r[1])) as f64
        };
        assert!((area("big") - 8000.0).abs() < 1.0);
        assert!((area(r"big\a.mkv") - 6000.0).abs() < 1.0);
        assert!((area("c.txt") - 2000.0).abs() < 1.0);
        let (px, own) = render(&tm, None);
        assert!(own.iter().all(|&o| o != NONE), "every pixel belongs to a file");
        let a = ix.find(r"big\a.mkv").unwrap();
        let owned = own.iter().filter(|&&o| o == a).count();
        assert!((owned as i64 - 6000).abs() < 150, "{owned}");
        let lit: std::collections::HashSet<u32> = px.iter().copied().collect();
        assert!(lit.len() > 50, "cushions are shaded, not flat");
    }

    #[test]
    fn headers_frame_directories_and_belong_to_them() {
        let ix = index();
        let o = Options { header: 12.0, ..Default::default() };
        let tm = Treemap::build(&ix, &ix.size, 0, 300, 200, o);
        let big = ix.find("big").unwrap();
        let f = tm.headers().find(|f| f.node == big).expect("big has a header");
        let (_, own) = render(&tm, None);
        let (x, y) = ((f.rect[0] + 5.0) as usize, (f.rect[1] + 3.0) as usize);
        assert_eq!(own[y * 300 + x], big, "header pixels select the directory");
        // children sit inside the frame, below the header
        let a = tm.rect_of(ix.find(r"big\a.mkv").unwrap()).unwrap();
        assert!(a[1] >= f.rect[1] + 12.0 - 0.01 && a[0] >= f.rect[0] && a[2] <= f.rect[2]);
    }

    #[test]
    fn highlight_dims_the_rest() {
        let ix = index();
        let tm = Treemap::build(&ix, &ix.size, 0, 100, 100, Options::default());
        let c = ix.find("c.txt").unwrap();
        let (px_all, own) = render(&tm, None);
        let only_c = move |n: u32| n == c;
        let (px_hl, _) = render(&tm, Some(&only_c));
        let lum = |p: u32| (p >> 16 & 255) + (p >> 8 & 255) + (p & 255);
        for i in 0..own.len() {
            if own[i] == c {
                assert_eq!(px_hl[i], px_all[i]);
            } else {
                assert!(lum(px_hl[i]) < lum(px_all[i]));
            }
        }
    }

    #[test]
    fn work_is_bounded_by_pixels_not_files() {
        let n = 100_000u32;
        let mut recs = vec![Rec::default(); n as usize + 20];
        recs[5] = Rec { flags: rflags::IN_USE | rflags::DIR, seq: 1, ..Default::default() };
        let mut names = vec![NameEntry { record: 5, parent: 5, off: 0, len: 1, parent_seq: 1 }];
        let arena = b".f".to_vec();
        for i in 0..n {
            let r = 16 + i;
            recs[r as usize] = Rec { size: 10, flags: rflags::IN_USE, seq: 1, ..Default::default() };
            names.push(NameEntry { record: r, parent: 5, off: 1, len: 1, parent_seq: 1 });
        }
        let ix = Index::build(&recs, &names, &arena, "C:").unwrap().0;
        let tm = Treemap::build(&ix, &ix.size, 0, 50, 50, Options::default());
        assert!(tm.tiles.len() < 5_000, "{} tiles", tm.tiles.len());
    }
}
