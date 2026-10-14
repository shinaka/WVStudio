//! Shape objects (docs/scenes.md): rects with rounded corners, ellipses and lines, with solid or
//! linear-gradient fills and strokes, drawn into a layer's buffer.
//!
//! A pixel's coverage is how much of it the region covers, estimated from a grid of sample points, or
//! with `crisp`, exactly whether its centre is inside. Both renderers follow that rule, so crisp shapes
//! match the studio pixel for pixel. Away from the edges a pixel is all in or all out, which a look at
//! its corners settles without sampling.

use image::RgbaImage;

use crate::playlist::{Object, Paint, Shape, ShapeKind};

/// Samples per pixel edge for anti-aliasing.
const GRID: u32 = 4;

/// The shape's extent on the canvas: left and top inclusive, right and bottom exclusive.
pub fn bounds(o: &Object, s: &Shape) -> (i64, i64, i64, i64) {
    let (x0, x1) = (o.x.min(o.x + o.w), o.x.max(o.x + o.w));
    let (y0, y1) = (o.y.min(o.y + o.h), o.y.max(o.y + o.h));
    let r = s.stroke.as_ref().map_or(0.0, |st| st.width / 2.0) + 1.0;
    ((x0 - r).floor() as i64, (y0 - r).floor() as i64, (x1 + r).ceil() as i64, (y1 + r).ceil() as i64)
}

/// Where a point is against the shape: in its fill, in its stroke, both or neither.
struct Region<'a> {
    kind: ShapeKind,
    /// The box, with the lower corner first (a line's w/h may be negative).
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    radius: f64,
    half_stroke: f64,
    shape: &'a Shape,
    /// A line's ends.
    a: (f64, f64),
    b: (f64, f64),
}

impl Region<'_> {
    /// The signed distance to the fill's outline (negative inside); a line has no fill.
    fn distance(&self, px: f64, py: f64) -> f64 {
        match self.kind {
            ShapeKind::Rect => {
                let (hw, hh) = ((self.x1 - self.x0) / 2.0, (self.y1 - self.y0) / 2.0);
                let r = self.radius.max(0.0).min(hw).min(hh);
                let (dx, dy) = ((px - (self.x0 + hw)).abs() - hw + r, (py - (self.y0 + hh)).abs() - hh + r);
                let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
                outside + dx.max(dy).min(0.0) - r
            }
            ShapeKind::Ellipse => {
                // scaled distance: exact on the axes, close enough elsewhere for a stroke's width
                let (rx, ry) = ((self.x1 - self.x0) / 2.0, (self.y1 - self.y0) / 2.0);
                let (cx, cy) = (self.x0 + rx, self.y0 + ry);
                if rx <= 0.0 || ry <= 0.0 {
                    return f64::INFINITY;
                }
                let (nx, ny) = ((px - cx) / rx, (py - cy) / ry);
                let q = (nx * nx + ny * ny).sqrt();
                let k = if q == 0.0 { rx.min(ry) } else { ((nx * rx).powi(2) + (ny * ry).powi(2)).sqrt() / q };
                (q - 1.0) * k
            }
            ShapeKind::Line => {
                let ((ax, ay), (bx, by)) = (self.a, self.b);
                let (vx, vy) = (bx - ax, by - ay);
                let len2 = vx * vx + vy * vy;
                let t = if len2 == 0.0 { 0.0 } else { (((px - ax) * vx + (py - ay) * vy) / len2).clamp(0.0, 1.0) };
                ((px - ax - t * vx).powi(2) + (py - ay - t * vy).powi(2)).sqrt()
            }
        }
    }

    fn in_fill(&self, px: f64, py: f64) -> bool {
        match self.kind {
            ShapeKind::Line => false,
            // a plain rect is half-open, like a canvas fillRect: x..x+w covers exactly w pixel centres
            ShapeKind::Rect if self.radius <= 0.0 => px >= self.x0 && px < self.x1 && py >= self.y0 && py < self.y1,
            _ => self.shape.fill.is_some() && self.distance(px, py) <= 0.0,
        }
    }

    fn in_stroke(&self, px: f64, py: f64) -> bool {
        if self.half_stroke <= 0.0 {
            return false;
        }
        match self.kind {
            ShapeKind::Line => self.distance(px, py) <= self.half_stroke,
            _ => self.distance(px, py).abs() <= self.half_stroke,
        }
    }

    /// Fill and stroke coverage of the pixel at (x, y), 0..1 each.
    fn coverage(&self, x: i64, y: i64, crisp: bool) -> (f64, f64) {
        let has_fill = self.shape.fill.is_some() && self.kind != ShapeKind::Line;
        let (cx, cy) = (x as f64 + 0.5, y as f64 + 0.5);
        if crisp {
            return (if has_fill && self.in_fill(cx, cy) { 1.0 } else { 0.0 }, if self.in_stroke(cx, cy) { 1.0 } else { 0.0 });
        }
        // corners agreeing with the centre: the whole pixel is one thing
        let probe = |px: f64, py: f64| (has_fill && self.in_fill(px, py), self.in_stroke(px, py));
        let at_centre = probe(cx, cy);
        let (x0, y0, x1, y1) = (x as f64, y as f64, x as f64 + 1.0, y as f64 + 1.0);
        if [probe(x0, y0), probe(x1, y0), probe(x0, y1), probe(x1, y1)].iter().all(|p| *p == at_centre) {
            return (if at_centre.0 { 1.0 } else { 0.0 }, if at_centre.1 { 1.0 } else { 0.0 });
        }
        let (mut fill, mut stroke) = (0, 0);
        for j in 0..GRID {
            for i in 0..GRID {
                let (px, py) = (x0 + (i as f64 + 0.5) / GRID as f64, y0 + (j as f64 + 0.5) / GRID as f64);
                let (f, s) = probe(px, py);
                fill += f as u32;
                stroke += s as u32;
            }
        }
        let n = (GRID * GRID) as f64;
        (fill as f64 / n, stroke as f64 / n)
    }
}

