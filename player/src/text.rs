//! Text objects (docs/text.md): a piece's own text plus the global layer, drawn into one transparent
//! layer covering just the box the text touches, that's then laid over each of the piece's frames.
//!
//! The geometry follows the spec to the letter, and lays text out as Chrome does on Windows (its line
//! metrics, HarfBuzz shaping), so the studio's preview matches the cooler. Drawing a layer doesn't depend
//! on preparing frames, so it can also run while playing.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use fontdue::{Font, FontSettings, Metrics};
use image::{RgbImage, Rgba, RgbaImage};
use rustybuzz::{Face, UnicodeBuffer, ttf_parser};

use crate::art::{HEIGHT, WIDTH};
use crate::playlist::{self, Align, Rect, Text, Valign};

/// Used when a text's own font is missing or unreadable.
pub const DEFAULT_FONT: &str = r"C:\Windows\Fonts\segoeui.ttf";
/// Bump when drawing changes, so text prepared by an older player is drawn again.
pub const VERSION: u32 = 2;
/// Anything bigger is a typo, and would only make huge bitmaps.
const MAX_SIZE: f32 = 2000.0;
const MAX_OUTLINE: f32 = 500.0;
/// fontdue turns curves into straight lines finely enough for text up to about this size; the default
/// (40) shows corners on big text.
const CURVES_FOR: f32 = 300.0;
/// No covered pixel nearby.
const NONE: u32 = u32::MAX;
/// Half covered, where crisp text turns on. fontdue rounds coverage down to bytes, so exactly half is
/// 127; the studio turns on at 128 of 255, Chrome's half.
const HALF: f32 = 127.0 / 255.0;

/// Text ready to lay over frames: RGBA (not premultiplied) in upright panel pixels, over the box the
/// text can touch (each text's mask, cut to its clip or the panel), not the whole panel.
pub struct Layer {
    /// Empty (0x0) while there's no text, so a piece without any costs nothing.
    image: RgbaImage,
    /// Where the image's top-left sits on the panel.
    left: u32,
    top: u32,
    bounds: Option<Bounds>,
}

/// Where a layer isn't transparent: left and top inclusive, right and bottom exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

impl Bounds {
    fn union(self, other: Option<Bounds>) -> Bounds {
        let Some(o) = other else { return self };
        Bounds {
            left: self.left.min(o.left),
            top: self.top.min(o.top),
            right: self.right.max(o.right),
            bottom: self.bottom.max(o.bottom),
        }
    }
}

impl Layer {
    /// Draws the texts in order, each over the ones before. Any it can't draw (an unknown type, no
    /// usable font) are skipped.
    pub fn render<'a>(texts: impl IntoIterator<Item = &'a Text>) -> Layer {
        let mut layer = Layer { image: RgbaImage::new(0, 0), left: 0, top: 0, bounds: None };
        for w in texts {
            if w.kind == "text" {
                layer.draw(w);
            }
        }
        layer
    }

    pub fn bounds(&self) -> Option<Bounds> {
        self.bounds
    }

    /// The layer's pixels, covering the box from `origin` (0x0 while there's no text).
    pub fn image(&self) -> &RgbaImage {
        &self.image
    }

    /// Where `image`'s top-left sits on the panel.
    pub fn origin(&self) -> (u32, u32) {
        (self.left, self.top)
    }

    /// The origin and the pixels, for laying the layer over a buffer.
    pub fn into_parts(self) -> (u32, u32, RgbaImage) {
        (self.left, self.top, self.image)
    }

    pub fn is_empty(&self) -> bool {
        self.bounds.is_none()
    }

    /// Lays the text over a 1600x720 frame, touching only the part where there is some.
    pub fn draw_onto(&self, frame: &mut RgbImage) {
        self.draw_onto_turned(frame, false);
    }

    /// draw_onto, for a frame that's already been turned 180 degrees (`turned`): the text lands where it
    /// would have before the turn, which saves turning the frame back and forth.
    pub fn draw_onto_turned(&self, frame: &mut RgbImage, turned: bool) {
        let Some(b) = self.bounds else { return };
        if frame.dimensions() != (WIDTH, HEIGHT) {
            return;
        }
        let last = (WIDTH * HEIGHT) as usize - 1;
        let stride = self.image.width() as usize;
        let (text, frame): (&[u8], &mut [u8]) = (&self.image, frame);
        for y in b.top as usize..b.bottom as usize {
            for x in b.left as usize..b.right as usize {
                let i = (y - self.top as usize) * stride + (x - self.left as usize);
                let j = y * WIDTH as usize + x;
                let j = if turned { last - j } else { j };
                let (src, dst) = (&text[i * 4..i * 4 + 4], &mut frame[j * 3..j * 3 + 3]);
                let a = src[3] as u32;
                if a == 0 {
                    continue;
                }
                for c in 0..3 {
                    dst[c] = ((src[c] as u32 * a + dst[c] as u32 * (255 - a) + 127) / 255) as u8;
                }
            }
        }
    }

    fn draw(&mut self, w: &Text) {
        if !(w.size > 0.0) || !(w.opacity > 0.0) {
            return;
        }
        let Some(font) = font(&w.font).or_else(|| font(Path::new(DEFAULT_FONT))) else { return };
        let size = w.size.min(MAX_SIZE);
        let outline = if w.outline.width > 0.0 { w.outline.width.min(MAX_OUTLINE) } else { 0.0 };
        // how far beyond the glyphs the outline can reach
        let pad = if outline > 0.0 { outline.ceil() as i32 + 1 } else { 0 };
        let Some(mut mask) = coverage(&font.glyphs, size, &layout(&font, w, size), pad, w.clip) else { return };
        let mut ring = (outline > 0.0).then(|| dilate(&mask, outline));
        if w.crisp {
            // each mask on its own, so the fill and outline meet with a hard edge, like the studio's
            threshold(&mut mask.alpha);
            ring.iter_mut().for_each(|r| threshold(r));
        }
        let fill = rgb(&w.color, [255, 255, 255]);
        let edge = rgb(&w.outline.color, [0, 0, 0]);
        self.paint(&mask, fill, ring.as_deref().map(|r| (r, edge)), w.opacity.min(1.0), w.clip);
    }

    /// Paints the fill over the outline (if any), then the two as one over the layer at `opacity`, so
    /// the outline doesn't show through faint text. `clip` is where it may draw, in panel pixels (the
    /// panel's edges cut it anyway).
    fn paint(&mut self, mask: &Mask, fill: [u8; 3], ring: Option<(&[f32], [u8; 3])>, opacity: f32, clip: Option<Rect>) {
        let (cl, ct, cr, cb) = visible(clip);
        let (x0, x1) = (mask.left.max(cl), (mask.left + mask.width as i32).min(cr));
        let (y0, y1) = (mask.top.max(ct), (mask.top + mask.height as i32).min(cb));
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        self.reserve(x0, y0, x1, y1);
        let mut drawn: Option<Bounds> = None;
        for y in y0..y1 {
            for x in x0..x1 {
                let i = (y - mask.top) as usize * mask.width + (x - mask.left) as usize;
                let body = mask.alpha[i];
                let (edge, edge_color) = ring.map_or((0.0, [0; 3]), |(r, color)| (r[i] * (1.0 - body), color));
                let alpha = body + edge;
                if alpha <= 0.0 {
                    continue;
                }
                let color = [0, 1, 2].map(|c| (fill[c] as f32 * body + edge_color[c] as f32 * edge) / alpha);
                let px = self.image.get_pixel_mut(x as u32 - self.left, y as u32 - self.top);
                over(px, color, alpha * opacity);
                if px[3] > 0 {
                    let (x, y) = (x as u32, y as u32);
                    drawn = Some(Bounds { left: x, top: y, right: x + 1, bottom: y + 1 }.union(drawn));
                }
            }
        }
        if let Some(drawn) = drawn {
            self.bounds = Some(drawn.union(self.bounds));
        }
    }

    /// Makes the image cover the box (panel pixels, inside the panel), keeping what's drawn: the first
    /// text's box, then the union with each later one's.
    fn reserve(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        if self.image.width() == 0 {
            self.image = RgbaImage::new((x1 - x0) as u32, (y1 - y0) as u32);
            (self.left, self.top) = (x0 as u32, y0 as u32);
            return;
        }
        let (l, t) = (self.left as i32, self.top as i32);
        let (r, b) = (l + self.image.width() as i32, t + self.image.height() as i32);
        if x0 >= l && y0 >= t && x1 <= r && y1 <= b {
            return;
        }
        let (nl, nt, nr, nb) = (l.min(x0), t.min(y0), r.max(x1), b.max(y1));
        let mut bigger = RgbaImage::new((nr - nl) as u32, (nb - nt) as u32);
        image::imageops::replace(&mut bigger, &self.image, (l - nl) as i64, (t - nt) as i64);
        self.image = bigger;
        (self.left, self.top) = (nl as u32, nt as u32);
    }
}