/// Draws the shape into `buf` (straight RGBA, the whole canvas) at the object's opacity, as one group:
/// the stroke covers the fill, and neither shows through the other where the object is translucent.
/// Returns the pixel box it touched, if any.
pub fn draw(buf: &mut RgbaImage, o: &Object, s: &Shape) -> Option<(i64, i64, i64, i64)> {
    let fill_at = s.fill.as_ref().map(|paint| paint_fn(paint, o));
    let stroke = s.stroke.as_ref().filter(|st| st.width > 0.0).map(|st| (rgb(&st.color), st.width / 2.0));
    if fill_at.is_none() && stroke.is_none() || o.opacity <= 0.0 {
        return None;
    }
    let region = Region {
        kind: s.shape,
        x0: o.x.min(o.x + o.w),
        y0: o.y.min(o.y + o.h),
        x1: o.x.max(o.x + o.w),
        y1: o.y.max(o.y + o.h),
        radius: s.radius,
        half_stroke: stroke.map_or(0.0, |(_, h)| h),
        shape: s,
        a: (o.x, o.y),
        b: (o.x + o.w, o.y + o.h),
    };
    let (bw, bh) = (buf.width() as i64, buf.height() as i64);
    let (l, t, r, b) = bounds(o, s);
    let (l, t, r, b) = (l.max(0), t.max(0), r.min(bw), b.min(bh));
    if l >= r || t >= b {
        return None;
    }
    let mut painted: Option<(i64, i64, i64, i64)> = None;
    for y in t..b {
        for x in l..r {
            let (fc, sc) = region.coverage(x, y, s.crisp);
            if fc <= 0.0 && sc <= 0.0 {
                continue;
            }
            // the object's own pixel, premultiplied: the fill, then the stroke over it
            let mut px = [0.0f64; 4];
            if let (Some(fill_at), true) = (&fill_at, fc > 0.0) {
                let c = fill_at(x as f64 + 0.5, y as f64 + 0.5);
                px = [c[0] as f64 * fc, c[1] as f64 * fc, c[2] as f64 * fc, fc];
            }
            if let (Some((c, _)), true) = (stroke, sc > 0.0) {
                for k in 0..3 {
                    px[k] = c[k] as f64 * sc + px[k] * (1.0 - sc);
                }
                px[3] = sc + px[3] * (1.0 - sc);
            }
            let a = px[3] * o.opacity as f64;
            if a <= 0.0 {
                continue;
            }
            let colour = [px[0] / px[3], px[1] / px[3], px[2] / px[3]];
            over(buf.get_pixel_mut(x as u32, y as u32), colour, a);
            painted = Some(match painted {
                Some((pl, pt, pr, pb)) => (pl.min(x), pt.min(y), pr.max(x + 1), pb.max(y + 1)),
                None => (x, y, x + 1, y + 1),
            });
        }
    }
    painted
}