/// Where text may show: the clip cut to the panel, or the panel. Left and top inclusive, right and
/// bottom exclusive; an empty box when nothing can show.
fn visible(clip: Option<Rect>) -> (i32, i32, i32, i32) {
    let (w, h) = (WIDTH as i64, HEIGHT as i64);
    let [l, t, r, b] = clip.unwrap_or([0, 0, w, h]);
    (l.clamp(0, w) as i32, t.clamp(0, h) as i32, r.clamp(0, w) as i32, b.clamp(0, h) as i32)
}

/// Paints `color` at `alpha` over a pixel (source-over, not premultiplied).
fn over(px: &mut Rgba<u8>, color: [f32; 3], alpha: f32) {
    let below = px[3] as f32 / 255.0 * (1.0 - alpha);
    let total = alpha + below;
    for c in 0..3 {
        px[c] = ((color[c] * alpha + px[c] as f32 * below) / total).round() as u8;
    }
    px[3] = (total * 255.0).round() as u8;
}

/// `#rrggbb`, or `fallback` if it isn't one.
fn rgb(s: &str, fallback: [u8; 3]) -> [u8; 3] {
    let Some(hex) = s.strip_prefix('#').filter(|h| h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit())) else {
        return fallback;
    };
    [0, 2, 4].map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
}

/// For crisp text: fully on or fully off.
fn threshold(alpha: &mut [f32]) {
    for a in alpha {
        *a = if *a >= HALF { 1.0 } else { 0.0 };
    }
}

/// A glyph and where its origin goes: on its line's baseline, in panel pixels.
struct Placed {
    index: u16,
    x: f32,
    y: f32,
}

/// The spec's geometry: lines `size * line_height` apart from the block's top, each lined up on `x` by
/// its width.
fn layout(font: &Typeface, w: &Text, size: f32) -> Vec<Placed> {
    let Some(face) = Face::from_slice(&font.data, 0) else { return Vec::new() };
    let (ascent, descent) = font.line_metrics(size);
    let lines: Vec<String> = w.text
        .split('\n')
        .map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            // like a browser's canvas: other whitespace (a tab, say) is a space
            line.chars().map(|c| if c.is_ascii_whitespace() { ' ' } else { c }).collect()
        })
        .collect();
    let step = size * w.line_height;
    let top = block_top(w.valign, w.y, block_height(ascent, descent, lines.len(), step));
    let mut placed = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let baseline = top + ascent + i as f32 * step;
        let (glyphs, width) = shape(&face, line, size / font.units_per_em);
        let left = line_start(w.align, w.x, width);
        placed.extend(glyphs.into_iter().map(|g| Placed { x: left + g.x, y: baseline + g.y, ..g }));
    }
    placed
}

/// A line shaped as the browser shapes it (rustybuzz is a port of HarfBuzz, which Chrome uses): kerning
/// from GPOS or `kern`, ligatures and so on. Gives each glyph's offset from the line's start on the
/// baseline, and the line's width, the sum of its advances. `scale` is pixels per font unit.
fn shape(face: &Face, line: &str, scale: f32) -> (Vec<Placed>, f32) {
    let mut buffer = UnicodeBuffer::new();
    buffer.push_str(line);
    buffer.guess_segment_properties();
    let shaped = rustybuzz::shape(face, &[], buffer);
    let mut pen = 0i64; // in font units, so rounding doesn't add up along the line
    let glyphs = shaped
        .glyph_infos()
        .iter()
        .zip(shaped.glyph_positions())
        .map(|(info, p)| {
            let at = Placed { index: info.glyph_id as u16, x: (pen + p.x_offset as i64) as f32 * scale,
                              y: -p.y_offset as f32 * scale };
            pen += p.x_advance as i64;
            at
        })
        .collect();
    (glyphs, pen as f32 * scale)
}

/// A block of `lines` lines: ascent + descent, plus a step per extra line.
fn block_height(ascent: f32, descent: f32, lines: usize, step: f32) -> f32 {
    ascent + descent + lines.saturating_sub(1) as f32 * step
}

fn block_top(valign: Valign, y: f32, height: f32) -> f32 {
    match valign {
        Valign::Top => y,
        Valign::Middle => y - height / 2.0,
        Valign::Bottom => y - height,
    }
}

fn line_start(align: Align, x: f32, width: f32) -> f32 {
    match align {
        Align::Left => x,
        Align::Center => x - width / 2.0,
        Align::Right => x - width,
    }
}

/// Coverage (0..1) over a rectangle of the panel, which may reach past its edges.
struct Mask {
    left: i32,
    top: i32,
    width: usize,
    height: usize,
    alpha: Vec<f32>,
}

/// The glyphs' coverage, over the area they cover plus `pad` all round, cut down to what can still
/// show: inside the clip (or the panel) by `pad`, so the outline is right at the clip's edge and hidden
/// text isn't rasterized.
fn coverage(font: &Font, size: f32, glyphs: &[Placed], pad: i32, clip: Option<Rect>) -> Option<Mask> {
    // where each glyph's bitmap goes: pen positions round to whole pixels
    let whole = |v: f32| v.clamp(-1e6, 1e6).round() as i32;
    let boxes: Vec<(u16, i32, i32, Metrics)> = glyphs
        .iter()
        .filter(|g| g.index < font.glyph_count())
        .filter_map(|g| {
            let m = font.metrics_indexed(g.index, size);
            (m.width > 0 && m.height > 0)
                .then(|| (g.index, whole(g.x) + m.xmin, whole(g.y) - m.ymin - m.height as i32, m))
        })
        .collect();
    // beyond the edge by more than the outline reaches, nothing can show
    let (cl, ct, cr, cb) = visible(clip);
    if cl >= cr || ct >= cb {
        return None;
    }
    let left = (boxes.iter().map(|b| b.1).min()? - pad).max(cl - pad);
    let top = (boxes.iter().map(|b| b.2).min()? - pad).max(ct - pad);
    let right = (boxes.iter().map(|b| b.1 + b.3.width as i32).max()? + pad).min(cr + pad);
    let bottom = (boxes.iter().map(|b| b.2 + b.3.height as i32).max()? + pad).min(cb + pad);
    if left >= right || top >= bottom {
        return None;
    }
    let (width, height) = ((right - left) as usize, (bottom - top) as usize);
    let mut alpha = vec![0f32; width * height];
    let mut bitmaps: HashMap<u16, Vec<u8>> = HashMap::new();
    for &(index, gx, gy, m) in &boxes {
        // the part of the glyph inside the mask
        let (x0, x1) = ((left - gx).max(0), (right - gx).min(m.width as i32));
        let (y0, y1) = ((top - gy).max(0), (bottom - gy).min(m.height as i32));
        if x0 >= x1 || y0 >= y1 {
            continue;
        }
        let bitmap = bitmaps.entry(index).or_insert_with(|| font.rasterize_indexed(index, size).1);
        for y in y0..y1 {
            let row = (gy + y - top) as usize * width;
            for x in x0..x1 {
                let g = bitmap[y as usize * m.width + x as usize] as f32 / 255.0;
                let a = &mut alpha[row + (gx + x - left) as usize];
                *a += g * (1.0 - *a); // overlapping glyphs add up like paint
            }
        }
    }
    Some(Mask { left, top, width, height, alpha })
}

/// The outline's coverage: the text's, grown by `width` in every direction (a round dilation).
///
/// A pixel q with coverage c lights a pixel p by c + width - |p - q|. That puts the outline's
/// anti-aliased edge `width` beyond the glyph's own edge, which is half a pixel out from a fully
/// covered pixel's centre and nearer in for a partly covered one. With the nearest covered pixel d
/// away (from the distance transform), pixels well inside or out are settled at once, and on the
/// outline's soft edge only covered pixels between d and d + 1 away can give the most.
fn dilate(mask: &Mask, width: f32) -> Vec<f32> {
    let (w, h) = (mask.width as i64, mask.height as i64);
    let nearest = nearest_covered(&mask.alpha, mask.width, mask.height);
    // every offset a covered pixel reaches, nearest first: (dx, dy, squared distance, distance)
    let reach = width.ceil() as i64 + 1;
    let mut offsets: Vec<(i64, i64, i64, f32)> = (-reach..=reach)
        .flat_map(|dy| (-reach..=reach).map(move |dx| (dx, dy, dx * dx + dy * dy)))
        .map(|(dx, dy, d2)| (dx, dy, d2, (d2 as f32).sqrt()))
        .filter(|o| o.3 < width + 1.0)
        .collect();
    offsets.sort_by_key(|o| o.2);
    let mut ring = vec![0f32; nearest.len()];
    for (i, out) in ring.iter_mut().enumerate() {
        let n = nearest[i];
        if n == NONE {
            continue;
        }
        let (px, py) = (i as i64 % w, i as i64 / w);
        let d2 = (px - n as i64 % w).pow(2) + (py - n as i64 / w).pow(2);
        let d = (d2 as f32).sqrt();
        if d >= width + 1.0 {
            continue; // too far for any covered pixel to reach
        }
        if d <= width - 1.0 {
            *out = 1.0; // well inside
            continue;
        }
        let mut best = 0f32;
        for &(dx, dy, _, dist) in &offsets[offsets.partition_point(|o| o.2 < d2)..] {
            if dist > d + 1.0 {
                break;
            }
            let (qx, qy) = (px + dx, py + dy);
            if (0..w).contains(&qx) && (0..h).contains(&qy) {
                let c = mask.alpha[(qy * w + qx) as usize];
                if c > 0.0 {
                    best = best.max(c + width - dist);
                }
            }
        }
        *out = best.min(1.0);
    }
    ring
}