/// Blends a colour with alpha `a` over a straight-alpha pixel.
pub fn over(px: &mut image::Rgba<u8>, colour: [f64; 3], a: f64) {
    let da = px[3] as f64 / 255.0;
    let oa = a + da * (1.0 - a);
    if oa <= 0.0 {
        return;
    }
    for k in 0..3 {
        let c = (colour[k] * a + px[k] as f64 * da * (1.0 - a)) / oa;
        px[k] = (c + 0.5).clamp(0.0, 255.0) as u8;
    }
    px[3] = (oa * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
}

/// The fill's colour at a point.
fn paint_fn(paint: &Paint, o: &Object) -> Box<dyn Fn(f64, f64) -> [u8; 3]> {
    match paint {
        Paint::Solid { color } => {
            let c = rgb(color);
            Box::new(move |_, _| c)
        }
        Paint::Linear { angle, stops } => {
            // 0 runs left to right, 90 top to bottom; the box's corners span 0..1, as CSS does it
            let (cx, cy) = (o.x + o.w / 2.0, o.y + o.h / 2.0);
            let (dx, dy) = (angle.to_radians().cos(), angle.to_radians().sin());
            let len = (o.w.abs() * dx.abs() + o.h.abs() * dy.abs()).max(1e-9);
            let mut stops: Vec<(f64, [f64; 3])> = stops.iter().map(|s| (s.at, rgb(&s.color).map(f64::from))).collect();
            stops.sort_by(|a, b| a.0.total_cmp(&b.0));
            if stops.is_empty() {
                stops.push((0.0, [255.0; 3]));
            }
            Box::new(move |px, py| {
                let t = ((px - cx) * dx + (py - cy) * dy) / len + 0.5;
                let c = if t <= stops[0].0 {
                    stops[0].1
                } else if t >= stops[stops.len() - 1].0 {
                    stops[stops.len() - 1].1
                } else {
                    let i = stops.iter().position(|s| s.0 > t).unwrap();
                    let (a, b) = (&stops[i - 1], &stops[i]);
                    let k = if b.0 > a.0 { (t - a.0) / (b.0 - a.0) } else { 1.0 };
                    [0, 1, 2].map(|j| a.1[j] + (b.1[j] - a.1[j]) * k)
                };
                c.map(|v| (v + 0.5).clamp(0.0, 255.0) as u8)
            })
        }
    }
}

/// `#rrggbb` (or `#rgb`); anything else is white.
pub fn rgb(s: &str) -> [u8; 3] {
    let hex = s.trim().trim_start_matches('#');
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    match hex.len() {
        6 => (|| Some([byte(0)?, byte(2)?, byte(4)?]))().unwrap_or([255; 3]),
        3 => {
            let n = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok().map(|v| v * 17);
            (|| Some([n(0)?, n(1)?, n(2)?]))().unwrap_or([255; 3])
        }
        _ => [255; 3],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playlist::{Kind, Stop, Stroke};

    fn object(x: f64, y: f64, w: f64, h: f64, s: Shape) -> (Object, Shape) {
        let o = Object { id: "s".into(), visible: true, opacity: 1.0, x, y, w, h, rotate: 0, flip: false, kind: Kind::Shape(s.clone()) };
        (o, s)
    }

    fn painted(buf: &RgbaImage) -> Vec<(u32, u32)> {
        buf.enumerate_pixels().filter(|(_, _, p)| p[3] > 0).map(|(x, y, _)| (x, y)).collect()
    }

    /// Crisp: a pixel is painted exactly when its centre is inside, so a rect at whole pixels covers
    /// exactly its box, and one at half pixels still covers w x h.
    #[test]
    fn crisp_rect_is_exact() {
        let mut buf = RgbaImage::new(20, 20);
        let (o, s) = object(2.0, 3.0, 4.0, 2.0, Shape { crisp: true, ..Shape::default() });
        assert_eq!(draw(&mut buf, &o, &s), Some((2, 3, 6, 5)));
        let mut want = Vec::new();
        for y in 3..5 {
            for x in 2..6 {
                want.push((x, y));
            }
        }
        assert_eq!(painted(&buf), want);
        assert!(buf.pixels().filter(|p| p[3] > 0).all(|p| *p == image::Rgba([255, 255, 255, 255])));
        let mut buf = RgbaImage::new(20, 20);
        let (o, s) = object(2.5, 3.5, 4.0, 2.0, Shape { crisp: true, ..Shape::default() });
        draw(&mut buf, &o, &s);
        assert_eq!(painted(&buf).len(), 8); // 2.5 <= centre < 6.5: columns 2..=5, and rows 3..=4
        assert_eq!(painted(&buf)[0], (2, 3));
    }

    /// A crisp stroke is the pixels whose centres are within half its width of the outline.
    #[test]
    fn crisp_stroke_and_ellipse() {
        let mut buf = RgbaImage::new(30, 30);
        let stroke = Some(Stroke { color: "#ff0000".into(), width: 2.0 });
        let (o, s) = object(5.0, 5.0, 10.0, 10.0, Shape { crisp: true, fill: None, stroke, ..Shape::default() });
        draw(&mut buf, &o, &s);
        // ring one pixel thick each side of the outline at 5 and 15: columns 4..=5 and 14..=15
        let px = painted(&buf);
        assert!(px.contains(&(4, 10)) && px.contains(&(5, 10)) && !px.contains(&(6, 10)) && !px.contains(&(3, 10)));
        assert!(px.contains(&(14, 10)) && px.contains(&(15, 10)) && !px.contains(&(16, 10)));
        assert_eq!(buf.get_pixel(4, 10), &image::Rgba([255, 0, 0, 255]));
        // an ellipse's centre pixel is in, the box corner is out
        let mut buf = RgbaImage::new(30, 30);
        let (o, s) = object(5.0, 5.0, 10.0, 10.0, Shape { crisp: true, shape: ShapeKind::Ellipse, ..Shape::default() });
        draw(&mut buf, &o, &s);
        let px = painted(&buf);
        assert!(px.contains(&(10, 10)) && !px.contains(&(5, 5)) && px.contains(&(5, 10)));
        for (x, y) in &px {
            let (nx, ny) = ((*x as f64 + 0.5 - 10.0) / 5.0, (*y as f64 + 0.5 - 10.0) / 5.0);
            assert!(nx * nx + ny * ny <= 1.0 + 1e-9);
        }
    }

    /// Anti-aliased edges are partly covered, and the whole shape's coverage adds up to its area.
    #[test]
    fn smooth_edges() {
        let mut buf = RgbaImage::new(20, 20);
        let (o, s) = object(2.5, 2.0, 4.0, 3.0, Shape::default());
        draw(&mut buf, &o, &s);
        assert_eq!(buf.get_pixel(2, 3)[3], 128); // half covered
        assert_eq!(buf.get_pixel(3, 3)[3], 255);
        let total: f64 = buf.pixels().map(|p| p[3] as f64 / 255.0).sum();
        assert!((total - 12.0).abs() < 0.1, "{total}");
        // a line from corner to corner
        let mut buf = RgbaImage::new(20, 20);
        let stroke = Some(Stroke { color: "#00ff00".into(), width: 2.0 });
        let (o, s) = object(2.0, 2.0, 10.0, 10.0, Shape { shape: ShapeKind::Line, stroke, ..Shape::default() });
        draw(&mut buf, &o, &s);
        assert!(buf.get_pixel(6, 6)[3] == 255 && buf.get_pixel(12, 2)[3] == 0);
    }

    #[test]
    fn gradients_run_along_the_angle() {
        let fill = Some(Paint::Linear { angle: 0.0, stops: vec![Stop { at: 0.0, color: "#000000".into() }, Stop { at: 1.0, color: "#ffffff".into() }] });
        let mut buf = RgbaImage::new(20, 20);
        let (o, s) = object(0.0, 0.0, 10.0, 4.0, Shape { fill, crisp: true, ..Shape::default() });
        draw(&mut buf, &o, &s);
        let row: Vec<u8> = (0..10).map(|x| buf.get_pixel(x, 1)[0]).collect();
        assert!(row.windows(2).all(|w| w[1] > w[0]), "{row:?}");
        assert_eq!((row[0], row[9]), (13, 242)); // pixel centres at 0.05 and 0.95 of the way
        assert_eq!(buf.get_pixel(3, 1), buf.get_pixel(3, 3)); // the same across
        let fill = Some(Paint::Linear { angle: 90.0, stops: vec![Stop { at: 0.0, color: "#ff0000".into() }, Stop { at: 0.5, color: "#0000ff".into() }] });
        let mut buf = RgbaImage::new(20, 20);
        let (o, s) = object(0.0, 0.0, 10.0, 10.0, Shape { fill, crisp: true, ..Shape::default() });
        draw(&mut buf, &o, &s);
        assert_eq!(buf.get_pixel(5, 9).0, [0, 0, 255, 255]); // past the last stop: its colour
        assert!(buf.get_pixel(5, 0)[0] > 200);
    }

    #[test]
    fn colours() {
        assert_eq!(rgb("#102030"), [16, 32, 48]);
        assert_eq!(rgb("#fff"), [255, 255, 255]);
        assert_eq!(rgb("red"), [255, 255, 255]);
    }
}