/// For each pixel, the index of the nearest pixel with any coverage (between centres), or NONE.
/// Felzenszwalb and Huttenlocher's exact distance transform: along each row, then down each column.
fn nearest_covered(alpha: &[f32], w: usize, h: usize) -> Vec<u32> {
    // along each row: the nearest covered column
    let mut column = vec![NONE; w * h];
    for y in 0..h {
        let (row, out) = (&alpha[y * w..(y + 1) * w], &mut column[y * w..(y + 1) * w]);
        let mut last = None;
        for x in 0..w {
            if row[x] > 0.0 {
                last = Some(x);
            }
            if let Some(l) = last {
                out[x] = l as u32;
            }
        }
        let mut next = None;
        for x in (0..w).rev() {
            if row[x] > 0.0 {
                next = Some(x);
            }
            if let Some(n) = next {
                if out[x] == NONE || n - x < x - out[x] as usize {
                    out[x] = n as u32;
                }
            }
        }
    }
    // down each column: the lowest of the parabolas (y - q)^2 + dx(q)^2, one per row q
    let mut nearest = vec![NONE; w * h];
    let (mut f, mut rows, mut starts) = (vec![0f64; h], Vec::with_capacity(h), Vec::with_capacity(h));
    for x in 0..w {
        rows.clear();
        starts.clear();
        for q in 0..h {
            let c = column[q * w + x];
            if c == NONE {
                continue;
            }
            f[q] = (x as f64 - c as f64).powi(2) + (q * q) as f64;
            let mut s = f64::NEG_INFINITY;
            while let Some(&p) = rows.last() {
                // where parabola q starts to be lower than parabola p
                s = (f[q] - f[p]) / (2 * (q - p)) as f64;
                if s > *starts.last().unwrap() {
                    break;
                }
                rows.pop();
                starts.pop();
                s = f64::NEG_INFINITY;
            }
            rows.push(q);
            starts.push(s);
        }
        if rows.is_empty() {
            continue;
        }
        let mut k = 0;
        for y in 0..h {
            while k + 1 < rows.len() && starts[k + 1] <= y as f64 {
                k += 1;
            }
            let q = rows[k];
            nearest[y * w + x] = (q * w + column[q * w + x] as usize) as u32;
        }
    }
    nearest
}

/// A font file, read for laying text out (rustybuzz shapes from its bytes) and for drawing its glyphs
/// (fontdue).
struct Typeface {
    data: Vec<u8>,
    glyphs: Font,
    units_per_em: f32,
    /// Ascent and descent in font units, both positive: the ones Chrome uses on Windows (DirectWrite's).
    /// That's the OS/2 table's typographic ones if it says to use them, else its Windows ones (often
    /// taller than hhea's), else hhea's.
    ascent: f32,
    descent: f32,
}

impl Typeface {
    /// None for a file that can't be used, whose text is then drawn in Segoe UI. A font from a shared widget
    /// is untrusted: a broken one must never stop the player.
    fn read(data: Vec<u8>) -> Option<Typeface> {
        let face = ttf_parser::Face::parse(&data, 0).ok()?;
        if !full_name_ok(&face) {
            return None;
        }
        let hhea = face.tables().hhea;
        let (ascent, descent) = match face.tables().os2 {
            Some(os2) if os2.use_typographic_metrics() => {
                (os2.typographic_ascender() as i32, -(os2.typographic_descender() as i32))
            }
            // unsigned in the file: ttf_parser reads them as signed (and negates the descent)
            Some(os2) if (os2.windows_ascender(), os2.windows_descender()) != (0, 0) => {
                (os2.windows_ascender() as u16 as i32, -(os2.windows_descender() as i32) as u16 as i32)
            }
            _ => (hhea.ascender as i32, -(hhea.descender as i32)),
        };
        let units_per_em = face.units_per_em() as f32;
        // substitutions too: shaping can pick glyphs that only GSUB reaches (ligatures, say)
        let settings = FontSettings { scale: CURVES_FOR, load_substitutions: true, ..FontSettings::default() };
        // fontdue isn't hardened against broken files the way ttf_parser is: one it panics on is a font that
        // can't be used, like any other
        let glyphs = std::panic::catch_unwind(|| Font::from_bytes(&data[..], settings)).ok()?.ok()?;
        Some(Typeface { data, glyphs, units_per_em, ascent: ascent as f32, descent: descent as f32 })
    }

    /// Ascent and descent at `size`, both positive, in whole pixels as Chrome rounds them.
    fn line_metrics(&self, size: f32) -> (f32, f32) {
        let px = |units: f32| (units * size / self.units_per_em + 0.5).floor();
        (px(self.ascent), px(self.descent))
    }
}

/// Whether the font's full name, as fontdue reads it (the first Unicode name record with ID 4), is whole
/// UTF-16: an even number of bytes, and every surrogate in a pair. fontdue decodes it without checking, and
/// one that isn't reads past its end (a panic) or makes a char that can't exist.
fn full_name_ok(face: &ttf_parser::Face) -> bool {
    match face.names().into_iter().find(|n| n.name_id == ttf_parser::name_id::FULL_NAME && n.is_unicode()) {
        None => true,
        Some(n) => {
            n.name.len() % 2 == 0
                && char::decode_utf16(n.name.chunks_exact(2).map(|b| u16::from_be_bytes([b[0], b[1]]))).all(|c| c.is_ok())
        }
    }
}

/// Fonts by path, kept for the life of the process. A font file that changes is read again.
fn font(path: &Path) -> Option<Arc<Typeface>> {
    type Fonts = HashMap<PathBuf, (Option<(SystemTime, u64)>, Option<Arc<Typeface>>)>;
    static FONTS: OnceLock<Mutex<Fonts>> = OnceLock::new();
    let stamp = playlist::stamp(path);
    let mut fonts = FONTS.get_or_init(Default::default).lock().unwrap();
    if let Some((seen, font)) = fonts.get(path)
        && *seen == stamp
    {
        return font.clone();
    }
    let data = std::fs::read(path).ok();
    let read = data.is_some();
    let font = data.and_then(Typeface::read).map(Arc::new);
    if read && font.is_none() {
        crate::player::log(format_args!("font {}: can't be read, its text is drawn in Segoe UI", path.display()));
    }
    fonts.insert(path.to_path_buf(), (stamp, font.clone()));
    font
}

/// Identifies how text looks: the texts, plus the size and modified time of each font they use and of
/// the fallback. None when there's no text, so pieces without any keep the cache keys they had.
pub fn key<'a>(texts: impl IntoIterator<Item = &'a Text>) -> Option<u64> {
    let texts: Vec<&Text> = texts.into_iter().collect();
    if texts.is_empty() {
        return None;
    }
    let mut h = DefaultHasher::new(); // fixed keys, so the same text hashes the same every run
    VERSION.hash(&mut h);
    serde_json::to_string(&texts).ok()?.hash(&mut h);
    for font in texts.iter().map(|w| w.font.as_path()).chain([Path::new(DEFAULT_FONT)]) {
        (font, playlist::stamp(font)).hash(&mut h);
    }
    Some(h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    fn text(s: &str) -> Text {
        Text { id: "t".into(), kind: "text".into(), text: s.into(), size: 100.0, ..Text::default() }
    }

    fn have_default_font() -> bool {
        Path::new(DEFAULT_FONT).exists()
    }

    impl Layer {
        /// The pixel at a panel position: transparent outside the image.
        fn at(&self, x: u32, y: u32) -> Rgba<u8> {
            let (l, t) = self.origin();
            if x < l || y < t || x - l >= self.image.width() || y - t >= self.image.height() {
                return Rgba([0, 0, 0, 0]);
            }
            *self.image.get_pixel(x - l, y - t)
        }
    }

    /// Lays the layer onto a black frame: what it draws, as a whole-panel picture.
    fn flat(layer: &Layer) -> RgbImage {
        let mut frame = RgbImage::new(WIDTH, HEIGHT);
        layer.draw_onto(&mut frame);
        frame
    }

    /// The texts drawn as the player drew them while every layer was a whole 1600x720 image: the same
    /// coverage, outline and blend, painted at panel positions, with only the panel's edges cutting them.
    fn whole_panel(texts: &[&Text]) -> RgbaImage {
        let mut image = RgbaImage::new(WIDTH, HEIGHT);
        for w in texts {
            let font = font(&w.font).or_else(|| font(Path::new(DEFAULT_FONT))).unwrap();
            let size = w.size.min(MAX_SIZE);
            let outline = if w.outline.width > 0.0 { w.outline.width.min(MAX_OUTLINE) } else { 0.0 };
            let pad = if outline > 0.0 { outline.ceil() as i32 + 1 } else { 0 };
            let Some(mut mask) = coverage(&font.glyphs, size, &layout(&font, w, size), pad, None) else { continue };
            let mut ring = (outline > 0.0).then(|| dilate(&mask, outline));
            if w.crisp {
                threshold(&mut mask.alpha);
                ring.iter_mut().for_each(|r| threshold(r));
            }
            let (fill, edge) = (rgb(&w.color, [255, 255, 255]), rgb(&w.outline.color, [0, 0, 0]));
            for y in mask.top.max(0)..(mask.top + mask.height as i32).min(HEIGHT as i32) {
                for x in mask.left.max(0)..(mask.left + mask.width as i32).min(WIDTH as i32) {
                    let i = (y - mask.top) as usize * mask.width + (x - mask.left) as usize;
                    let body = mask.alpha[i];
                    let rim = ring.as_ref().map_or(0.0, |r| r[i] * (1.0 - body));
                    if body + rim <= 0.0 {
                        continue;
                    }
                    let color = [0, 1, 2].map(|c| (fill[c] as f32 * body + edge[c] as f32 * rim) / (body + rim));
                    over(image.get_pixel_mut(x as u32, y as u32), color, (body + rim) * w.opacity.min(1.0));
                }
            }
        }
        image
    }

    /// Every panel pixel of the layer equals the whole-panel drawing's.
    fn same_as_whole_panel(layer: &Layer, texts: &[&Text]) {
        let want = whole_panel(texts);
        for (x, y, p) in want.enumerate_pixels() {
            assert_eq!(layer.at(x, y), *p, "at {x},{y}");
        }
    }

    /// A text's layer is the box its glyphs (and outline) cover, cut to the panel, not 1600x720, and it
    /// draws exactly what the whole-panel layer drew; more texts grow it to the box holding them all.
    #[test]
    fn text_layer_is_small() {
        if !have_default_font() {
            return;
        }
        let outlined = Text { x: 300.0, y: 200.0, outline: playlist::Outline { color: "#ff0000".into(), width: 3.0 }, ..text("Small") };
        let layer = Layer::render([&outlined]);
        // the image is the mask's box: the glyphs plus the outline's reach
        let font = font(Path::new(DEFAULT_FONT)).unwrap();
        let mask = coverage(&font.glyphs, 100.0, &layout(&font, &outlined, 100.0), 4, None).unwrap();
        assert_eq!(layer.origin(), (mask.left as u32, mask.top as u32));
        assert_eq!(layer.image().dimensions(), (mask.width as u32, mask.height as u32));
        assert!(mask.width < 400 && mask.height < 150, "{}x{}", mask.width, mask.height);
        same_as_whole_panel(&layer, &[&outlined]);
        // the same through draw_onto: the layer lands where the whole-panel one did
        let mut frame = RgbImage::new(WIDTH, HEIGHT);
        for (x, y, p) in whole_panel(&[&outlined]).enumerate_pixels() {
            let a = p[3] as u32;
            frame.put_pixel(x, y, Rgb([0, 1, 2].map(|c| ((p[c] as u32 * a + 127) / 255) as u8)));
        }
        assert!(flat(&layer) == frame);
        // faint, crisp, and hanging off the panel's edges (cut there, as before)
        for t in [Text { opacity: 0.4, ..outlined.clone() }, Text { crisp: true, ..outlined.clone() },
                  Text { x: -30.0, y: -40.0, ..outlined.clone() }, Text { x: 1590.0, y: 690.0, ..outlined.clone() }] {
            let layer = Layer::render([&t]);
            let (w, h) = layer.image().dimensions();
            assert!(w < 400 && h < 150, "{w}x{h}");
            same_as_whole_panel(&layer, &[&t]);
        }
        // a second text far away grows the image to the box holding both; an overlapping one paints over
        // the first as it did
        let far = Text { x: 1200.0, y: 600.0, ..text("Far") };
        let across = Text { x: 320.0, y: 230.0, color: "#00ff00".into(), opacity: 0.5, ..text("Over") };
        let all = Layer::render([&outlined, &far, &across]);
        let (l, t) = all.origin();
        assert!(l <= layer.origin().0 && t <= layer.origin().1 && all.image().width() > 800 && all.image().height() > 300);
        same_as_whole_panel(&all, &[&outlined, &far, &across]);
    }

    /// Inside its clip a text is drawn exactly as without one, outline and all; outside it nothing is.
    #[test]
    fn clipped_to_its_box() {
        if !have_default_font() {
            return;
        }
        let plain = Text { x: 100.0, y: 100.0, outline: playlist::Outline { color: "#00ff00".into(), width: 4.0 }, ..text("Clip me") };
        let whole = Layer::render([&plain]);
        let b = whole.bounds().unwrap();
        let clip = [b.left as i64 + 20, b.top as i64 + 10, b.right as i64 - 30, b.bottom as i64 - 15];
        let cut = Layer::render([&Text { clip: Some(clip), ..plain.clone() }]);
        let c = cut.bounds().unwrap();
        assert!(c.left as i64 >= clip[0] && c.top as i64 >= clip[1] && c.right as i64 <= clip[2] && c.bottom as i64 <= clip[3], "{c:?}");
        assert!(cut.image().width() as i64 <= clip[2] - clip[0] && cut.image().height() as i64 <= clip[3] - clip[1]);
        for (x, y, p) in flat(&whole).enumerate_pixels() {
            let inside = (clip[0]..clip[2]).contains(&(x as i64)) && (clip[1]..clip[3]).contains(&(y as i64));
            assert_eq!(cut.at(x, y), if inside { whole.at(x, y) } else { Rgba([0, 0, 0, 0]) }, "at {x},{y}: {p:?}");
        }
        // a clip holding the whole text changes nothing; one past the panel, or empty, draws nothing
        let roomy = Layer::render([&Text { clip: Some([-50, -50, 2000, 1000]), ..plain.clone() }]);
        assert!(roomy.image() == whole.image() && roomy.origin() == whole.origin());
        assert!(Layer::render([&Text { clip: Some([-100, -100, -1, -1]), ..plain.clone() }]).is_empty());
        assert!(Layer::render([&Text { clip: Some([200, 200, 200, 300]), ..plain.clone() }]).is_empty());
        assert!(Layer::render([&Text { clip: Some([300, 300, 200, 200]), ..plain.clone() }]).is_empty());
    }

    #[test]
    fn geometry() {
        assert_eq!(block_height(40.0, 10.0, 1, 60.0), 50.0);
        assert_eq!(block_height(40.0, 10.0, 3, 60.0), 170.0);
        assert_eq!(block_top(Valign::Top, 100.0, 50.0), 100.0);
        assert_eq!(block_top(Valign::Middle, 100.0, 50.0), 75.0);
        assert_eq!(block_top(Valign::Bottom, 100.0, 50.0), 50.0);
        assert_eq!(line_start(Align::Left, 300.0, 80.0), 300.0);
        assert_eq!(line_start(Align::Center, 300.0, 80.0), 260.0);
        assert_eq!(line_start(Align::Right, 300.0, 80.0), 220.0);
    }

    #[test]
    fn colours() {
        assert_eq!(rgb("#ff8000", [1, 2, 3]), [255, 128, 0]);
        assert_eq!(rgb("#FFF", [1, 2, 3]), [1, 2, 3]);
        assert_eq!(rgb("ff8000", [1, 2, 3]), [1, 2, 3]);
        assert_eq!(rgb("#gg0000", [1, 2, 3]), [1, 2, 3]);
        assert_eq!(rgb("", [1, 2, 3]), [1, 2, 3]);
    }

    #[test]
    fn no_text_costs_nothing() {
        let layer = Layer::render([]);
        assert!(layer.is_empty() && layer.image.width() == 0);
        let unknown = Text { kind: "image".into(), ..text("x") };
        assert!(Layer::render([&unknown]).is_empty());
        assert!(Layer::render([&text("")]).is_empty());
        assert!(Layer::render([&text("   ")]).is_empty());
        let mut frame = RgbImage::from_pixel(WIDTH, HEIGHT, Rgb([10, 20, 30]));
        layer.draw_onto(&mut frame);
        assert!(frame.pixels().all(|p| p.0 == [10, 20, 30]));
        assert_eq!(key([]), None);
        assert!(key([&text("x")]).is_some());
    }

    #[test]
    fn lines_up_on_the_anchor() {
        if !have_default_font() {
            return;
        }
        let at = |align, valign| {
            let w = Text { x: 800.0, y: 360.0, align, valign, ..text("HH\nHHHH") };
            Layer::render([&w]).bounds().unwrap()
        };
        let font = font(Path::new(DEFAULT_FONT)).unwrap();
        let (ascent, descent) = font.line_metrics(100.0);
        let step = 100.0 * 1.2f32;
        let height = block_height(ascent, descent, 2, step);
        // H sits on the baseline, with a small side bearing either side
        let b = at(Align::Left, Valign::Top);
        assert!((800..815).contains(&b.left), "{b:?}");
        let b = at(Align::Right, Valign::Top);
        assert!((786..=800).contains(&b.right), "{b:?}");
        let b = at(Align::Center, Valign::Top);
        assert!(((b.left + b.right) as f32 / 2.0 - 800.0).abs() <= 1.0, "{b:?}");
        let last_baseline = |top: f32| (top + ascent + step).round() as u32;
        assert_eq!(at(Align::Left, Valign::Top).bottom, last_baseline(360.0));
        assert_eq!(at(Align::Left, Valign::Middle).bottom, last_baseline(360.0 - height / 2.0));
        assert_eq!(at(Align::Left, Valign::Bottom).bottom, last_baseline(360.0 - height));
        // lines are aligned one by one: the short line's centre matches the long one's
        let w = Text { x: 800.0, y: 100.0, align: Align::Center, ..text("HH\nHHHH") };
        let layer = Layer::render([&w]);
        let row_span = |y: u32| {
            let xs: Vec<u32> = (0..WIDTH).filter(|&x| layer.at(x, y)[3] > 0).collect();
            (xs[0] + xs[xs.len() - 1]) as f32 / 2.0
        };
        let mid_line = |i: f32| (100.0 + ascent + i * 120.0 - 30.0) as u32;
        assert!((row_span(mid_line(0.0)) - row_span(mid_line(1.0))).abs() <= 1.0);
    }

    #[test]
    fn kerning() {
        if !have_default_font() {
            return;
        }
        let font = font(Path::new(DEFAULT_FONT)).unwrap();
        let placed = layout(&font, &text("AV"), 100.0);
        let advance = |c| font.glyphs.metrics(c, 100.0).advance_width;
        assert!(placed[1].x - placed[0].x < advance('A') - 1.0);
        // and the line's width includes it: right-aligned, V ends where it would unkerned, less the kern
        let right = layout(&font, &Text { align: Align::Right, x: 500.0, ..text("AV") }, 100.0);
        let kern = placed[1].x - placed[0].x - advance('A');
        assert!((right[0].x - (500.0 - (advance('A') + kern + advance('V')))).abs() < 1e-3);
    }

    /// Line widths as Chrome measures them, in a font that kerns only in GPOS (no `kern` table).
    #[test]
    fn shapes_like_chrome() {
        let Some(path) = std::env::var_os("LOCALAPPDATA")
            .map(|d| PathBuf::from(d).join(r"Microsoft\Windows\Fonts\Montserrat-Regular.ttf"))
            .filter(|p| p.exists())
        else {
            return;
        };
        let font = font(&path).unwrap();
        let face = Face::from_slice(&font.data, 0).unwrap();
        for (line, size, chrome) in
            [("Montserrat 24: AVATAR Toy Wolf", 24.0, 382.58), ("Montserrat AVATAR Toy", 48.0, 561.70), ("AVg", 120.0, 241.20)]
        {
            let (_, width) = shape(&face, line, size / font.units_per_em);
            assert!((width - chrome).abs() < 0.01, "{line} at {size}: {width}, Chrome {chrome}");
        }
        // ligatures, as the browser makes them: "fi" is one glyph
        let (glyphs, _) = shape(&face, "fi", 1.0);
        assert_eq!(glyphs.len(), 1);
    }

    /// Ascent and descent as Chrome has them on Windows, to whole pixels.
    #[test]
    fn line_metrics_like_chrome() {
        let windows = |name: &str| font(&Path::new(r"C:\Windows\Fonts").join(name));
        // Consolas: OS/2's Windows ascent (1884 of 2048), much taller than its hhea one (1521)
        if let Some(consolas) = windows("consola.ttf") {
            assert_eq!(consolas.line_metrics(48.0), (44.0, 12.0));
            assert_eq!(consolas.line_metrics(24.0), (22.0, 6.0));
        }
        if let Some(calibri) = windows("calibri.ttf") {
            assert_eq!(calibri.line_metrics(32.0), (30.0, 9.0));
        }
        // Montserrat says to use its typographic metrics (968 and 251 of 1000)
        let user = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join(r"Microsoft\Windows\Fonts"));
        if let Some(montserrat) = user.and_then(|d| font(&d.join("Montserrat-Regular.ttf"))) {
            assert_eq!(montserrat.line_metrics(48.0), (46.0, 12.0));
        }
    }

    #[test]
    fn crisp_half_is_on() {
        // fontdue gives exactly half covered as 127 of 255
        let mut alpha = [127.0 / 255.0, 126.0 / 255.0, 0.5, 1.0, 0.0];
        threshold(&mut alpha);
        assert_eq!(alpha, [1.0, 0.0, 1.0, 1.0, 0.0]);
    }

    #[test]
    fn outline_opacity_crisp() {
        if !have_default_font() {
            return;
        }
        let plain = Text { x: 100.0, y: 100.0, ..text("Hg") };
        let b = Layer::render([&plain]).bounds().unwrap();
        let outlined = Text { outline: playlist::Outline { color: "#ff0000".into(), width: 6.0 }, ..plain.clone() };
        let layer = Layer::render([&outlined]);
        let o = layer.bounds().unwrap();
        assert!((b.left - o.left).abs_diff(6) <= 1 && (o.right - b.right).abs_diff(6) <= 1, "{b:?} {o:?}");
        assert!((b.top - o.top).abs_diff(6) <= 1 && (o.bottom - b.bottom).abs_diff(6) <= 1, "{b:?} {o:?}");
        // the outline shows red just outside the glyphs, the fill stays white inside
        assert!(layer.image.pixels().any(|p| p.0 == [255, 0, 0, 255]));
        assert!(layer.image.pixels().any(|p| p.0 == [255, 255, 255, 255]));

        let faint = Layer::render([&Text { opacity: 0.5, ..plain.clone() }]);
        assert_eq!(faint.image.pixels().map(|p| p[3]).max(), Some(128));
        // opacity applies to fill and outline as one: the outline doesn't show through the fill
        let faint = Layer::render([&Text { opacity: 0.5, ..outlined.clone() }]);
        assert_eq!(faint.image.pixels().map(|p| p[3]).max(), Some(128));
        assert!(faint.image.pixels().any(|p| p.0 == [255, 255, 255, 128]));
        assert!(faint.image.pixels().any(|p| p.0 == [255, 0, 0, 128]));

        let crisp = Layer::render([&Text { crisp: true, ..outlined.clone() }]);
        assert!(crisp.image.pixels().all(|p| p[3] == 0 || p[3] == 255));
        assert!(crisp.image.pixels().all(|p| p[3] == 0 || matches!(p.0, [255, 0, 0, _] | [255, 255, 255, _])));
    }

    #[test]
    fn missing_font_falls_back() {
        if !have_default_font() {
            return;
        }
        let normal = Layer::render([&text("Fallback")]);
        let missing = Layer::render([&Text { font: r"C:\nowhere\nothing.ttf".into(), ..text("Fallback") }]);
        assert!(!normal.is_empty());
        assert_eq!(normal.bounds(), missing.bounds());
        assert!(normal.image == missing.image);
    }

    /// A font's tables, as (tag, offset, length).
    fn tables(data: &[u8]) -> Vec<([u8; 4], usize, usize)> {
        let be32 = |at: usize| u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        let count = u16::from_be_bytes([data[4], data[5]]) as usize;
        (0..count).map(|i| (data[12 + 16 * i..16 + 16 * i].try_into().unwrap(), be32(20 + 16 * i), be32(24 + 16 * i))).collect()
    }

    /// The font with its full name (the record fontdue reads: name ID 4, Unicode) changed by `change`, given
    /// the record's length field's place and the name's own bytes' place.
    fn with_full_name(data: &[u8], change: impl Fn(&mut Vec<u8>, usize, usize)) -> Vec<u8> {
        let mut data = data.to_vec();
        let (_, at, _) = tables(&data).into_iter().find(|t| &t.0 == b"name").expect("no name table");
        let be16 = |d: &[u8], i: usize| u16::from_be_bytes([d[i], d[i + 1]]) as usize;
        let (count, strings) = (be16(&data, at + 2), at + be16(&data, at + 4));
        let record = (0..count).map(|k| at + 6 + 12 * k).find(|&r| {
            let (platform, encoding, id) = (be16(&data, r), be16(&data, r + 2), be16(&data, r + 6));
            id == 4 && (platform == 0 || (platform == 3 && [0, 1, 10].contains(&encoding)))
        });
        let r = record.expect("no full name");
        let name = strings + be16(&data, r + 10);
        change(&mut data, r + 8, name);
        data
    }

    /// A shared widget's font is untrusted: one fontdue can't read (a full name of an odd length, a half
    /// surrogate pair, bytes changed anywhere in its tables) draws its text in Segoe UI, and never panics.
    #[test]
    fn broken_fonts_fall_back() {
        let fonts = Path::new(r"C:\Windows\Fonts");
        let Some(source) = ["framd.ttf", "arial.ttf", "bahnschrift.ttf"].iter().map(|f| fonts.join(f)).find(|p| p.exists()) else { return };
        if !have_default_font() {
            return;
        }
        let good = std::fs::read(&source).unwrap();
        let dir = std::env::temp_dir().join(format!("wvplay-text-broken-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sample = |font: &Path| Layer::render([&Text { font: font.into(), ..text("12:34 Hello") }]);
        let normal = Layer::render([&text("12:34 Hello")]);
        let own = sample(&source);
        assert!(own.image != normal.image, "the font draws like Segoe UI: pick another");
        let odd = with_full_name(&good, |d, len, _| {
            let n = u16::from_be_bytes([d[len], d[len + 1]]);
            d[len..len + 2].copy_from_slice(&(n - 1).to_be_bytes());
        });
        // a high surrogate with no low one after it, at the end of the name
        let half = with_full_name(&good, |d, len, name| {
            let n = u16::from_be_bytes([d[len], d[len + 1]]) as usize;
            d[name + n - 2..name + n].copy_from_slice(&[0xD8, 0x3D]);
        });
        // a low surrogate on its own, which fontdue would turn into a char past U+10FFFF
        let low = with_full_name(&good, |d, _, name| d[name..name + 4].copy_from_slice(&[0xDF, 0xFF, 0xFF, 0xFF]));
        for (name, bytes) in [("odd.ttf", &odd), ("half.ttf", &half), ("low.ttf", &low)] {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            let drawn = sample(&path);
            assert!(drawn.image == normal.image && drawn.bounds() == normal.bounds(), "{name} isn't drawn in Segoe UI");
        }
        // a small font with bytes changed inside its tables (the table directory kept whole), as a fuzzer
        // would: each one draws, in itself or in Segoe UI, without a panic
        let small = fonts.join("marlett.ttf");
        let seed = if small.exists() { std::fs::read(&small).unwrap() } else { good.clone() };
        let spans: Vec<(usize, usize)> = tables(&seed).into_iter().filter(|t| t.2 > 0 && t.1 + t.2 <= seed.len()).map(|t| (t.1, t.2)).collect();
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |n: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n as u64) as usize
        };
        for case in 0..300 {
            let mut bytes = seed.clone();
            for _ in 0..[1, 2, 3, 5, 10, 30][next(6)] {
                let (off, len) = spans[next(spans.len())];
                let at = off + next(len);
                let word: &[u8] = &[[0x00, 0x00], [0x7F, 0xFF], [0x80, 0x00], [0xFF, 0xFF], [0xFF, 0xFE], [next(256) as u8, next(256) as u8]][next(6)];
                let end = (at + if next(2) == 0 { 1 } else { 2 }).min(off + len);
                bytes[at..end].copy_from_slice(&word[..end - at]);
            }
            let path = dir.join(format!("fuzz-{case}.ttf"));
            std::fs::write(&path, &bytes).unwrap();
            let _ = sample(&path);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clipped_at_the_edges() {
        if !have_default_font() {
            return;
        }
        let off = Text { x: -20.0, y: 620.0, outline: playlist::Outline { width: 4.0, ..Default::default() }, ..text("Edge") };
        let b = Layer::render([&off]).bounds().unwrap();
        assert_eq!((b.left, b.bottom), (0, HEIGHT));
        let gone = Text { x: 5000.0, y: -5000.0, ..text("Gone") };
        assert!(Layer::render([&gone]).is_empty());
    }

    #[test]
    fn draws_onto_frames() {
        if !have_default_font() {
            return;
        }
        let layer = Layer::render([&Text { x: 10.0, y: 10.0, ..text("I") }]);
        let mut frame = RgbImage::from_pixel(WIDTH, HEIGHT, Rgb([0, 0, 255]));
        layer.draw_onto(&mut frame);
        let b = layer.bounds().unwrap();
        for (x, y, p) in frame.enumerate_pixels() {
            let inside = (b.left..b.right).contains(&x) && (b.top..b.bottom).contains(&y);
            match layer.at(x, y)[3] {
                0 => assert_eq!(p.0, [0, 0, 255]),
                255 => assert_eq!(p.0, [255, 255, 255]),
                _ => assert!(inside && p[0] > 0 && p[0] < 255),
            }
        }
    }

    /// The fast dilation against the definition, on real glyph edges.
    #[test]
    fn dilation_matches_brute_force() {
        if !have_default_font() {
            return;
        }
        let font = font(Path::new(DEFAULT_FONT)).unwrap();
        for (width, size) in [(0.5, 30.0), (1.0, 40.0), (2.5, 40.0), (7.0, 60.0), (20.0, 80.0)] {
            let pad = (width as f32).ceil() as i32 + 1;
            let w = Text { x: 20.0, y: 20.0, ..text("Ag/o") };
            let mask = coverage(&font.glyphs, size, &layout(&font, &w, size), pad, None).unwrap();
            let fast = dilate(&mask, width);
            let (mw, mh, reach) = (mask.width as i64, mask.height as i64, pad as i64);
            let mut worst = 0f32;
            for py in 0..mh {
                for px in 0..mw {
                    let mut best = 0f32;
                    for qy in (py - reach).max(0)..(py + reach + 1).min(mh) {
                        for qx in (px - reach).max(0)..(px + reach + 1).min(mw) {
                            let c = mask.alpha[(qy * mw + qx) as usize];
                            if c > 0.0 {
                                let d = (((px - qx).pow(2) + (py - qy).pow(2)) as f32).sqrt();
                                best = best.max(c + width - d);
                            }
                        }
                    }
                    worst = worst.max((best.min(1.0) - fast[(py * mw + px) as usize]).abs());
                }
            }
            assert!(worst < 1e-5, "outline {width}: off by {worst}");
        }
    }
}
