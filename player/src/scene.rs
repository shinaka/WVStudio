//! Draws scenes (docs/scenes.md): the background, then each visible layer's objects into a transparent
//! buffer, blended over the frame at the layer's opacity, with the global layers on top.
//!
//! Static work happens once. Before the frames are made, the stack is sorted into static and animated
//! parts: everything below the first animated layer is flattened into one RGB base, each static run
//! above into one RGBA still, and only animated layers are drawn per frame, and then only when one of
//! their objects' frames has changed. Static layer buffers are also kept across re-prepares, keyed on
//! the layer's content, so editing one layer draws only that layer again.
//!
//! Objects from a widget's copy (docs/widgets.md, Drawing) are drawn like any other, each inside its
//! clip. The translucent copies and widget layers they sit in are groups: their objects are drawn
//! together into a buffer the size of the group's box, which is then laid over at the group's opacity.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::cell::Cell;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use image::{RgbImage, RgbaImage};

use crate::art::{self, HEIGHT, Options, Over, Prepared, WIDTH};
use crate::media::{self, Cursor, Source, View};
use crate::playlist::{Fill, Group, Kind, Layer, Object, Rect, Scene, Shape, Text, stamp};
use crate::text::{self, DEFAULT_FONT};
use crate::widget::rnd;
use crate::{player, shape};

/// Bump when drawing changes, so scenes prepared by an older player are drawn again.
pub const VERSION: u32 = 3;
/// The longest cycle: the loops' least common multiple is cut off here.
const MAX_CYCLE: Duration = Duration::from_secs(60);
/// Frames closer together than this are merged.
const MIN_FRAME: Duration = Duration::from_millis(20);
/// Frames composed at a time before encoding: each holds its animated layers' buffers.
const BATCH: usize = 32;
/// Static layer buffers kept across re-prepares.
const LAYER_BYTES: usize = 256 << 20;

thread_local! {
    /// How many layer buffers this thread has drawn, for tests of what's reused.
    pub static RENDERS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
thread_local! {
    /// The size of every group buffer this thread has made, for tests of what groups cost.
    static GROUP_SIZES: std::cell::RefCell<Vec<(u32, u32)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A drawn layer: straight RGBA over the part of the canvas it touches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Buf {
    pub left: u32,
    pub top: u32,
    /// 0x0 when the layer drew nothing.
    pub image: RgbaImage,
}

impl Buf {
    fn empty() -> Buf {
        Buf { left: 0, top: 0, image: RgbaImage::new(0, 0) }
    }

    pub fn is_empty(&self) -> bool {
        self.image.width() == 0
    }

    fn bytes(&self) -> usize {
        self.image.len()
    }

    /// The part of `canvas`, whose top-left is `origin` on the panel, inside the pixel box: cut to the
    /// canvas, and to the panel, where a `Buf` always lies.
    fn crop(canvas: &RgbaImage, origin: (i64, i64), rect: Option<(i64, i64, i64, i64)>) -> Buf {
        let on_panel = overlap(extent(origin, canvas), PANEL);
        let Some((l, t, r, b)) = rect.zip(on_panel).and_then(|(rect, canvas)| overlap(rect, canvas)) else { return Buf::empty() };
        let (x, y) = ((l - origin.0) as u32, (t - origin.1) as u32);
        let image = image::imageops::crop_imm(canvas, x, y, (r - l) as u32, (b - t) as u32).to_image();
        Buf { left: l as u32, top: t as u32, image }
    }

    /// `crop`, taking the canvas itself when the whole of it was touched (and it lies on the panel).
    fn take(canvas: RgbaImage, origin: (i64, i64), rect: Option<(i64, i64, i64, i64)>) -> Buf {
        let whole = extent(origin, &canvas);
        match rect {
            Some(rect) if overlap(rect, whole) == Some(whole) && overlap(whole, PANEL) == Some(whole) => {
                Buf { left: origin.0 as u32, top: origin.1 as u32, image: canvas }
            }
            _ => Buf::crop(&canvas, origin, rect),
        }
    }

    /// Lays the buffer over an RGB frame at `opacity`. With `turned`, the frame has been turned 180
    /// degrees already, and the buffer lands where it would have before the turn.
    pub fn onto_rgb(&self, frame: &mut RgbImage, opacity: f32, turned: bool) {
        if self.is_empty() || frame.dimensions() != (WIDTH, HEIGHT) {
            return;
        }
        let k = (opacity.clamp(0.0, 1.0) * 256.0).round() as u32;
        let (w, h) = self.image.dimensions();
        let last = (WIDTH * HEIGHT) as usize - 1;
        let (src, dst): (&[u8], &mut [u8]) = (&self.image, frame);
        for y in 0..h as usize {
            for x in 0..w as usize {
                let s = &src[(y * w as usize + x) * 4..][..4];
                let a = (s[3] as u32 * k + 128) >> 8;
                if a == 0 {
                    continue;
                }
                let i = (self.top as usize + y) * WIDTH as usize + self.left as usize + x;
                let d = &mut dst[(if turned { last - i } else { i }) * 3..][..3];
                for c in 0..3 {
                    d[c] = ((s[c] as u32 * a + d[c] as u32 * (255 - a) + 127) / 255) as u8;
                }
            }
        }
    }

    /// Lays the buffer over an RGBA buffer whose top-left is `origin` on the panel, at `opacity`, where the
    /// two meet. Returns the box it touched, in panel pixels.
    fn onto_rgba(&self, target: &mut RgbaImage, origin: (i64, i64), opacity: f32) -> Option<(i64, i64, i64, i64)> {
        if self.is_empty() {
            return None;
        }
        let (w, h) = self.image.dimensions();
        let (ox, oy) = origin;
        let (l, t) = ((self.left as i64).max(ox), (self.top as i64).max(oy));
        let r = (self.left as i64 + w as i64).min(ox + target.width() as i64);
        let b = (self.top as i64 + h as i64).min(oy + target.height() as i64);
        if l >= r || t >= b {
            return None;
        }
        let k = (opacity.clamp(0.0, 1.0) * 256.0).round() as u32;
        for y in t..b {
            for x in l..r {
                let s = self.image.get_pixel((x - self.left as i64) as u32, (y - self.top as i64) as u32);
                let a = (s[3] as u32 * k + 128) >> 8;
                if a > 0 {
                    over_u8(target.get_pixel_mut((x - ox) as u32, (y - oy) as u32), &s.0, a);
                }
            }
        }
        Some((l, t, r, b))
    }
}

/// Blends a colour with alpha `sa` (0..255) over a straight-alpha pixel, in integers: this runs per pixel
/// of every animated layer.
#[inline]
fn over_u8(d: &mut image::Rgba<u8>, s: &[u8], sa: u32) {
    let da = d[3] as u32;
    if da == 0 {
        // onto nothing yet: the usual case for the first object in a layer
        *d = image::Rgba([s[0], s[1], s[2], sa as u8]);
        return;
    }
    let rest = da * (255 - sa) / 255; // what shows of what was there
    let oa = sa + rest;
    if oa == 0 {
        return;
    }
    for c in 0..3 {
        d[c] = ((s[c] as u32 * sa + d[c] as u32 * rest + oa / 2) / oa) as u8;
    }
    d[3] = oa as u8;
}

/// An object ready to draw.
struct Obj<'a> {
    object: &'a Object,
    kind: ObjKind<'a>,
}

enum ObjKind<'a> {
    Media {
        cursor: Cursor,
        fill: Fill,
        /// When each frame starts within one loop, and the loop's length.
        starts: Vec<Duration>,
        loop_len: Duration,
    },
    Text(&'a Text),
    Shape(&'a Shape),
}

impl Obj<'_> {
    fn animated(&self) -> bool {
        matches!(&self.kind, ObjKind::Media { starts, .. } if starts.len() > 1)
    }

    /// Text with placeholders, filled in while playing.
    fn live(&self) -> bool {
        matches!(&self.kind, ObjKind::Text(w) if w.live)
    }

    /// Which of its frames shows at `t`.
    fn index_at(&self, t: Duration) -> usize {
        match &self.kind {
            ObjKind::Media { starts, loop_len, .. } if starts.len() > 1 => {
                let u = Duration::from_micros((t.as_micros() % loop_len.as_micros().max(1)) as u64);
                starts.partition_point(|s| *s <= u).saturating_sub(1)
            }
            _ => 0,
        }
    }
}

/// A visible layer (or part of one, split around live text) with its objects.
struct Slot<'a> {
    opacity: f32,
    objects: Vec<Obj<'a>>,
    /// Identifies a static layer's content, for the buffer cache.
    key: u64,
}

impl Slot<'_> {
    fn animated(&self) -> bool {
        self.objects.iter().any(Obj::animated)
    }

    fn indices(&self, t: Duration) -> Vec<usize> {
        self.objects.iter().map(|o| o.index_at(t)).collect()
    }

    /// Draws the objects at `t`, bottom to top, into a fresh transparent buffer.
    fn render(&mut self, t: Duration) -> Buf {
        RENDERS.with(|r| r.set(r.get() + 1));
        // the usual video layer: one opaque picture in its box is the buffer itself, with no drawing (not
        // for a picture in a group, or one its clip cuts)
        if let [obj] = &mut self.objects[..]
            && obj.object.opacity >= 1.0
            && obj.object.groups.is_empty()
            && let index = obj.index_at(t)
            && let ObjKind::Media { cursor, fill: Fill::None, .. } = &mut obj.kind
        {
            let frame = cursor.frame(index);
            let (x, y) = media_at(obj.object, frame.dimensions());
            let whole = (x, y, x + frame.width() as i64, y + frame.height() as i64);
            let uncut = clip(obj.object).is_none_or(|c| overlap(whole, c) == Some(whole));
            if uncut && frame.pixels().all(|p| p[3] == 255) {
                let Some((l, t, r, b)) = overlap(whole, PANEL) else { return Buf::empty() };
                let image = image::imageops::crop_imm(&*frame, (l - x) as u32, (t - y) as u32, (r - l) as u32, (b - t) as u32).to_image();
                return Buf { left: l as u32, top: t as u32, image };
            }
        }
        render_objects(&mut self.objects, t, 0, (0, 0), (WIDTH, HEIGHT))
    }

    /// A static layer's buffer, from the cache when its content hasn't changed.
    fn render_cached(&mut self) -> Arc<Buf> {
        static LAYERS: OnceLock<Mutex<(u64, HashMap<u64, (u64, Arc<Buf>)>)>> = OnceLock::new();
        let cache = LAYERS.get_or_init(Default::default);
        {
            let mut c = cache.lock().unwrap();
            c.0 += 1;
            let tick = c.0;
            if let Some((used, buf)) = c.1.get_mut(&self.key) {
                *used = tick;
                return buf.clone();
            }
        }
        let buf = Arc::new(self.render(Duration::ZERO));
        let mut c = cache.lock().unwrap();
        let tick = c.0;
        c.1.insert(self.key, (tick, buf.clone()));
        while c.1.len() > 1 && c.1.values().map(|(_, b)| b.bytes()).sum::<usize>() > LAYER_BYTES {
            let victim = c.1.iter().filter(|(k, _)| **k != self.key).min_by_key(|(_, (used, _))| *used).map(|(k, _)| *k);
            match victim {
                Some(k) => c.1.remove(&k),
                None => break,
            };
        }
        buf
    }
}

/// Draws `objs` at `t` into a transparent buffer whose top-left is `origin` on the panel and whose size is
/// `size`, opening and closing their groups below `depth` (the ones already open are the caller's).
fn render_objects(objs: &mut [Obj], t: Duration, depth: usize, origin: (i64, i64), size: (u32, u32)) -> Buf {
    let mut base = Canvas { image: RgbaImage::new(size.0, size.1), origin, touched: None };
    // the groups open now, outermost first, each with its own buffer
    let mut open: Vec<(Group, Canvas)> = Vec::new();
    for obj in objs {
        let groups = obj.object.groups.get(depth..).unwrap_or_default();
        // close the groups this object isn't in, innermost first; then open its own
        let kept = open.iter().zip(groups).take_while(|((g, _), h)| g.id == h.id).count();
        while open.len() > kept {
            close(&mut open, &mut base);
        }
        for g in &groups[kept..] {
            let canvas = Canvas::group(g, open.last().map_or(&base, |(_, c)| c));
            open.push((g.clone(), canvas));
        }
        open.last_mut().map_or(&mut base, |(_, c)| c).draw(obj, t);
    }
    while !open.is_empty() {
        close(&mut open, &mut base);
    }
    Buf::take(base.image, base.origin, base.touched)
}

/// Lays the innermost open group over what holds it, at the group's opacity.
fn close(open: &mut Vec<(Group, Canvas)>, base: &mut Canvas) {
    let Some((group, canvas)) = open.pop() else { return };
    let Some(touched) = canvas.touched else { return };
    let below = open.last_mut().map_or(base, |(_, c)| c);
    // a group's buffer lies on the panel (Canvas::group), so it is a Buf as it stands
    let buf = Buf { left: canvas.origin.0 as u32, top: canvas.origin.1 as u32, image: canvas.image };
    let laid = buf.onto_rgba(&mut below.image, below.origin, group.opacity).and_then(|r| overlap(r, touched));
    below.touched = union(below.touched, laid);
}

/// A transparent buffer being drawn into, whose top-left is `origin` on the panel, and the box (in panel
/// pixels) of what's been drawn into it.
struct Canvas {
    image: RgbaImage,
    origin: (i64, i64),
    touched: Option<(i64, i64, i64, i64)>,
}

impl Canvas {
    /// A group's buffer: its box, cut to the panel and to the buffer it will be laid over, since nothing
    /// beyond them can show. Empty when nothing is left.
    fn group(g: &Group, below: &Canvas) -> Canvas {
        let [l, t, r, b] = g.rect;
        let (l, t, r, b) = overlap((l, t, r, b), PANEL)
            .and_then(|rect| overlap(rect, extent(below.origin, &below.image)))
            .unwrap_or((0, 0, 0, 0));
        #[cfg(test)]
        GROUP_SIZES.with(|s| s.borrow_mut().push(((r - l) as u32, (b - t) as u32)));
        Canvas { image: RgbaImage::new((r - l) as u32, (b - t) as u32), origin: (l, t), touched: None }
    }

    /// Draws one object at `t`, inside its clip.
    fn draw(&mut self, obj: &mut Obj, t: Duration) {
        let index = obj.index_at(t);
        let object = obj.object;
        let rect = match &mut obj.kind {
            ObjKind::Media { cursor, fill, .. } => {
                let frame = cursor.frame(index);
                draw_media(&mut self.image, self.origin, object, &frame, *fill)
            }
            // the text's own layer covers its text (cut to its clip), with its own origin
            ObjKind::Text(w) => {
                let layer = text::Layer::render([*w]);
                layer.bounds().and_then(|b| {
                    let (left, top, image) = layer.into_parts();
                    let laid = Buf { left, top, image }.onto_rgba(&mut self.image, self.origin, 1.0)?;
                    overlap(laid, (b.left as i64, b.top as i64, b.right as i64, b.bottom as i64))
                })
            }
            ObjKind::Shape(s) => shape::draw(&mut self.image, self.origin, object, s),
        };
        self.touched = union(self.touched, rect);
    }
}

fn union(a: Option<(i64, i64, i64, i64)>, b: Option<(i64, i64, i64, i64)>) -> Option<(i64, i64, i64, i64)> {
    match (a, b) {
        (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3))),
        (a, None) => a,
        (None, b) => b,
    }
}

/// Where two pixel boxes (left, top, right, bottom) meet, if they do.
fn overlap(a: (i64, i64, i64, i64), b: (i64, i64, i64, i64)) -> Option<(i64, i64, i64, i64)> {
    let (l, t, r, b) = (a.0.max(b.0), a.1.max(b.1), a.2.min(b.2), a.3.min(b.3));
    (l < r && t < b).then_some((l, t, r, b))
}

/// The panel, as a pixel box.
const PANEL: (i64, i64, i64, i64) = (0, 0, WIDTH as i64, HEIGHT as i64);

/// The pixel box a buffer covers, with its top-left at `origin`.
fn extent(origin: (i64, i64), image: &RgbaImage) -> (i64, i64, i64, i64) {
    (origin.0, origin.1, origin.0 + image.width() as i64, origin.1 + image.height() as i64)
}

/// Where an object may draw, when it's part of a widget's copy.
fn clip(o: &Object) -> Option<(i64, i64, i64, i64)> {
    o.clip.map(|[l, t, r, b]| (l, t, r, b))
}

/// Where an image's frame goes on the panel: its box's top-left in whole pixels, or with `keep_aspect`, the
/// frame (the file fitted inside the box, as `slots` sized it) centred in the box (docs/widgets.md,
/// Images in a box).
fn media_at(o: &Object, frame: (u32, u32)) -> (i64, i64) {
    let (x, y) = (rnd(o.x), rnd(o.y));
    match &o.kind {
        Kind::Media(m) if m.keep_aspect => {
            let (bw, bh) = (rnd(o.w).max(1), rnd(o.h).max(1));
            (x + (bw - frame.0 as i64).div_euclid(2), y + (bh - frame.1 as i64).div_euclid(2))
        }
        _ => (x, y),
    }
}

/// The box an image or video is drawn at, in whole pixels (docs/widgets.md, Images in a box): `(x, y, w,
/// h)` is its placed box, and a file of `fw` x `fh` (after turning) is fitted inside it, keeping its shape,
/// and centred. The renderers share it, with the vectors in tests/widgets/aspect.json.
pub fn fit_aspect(x: i64, y: i64, w: i64, h: i64, fw: i64, fh: i64) -> (i64, i64, i64, i64) {
    if w <= 0 || h <= 0 || fw <= 0 || fh <= 0 {
        return (x, y, w, h);
    }
    let q = (w as f64 / fw as f64).min(h as f64 / fh as f64);
    let (vw, vh) = (rnd(fw as f64 * q).max(1), rnd(fh as f64 * q).max(1));
    (x + (w - vw).div_euclid(2), y + (h - vh).div_euclid(2), vw, vh)
}

/// Draws a turned and scaled frame at the object's box, repeated outward as its fill mode says, at the
/// object's opacity, into `canvas`, whose top-left is `origin` on the panel, inside the object's clip. The
/// repeats cover the clip, or the canvas when there's none; their pattern stays anchored to the object.
/// Returns the pixel box touched, in panel pixels.
fn draw_media(canvas: &mut RgbaImage, origin: (i64, i64), o: &Object, frame: &RgbaImage, fill: Fill) -> Option<(i64, i64, i64, i64)> {
    let (x, y) = media_at(o, frame.dimensions());
    let (w, h) = (frame.width() as i64, frame.height() as i64);
    let k = (o.opacity.clamp(0.0, 1.0) * 256.0).round() as u32;
    if w == 0 || h == 0 || k == 0 {
        return None;
    }
    // where it may draw: the canvas, cut to the clip
    let area = match clip(o) {
        Some(c) => overlap(extent(origin, canvas), c)?,
        None => extent(origin, canvas),
    };
    let (l, t, r, b) = match fill {
        Fill::None => overlap((x, y, x + w, y + h), area)?,
        _ => area,
    };
    if l >= r || t >= b {
        return None;
    }
    let (cl, ct) = origin;
    let map = |d: i64, n: i64| -> usize {
        match fill {
            Fill::None | Fill::Extend => d.clamp(0, n - 1) as usize,
            Fill::Tile => d.rem_euclid(n) as usize,
            Fill::Mirror => {
                let r = d.rem_euclid(n);
                (if d.div_euclid(n) % 2 != 0 { n - 1 - r } else { r }) as usize
            }
        }
    };
    let cols: Vec<usize> = (l..r).map(|px| map(px - x, w)).collect();
    let src: &[u8] = frame;
    for py in t..b {
        let row = map(py - y, h) * w as usize;
        for (i, px) in (l..r).enumerate() {
            let s = &src[(row + cols[i]) * 4..][..4];
            let a = (s[3] as u32 * k + 128) >> 8;
            if a == 0 {
                continue;
            }
            let d = canvas.get_pixel_mut((px - cl) as u32, (py - ct) as u32);
            if a >= 255 {
                *d = image::Rgba([s[0], s[1], s[2], 255]);
            } else {
                over_u8(d, s, a);
            }
        }
    }
    Some((l, t, r, b))
}

/// The scene's layers and the global ones, ready to draw, sorted around the live text (docs/widgets.md,
/// Live text): `below` goes into the prepared frames; `over` is laid over them while playing, its stills
/// not drawn yet. Live text itself is never in `below`.
struct Plan<'a> {
    below: Vec<Slot<'a>>,
    over: Vec<Node<'a>>,
    placement: Option<Placement>,
}

/// Where a scene's live text is drawn (docs/widgets.md, Live text).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    /// Over everything, since something animated is drawn from S upward; else in place.
    pub on_top: bool,
    /// The id of S's first object: where the group that the first live text sits in starts.
    pub cut: String,
}

/// Part of what's laid over the frames, before its stills are drawn.
enum Node<'a> {
    /// live_texts()[index], drawn at its own opacity × `fold`, inside its clip
    Text { index: usize, fold: f32 },
    /// an object that isn't live text
    Obj(Obj<'a>),
    /// a whole layer above S without live text, drawn (and cached) as any static layer is
    Slot(Slot<'a>),
    /// a translucent layer, copy or widget layer: its children drawn into a buffer of `rect`, laid over at
    /// `opacity`; `depth` of its objects' groups are open inside it
    Group { opacity: f32, rect: Rect, depth: usize, children: Vec<Node<'a>> },
}

impl<'a> Node<'a> {
    fn live(&self) -> bool {
        match self {
            Node::Text { .. } => true,
            Node::Group { children, .. } => children.iter().any(Node::live),
            Node::Obj(_) | Node::Slot(_) => false,
        }
    }

    /// The objects of a node without live text, in drawing order.
    fn objects_into(self, out: &mut Vec<Obj<'a>>) {
        match self {
            Node::Obj(obj) => out.push(obj),
            Node::Group { children, .. } => children.into_iter().for_each(|c| c.objects_into(out)),
            Node::Slot(slot) => out.extend(slot.objects),
            Node::Text { .. } => {}
        }
    }
}

/// The whole panel, as a group's rect.
const PANEL_RECT: Rect = [0, 0, WIDTH as i64, HEIGHT as i64];

/// Where two rects meet, or None when they don't.
fn meet(a: Rect, b: Rect) -> Option<Rect> {
    let r = [a[0].max(b[0]), a[1].max(b[1]), a[2].min(b[2]), a[3].min(b[3])];
    (r[0] < r[2] && r[1] < r[3]).then_some(r)
}

fn plan<'a>(scene: &'a Scene, global: &'a [Layer], opts: &Options) -> Plan<'a> {
    split_live(slots(scene, global, opts), opts)
}

/// One slot per visible layer, the scene's then the global ones, bottom first, each with its visible
/// objects ready to draw (an image or video that can't be opened is left out). Live text is still in them.
fn slots<'a>(scene: &'a Scene, global: &'a [Layer], opts: &Options) -> Vec<Slot<'a>> {
    let mut slots = Vec::new();
    for layer in scene.layers.iter().chain(global).filter(|l| l.visible) {
        let mut objects = Vec::new();
        for o in layer.objects.iter().filter(|o| o.visible) {
            let kind = match &o.kind {
                Kind::Media(m) => {
                    let source: Arc<Source> = match media::open(&m.src, &opts.ffmpeg) {
                        Ok(s) => s,
                        Err(e) => {
                            player::log(format_args!("skipping {}: {e}", o.id));
                            continue;
                        }
                    };
                    // its box in whole pixels; with keep_aspect, the file's own size (turned) fitted inside it
                    let (mut w, mut h) = (rnd(o.w).max(1), rnd(o.h).max(1));
                    if m.keep_aspect {
                        let (fw, fh) = if o.rotate % 180 == 90 { (source.height, source.width) } else { (source.width, source.height) };
                        (_, _, w, h) = fit_aspect(rnd(o.x), rnd(o.y), w, h, fw as i64, fh as i64);
                    }
                    let size = |v: i64| v.clamp(1, u32::MAX as i64) as u32;
                    let tint = m.tint.as_deref().and_then(shape::parse_rgb);
                    let view = View { rotate: o.rotate, flip: o.flip, w: size(w), h: size(h), smooth: m.smooth, tint };
                    let durations = source.durations();
                    let mut starts = Vec::with_capacity(durations.len());
                    let mut at = Duration::ZERO;
                    for d in &durations {
                        starts.push(at);
                        at += *d;
                    }
                    ObjKind::Media { cursor: Cursor::new(&m.src, &source, view), fill: m.fill, starts, loop_len: at.max(Duration::from_millis(1)) }
                }
                Kind::Text(w) => ObjKind::Text(w),
                Kind::Shape(s) => ObjKind::Shape(s),
            };
            objects.push(Obj { object: o, kind });
        }
        slots.push(Slot { opacity: layer.opacity, objects, key: 0 });
    }
    slots
}

/// Sorts the slots around the live text (docs/widgets.md, Live text), and keys the ones drawn as layers
/// for the buffer cache. Live texts are numbered in drawing order, as playlist::live_texts lists them.
///
/// P is the first live text, in slot `p`; S is where its group starts: the whole slot when the slot is
/// translucent, else the run of the slot's objects in P's outermost group, else P alone. With nothing
/// animated from S upward, every live text is drawn in place: what's below S goes into the frames, and
/// the rest becomes a tree of stills, live texts and the translucent groups that hold live text. Otherwise
/// every live text is drawn on top, at its opacity times its layer's and its groups'.
fn split_live<'a>(mut slots: Vec<Slot<'a>>, opts: &Options) -> Plan<'a> {
    let first = slots.iter().enumerate().find_map(|(i, s)| s.objects.iter().position(Obj::live).map(|j| (i, j)));
    let Some((p, j)) = first else {
        return keyed(Plan { below: slots, over: Vec::new(), placement: None }, opts);
    };
    let start = {
        let objects = &slots[p].objects;
        match objects[j].object.groups.first() {
            _ if slots[p].opacity < 1.0 => 0,
            // a copy's leaves are contiguous, so its group is the run around P
            Some(g) => (0..j).rev().take_while(|k| objects[*k].object.groups.first().is_some_and(|h| h.id == g.id)).last().unwrap_or(j),
            None => j,
        }
    };
    let on_top = slots[p].objects[start..].iter().any(Obj::animated) || slots[p + 1..].iter().any(Slot::animated);
    let placement = Some(Placement { on_top, cut: slots[p].objects[start].object.id.clone() });
    let mut over = Vec::new();
    if on_top {
        for slot in &mut slots {
            for obj in std::mem::take(&mut slot.objects) {
                if obj.live() {
                    let fold = obj.object.groups.iter().fold(slot.opacity, |f, g| f * g.opacity);
                    over.push(Node::Text { index: over.len(), fold });
                } else {
                    slot.objects.push(obj);
                }
            }
        }
        return keyed(Plan { below: slots, over, placement }, opts);
    }
    let later = slots.split_off(p + 1);
    let mut slot = slots.pop().unwrap();
    let upper = slot.objects.split_off(start);
    let mut index = 0;
    if slot.opacity < 1.0 {
        over.push(Node::Group { opacity: slot.opacity, rect: PANEL_RECT, depth: 0, children: nodes(upper, 0, &mut index) });
    } else {
        if !slot.objects.is_empty() {
            slots.push(slot); // what's below S, at the layer's opacity of 1
        }
        over.extend(nodes(upper, 0, &mut index));
    }
    for slot in later {
        if !slot.objects.iter().any(Obj::live) {
            over.push(Node::Slot(slot));
        } else if slot.opacity < 1.0 {
            over.push(Node::Group { opacity: slot.opacity, rect: PANEL_RECT, depth: 0, children: nodes(slot.objects, 0, &mut index) });
        } else {
            over.extend(nodes(slot.objects, 0, &mut index));
        }
    }
    keyed(Plan { below: slots, over, placement }, opts)
}

/// The plan with its layers keyed for the buffer cache.
fn keyed<'a>(mut plan: Plan<'a>, opts: &Options) -> Plan<'a> {
    fn key_slots(nodes: &mut [Node], opts: &Options) {
        for node in nodes {
            match node {
                Node::Slot(slot) => slot.key = slot_key(slot, opts),
                Node::Group { children, .. } => key_slots(children, opts),
                Node::Text { .. } | Node::Obj(_) => {}
            }
        }
    }
    for slot in &mut plan.below {
        slot.key = slot_key(slot, opts);
    }
    key_slots(&mut plan.over, opts);
    plan
}

/// Objects as nodes, with `depth` of their groups open already: each live text numbered in turn, and the
/// objects sharing a group at this depth gathered into it. A group off the panel shows nothing, but its
/// live texts keep their numbers.
fn nodes<'a>(objects: Vec<Obj<'a>>, depth: usize, index: &mut usize) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    let mut objects = objects.into_iter().peekable();
    while let Some(obj) = objects.next() {
        let object: &'a Object = obj.object;
        if let Some(group) = object.groups.get(depth) {
            let mut members = vec![obj];
            while let Some(o) = objects.next_if(|o| o.object.groups.get(depth).is_some_and(|g| g.id == group.id)) {
                members.push(o);
            }
            match meet(group.rect, PANEL_RECT) {
                Some(rect) => out.push(Node::Group { opacity: group.opacity, rect, depth: depth + 1, children: nodes(members, depth + 1, index) }),
                None => *index += members.iter().filter(|o| o.live()).count(),
            }
        } else if obj.live() {
            out.push(Node::Text { index: *index, fold: 1.0 });
            *index += 1;
        } else {
            out.push(Node::Obj(obj));
        }
    }
    out
}

/// The over list, with its stills drawn: each run of nodes without live text becomes one still, drawn
/// into a buffer of `rect` (the parent's) with `depth` of its objects' groups open already.
fn convert(nodes: Vec<Node>, depth: usize, rect: Rect) -> Vec<Over> {
    let mut over = Vec::new();
    let mut run = Vec::new();
    for node in nodes {
        if !node.live() {
            run.push(node);
            continue;
        }
        over.extend(still(std::mem::take(&mut run), depth, rect).map(Over::Still));
        match node {
            Node::Text { index, fold } => over.push(Over::Text { index, fold }),
            Node::Group { opacity, rect, depth, children } => over.push(Over::Group { opacity, rect, children: convert(children, depth, rect) }),
            Node::Obj(_) | Node::Slot(_) => {}
        }
    }
    over.extend(still(run, depth, rect).map(Over::Still));
    over
}

/// A run of nodes without live text drawn as one still, or None when it draws nothing. Objects are drawn
/// together; a whole layer comes from the layer cache, laid over at its opacity.
fn still(run: Vec<Node>, depth: usize, rect: Rect) -> Option<Arc<Buf>> {
    let origin = (rect[0], rect[1]);
    let size = ((rect[2] - rect[0]) as u32, (rect[3] - rect[1]) as u32);
    let mut parts: Vec<(Arc<Buf>, f32)> = Vec::new();
    let mut objects = Vec::new();
    let draw = |objects: &mut Vec<Obj>, parts: &mut Vec<(Arc<Buf>, f32)>| {
        if !objects.is_empty() {
            parts.push((Arc::new(render_objects(objects, Duration::ZERO, depth, origin, size)), 1.0));
            objects.clear();
        }
    };
    for node in run {
        match node {
            Node::Slot(mut slot) => {
                draw(&mut objects, &mut parts);
                parts.push((slot.render_cached(), slot.opacity));
            }
            node => node.objects_into(&mut objects),
        }
    }
    draw(&mut objects, &mut parts);
    parts.retain(|(buf, opacity)| !buf.is_empty() && *opacity > 0.0);
    match &parts[..] {
        [] => None,
        [(buf, opacity)] if *opacity >= 1.0 => Some(buf.clone()),
        _ => {
            let mut canvas = RgbaImage::new(size.0, size.1);
            let mut touched = None;
            for (buf, opacity) in &parts {
                touched = union(touched, buf.onto_rgba(&mut canvas, origin, *opacity));
            }
            touched?;
            Some(Arc::new(if rect == PANEL_RECT {
                Buf::take(canvas, (0, 0), touched)
            } else {
                Buf { left: rect[0] as u32, top: rect[1] as u32, image: canvas }
            }))
        }
    }
}

/// The over list baked with `texts` (the live text, filled in) into one buffer to lay over the frames:
/// stills laid over as they are, each text drawn at its opacity × fold inside its clip, and each group
/// through a buffer of its rect, laid over at its opacity (docs/widgets.md, Live text). A text whose
/// index isn't in `texts` is skipped.
pub(crate) fn bake(over: &[Over], texts: &[Text]) -> Buf {
    fn into(over: &[Over], texts: &[Text], canvas: &mut RgbaImage, origin: (i64, i64)) -> Option<(i64, i64, i64, i64)> {
        let mut touched = None;
        for node in over {
            let drawn = match node {
                Over::Still(buf) => buf.onto_rgba(canvas, origin, 1.0),
                Over::Text { index, fold } => {
                    texts.get(*index).and_then(|w| text_buf(&Text { opacity: w.opacity * fold, ..w.clone() }).onto_rgba(canvas, origin, 1.0))
                }
                Over::Group { opacity, rect, children } => meet(*rect, PANEL_RECT).and_then(|[l, t, r, b]| {
                    let mut group = RgbaImage::new((r - l) as u32, (b - t) as u32);
                    into(children, texts, &mut group, (l, t))?;
                    Buf { left: l as u32, top: t as u32, image: group }.onto_rgba(canvas, origin, *opacity)
                }),
            };
            touched = union(touched, drawn);
        }
        touched
    }
    if over.is_empty() {
        return Buf::empty();
    }
    let mut canvas = RgbaImage::new(WIDTH, HEIGHT);
    let touched = into(over, texts, &mut canvas, (0, 0));
    Buf::take(canvas, (0, 0), touched)
}

/// One text drawn on its own, cut to its clip: the part of the panel it touches.
fn text_buf(w: &Text) -> Buf {
    let layer = text::Layer::render([w]);
    let Some(b) = layer.bounds() else { return Buf::empty() };
    let drawn = [b.left as i64, b.top as i64, b.right as i64, b.bottom as i64];
    let rect = meet(drawn, w.clip.unwrap_or(PANEL_RECT)).map(|[l, t, r, b]| (l, t, r, b));
    let (left, top, image) = layer.into_parts();
    Buf::crop(&image, (left as i64, top as i64), rect)
}

/// Where the scene's live text is drawn (docs/widgets.md, Live text), or None when it has none.
pub fn placement(scene: &Scene, global: &[Layer], opts: &Options) -> Option<Placement> {
    plan(scene, global, opts).placement
}

/// Identifies a static layer's drawing: its objects and the files they use.
fn slot_key(slot: &Slot, opts: &Options) -> u64 {
    let mut h = DefaultHasher::new();
    (VERSION, text::VERSION).hash(&mut h);
    for obj in &slot.objects {
        serde_json::to_string(obj.object).unwrap_or_default().hash(&mut h);
        match &obj.object.kind {
            Kind::Media(m) => (m.src.as_path(), stamp(&m.src)).hash(&mut h),
            Kind::Text(w) => (w.font.as_path(), stamp(&w.font), stamp(Path::new(DEFAULT_FONT))).hash(&mut h),
            Kind::Shape(_) => {}
        }
    }
    opts.ffmpeg.hash(&mut h);
    h.finish()
}

/// Identifies a scene's prepared frames: its JSON (with live text blanked, since that's filled in while
/// playing) plus the global layers, the stamps of every file they use, and the encoding options.
pub fn key(scene: &Scene, global: &[Layer], opts: &Options) -> u64 {
    let blank = |layers: &[Layer]| -> Vec<Layer> {
        layers
            .iter()
            .map(|l| Layer {
                objects: l
                    .objects
                    .iter()
                    .map(|o| match &o.kind {
                        Kind::Text(w) if w.live => Object { kind: Kind::Text(Text { text: String::new(), ..w.clone() }), ..o.clone() },
                        _ => o.clone(),
                    })
                    .collect(),
                ..l.clone()
            })
            .collect()
    };
    let mut h = DefaultHasher::new(); // fixed keys, so the same scene hashes the same every run
    (VERSION, text::VERSION, opts.rotate180, opts.quality).hash(&mut h);
    let scene = Scene { layers: blank(&scene.layers), ..scene.clone() };
    serde_json::to_string(&scene).unwrap_or_default().hash(&mut h);
    serde_json::to_string(&blank(global)).unwrap_or_default().hash(&mut h);
    let global = Scene { id: String::new(), name: String::new(), seconds: 0.0, background: String::new(), layers: global.to_vec() };
    for s in [&scene, &global] {
        for path in s.media().chain(s.fonts()).chain([Path::new(DEFAULT_FONT)]) {
            (path, stamp(path)).hash(&mut h);
        }
    }
    h.finish()
}

/// When the frames fall within one cycle, each with how long it shows: one still frame when nothing
/// animates, else the union of every animated object's frame changes over the loops' least common
/// multiple (capped), with frames at least MIN_FRAME apart.
fn timeline(slots: &[Slot]) -> Vec<(Duration, Duration)> {
    let loops: Vec<(&Vec<Duration>, Duration)> = slots
        .iter()
        .flat_map(|s| &s.objects)
        .filter_map(|o| match &o.kind {
            ObjKind::Media { starts, loop_len, .. } if starts.len() > 1 => Some((starts, *loop_len)),
            _ => None,
        })
        .collect();
    if loops.is_empty() {
        return vec![(Duration::ZERO, Duration::from_secs(1))];
    }
    let cycle = cycle(loops.iter().map(|(_, l)| *l));
    let mut times: Vec<Duration> = Vec::new();
    for (starts, loop_len) in &loops {
        let mut base = Duration::ZERO;
        while base < cycle {
            times.extend(starts.iter().map(|s| base + *s).take_while(|t| *t < cycle));
            base += *loop_len;
        }
    }
    times.sort();
    times.dedup();
    let mut kept: Vec<Duration> = Vec::new();
    for t in times {
        if kept.last().is_none_or(|last| t - *last >= MIN_FRAME) {
            kept.push(t);
        }
    }
    (0..kept.len()).map(|i| (kept[i], kept.get(i + 1).copied().unwrap_or(cycle) - kept[i])).collect()
}

/// The least common multiple of the loop lengths, capped at MAX_CYCLE.
pub fn cycle(loops: impl Iterator<Item = Duration>) -> Duration {
    fn gcd(a: u128, b: u128) -> u128 {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let cap = MAX_CYCLE.as_micros();
    let mut lcm: u128 = 1;
    for l in loops {
        let l = l.as_micros().max(1);
        lcm = lcm / gcd(lcm, l) * l;
        if lcm >= cap {
            return MAX_CYCLE;
        }
    }
    Duration::from_micros(lcm as u64)
}

/// The RGB canvas with the background colour.
fn background(scene: &Scene) -> RgbImage {
    let c = shape::rgb(&scene.background);
    RgbImage::from_pixel(WIDTH, HEIGHT, image::Rgb(c))
}

/// One frame waiting to be composed and encoded: the shared base, then the layers over it.
struct Composite {
    base: Arc<RgbImage>,
    layers: Vec<(Arc<Buf>, f32)>,
    duration: Duration,
}

impl Composite {
    fn compose(self) -> (RgbImage, Duration) {
        let mut frame = (*self.base).clone();
        for (buf, opacity) in &self.layers {
            buf.onto_rgb(&mut frame, *opacity, false);
        }
        (frame, self.duration)
    }
}

/// What sits over the base in a prepared frame: a static run flattened once, or an animated layer
/// drawn when its frames change.
enum Segment {
    Still(Arc<Buf>),
    Animated { slot: usize, last: Option<(Vec<usize>, Arc<Buf>)> },
}

/// Flattens a run of static slots into one buffer.
fn flatten(slots: &mut [Slot]) -> Buf {
    let mut canvas = RgbaImage::new(WIDTH, HEIGHT);
    let mut touched = None;
    for slot in slots {
        let buf = slot.render_cached();
        touched = union(touched, buf.onto_rgba(&mut canvas, (0, 0), slot.opacity));
    }
    Buf::take(canvas, (0, 0), touched)
}

/// The scene's cycle of frames, encoded for the panel, plus what's laid over them with its live text.
pub fn prepare(scene: &Scene, global: &[Layer], opts: &Options) -> Result<Prepared, String> {
    let mut plan = plan(scene, global, opts);
    let times = timeline(&plan.below);
    // the base: the background and every static layer under the first animated one
    let mut base = background(scene);
    let first_animated = plan.below.iter().position(Slot::animated).unwrap_or(plan.below.len());
    for slot in &mut plan.below[..first_animated] {
        slot.render_cached().onto_rgb(&mut base, slot.opacity, false);
    }
    let base = Arc::new(base);
    // above it: static runs flattened once, animated layers on their own
    let mut segments = Vec::new();
    let mut i = first_animated;
    while i < plan.below.len() {
        if plan.below[i].animated() {
            segments.push(Segment::Animated { slot: i, last: None });
            i += 1;
        } else {
            let end = (i..plan.below.len()).find(|j| plan.below[*j].animated()).unwrap_or(plan.below.len());
            segments.push(Segment::Still(Arc::new(flatten(&mut plan.below[i..end]))));
            i = end;
        }
    }
    let sampling = art::sampling(scene, global);
    let mut frames = Vec::with_capacity(times.len());
    let mut batch: Vec<Composite> = Vec::with_capacity(BATCH);
    for (t, duration) in times {
        let mut layers = Vec::with_capacity(segments.len());
        for seg in &mut segments {
            match seg {
                Segment::Still(buf) => layers.push((buf.clone(), 1.0)),
                Segment::Animated { slot, last } => {
                    let s = &mut plan.below[*slot];
                    let indices = s.indices(t);
                    let buf = match last {
                        Some((seen, buf)) if *seen == indices => buf.clone(),
                        _ => {
                            let buf = Arc::new(s.render(t));
                            *last = Some((indices, buf.clone()));
                            buf
                        }
                    };
                    layers.push((buf, s.opacity));
                }
            }
        }
        batch.push(Composite { base: base.clone(), layers, duration });
        if batch.len() == BATCH {
            frames.extend(art::encode_all(std::mem::take(&mut batch), Composite::compose, opts, sampling)?);
        }
    }
    frames.extend(art::encode_all(batch, Composite::compose, opts, sampling)?);
    Ok(Prepared { frames, over: convert(plan.over, 0, PANEL_RECT) })
}

/// The scene at time `t`, upright, with `live` (its live text, filled in, as playlist::live_texts lists
/// it) drawn where it belongs: what the cooler would show, for checking without it.
pub fn render_at(scene: &Scene, global: &[Layer], opts: &Options, t: Duration, live: &[Text]) -> RgbImage {
    let mut plan = plan(scene, global, opts);
    let mut frame = background(scene);
    for slot in &mut plan.below {
        let buf = if slot.animated() { Arc::new(slot.render(t)) } else { slot.render_cached() };
        buf.onto_rgb(&mut frame, slot.opacity, false);
    }
    bake(&convert(plan.over, 0, PANEL_RECT), live).onto_rgb(&mut frame, 1.0, false);
    frame
}

/// The whole cycle's frames with their times, for tests and tools.
pub fn times(scene: &Scene, global: &[Layer], opts: &Options) -> Vec<(Duration, Duration)> {
    timeline(&plan(scene, global, opts).below)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playlist::{self, Playlist};
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Rgb, Rgba};
    use std::path::PathBuf;

    /// A folder with small test media: `two.png` (red, green side by side), `a.gif` (3 frames of 100 ms)
    /// and `b.gif` (2 frames of 150 ms).
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("wvplay-scene-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let mut two = RgbaImage::new(2, 1);
            two.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
            two.put_pixel(1, 0, Rgba([0, 255, 0, 255]));
            two.save(dir.join("two.png")).unwrap();
            for (name, shades, ms) in [("a.gif", vec![40u8, 80, 120], 100), ("b.gif", vec![200, 250], 150)] {
                let mut gif = GifEncoder::new(std::fs::File::create(dir.join(name)).unwrap());
                for shade in shades {
                    let im = RgbaImage::from_pixel(4, 4, Rgba([shade, 0, 0, 255]));
                    gif.encode_frame(image::Frame::from_parts(im, 0, 0, Delay::from_numer_denom_ms(ms, 1))).unwrap();
                }
            }
            Scratch { dir }
        }

        fn playlist(&self, scenes: &str, global: &str) -> Playlist {
            let json = format!(r#"{{ "version": 2, "global": {{ "layers": [{global}] }}, "scenes": [{scenes}] }}"#);
            playlist::parse(json.as_bytes(), &self.dir, 30.0, Path::new("ffmpeg")).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn opts() -> Options {
        Options { rotate180: false, quality: 95, ffmpeg: PathBuf::from("ffmpeg") }
    }

    fn rect(id: &str, x: i32, y: i32, w: i32, h: i32, color: &str, opacity: f32) -> String {
        format!(r##"{{ "id": "{id}", "type": "shape", "shape": "rect", "x": {x}, "y": {y}, "w": {w}, "h": {h}, "opacity": {opacity}, "crisp": true, "fill": {{ "type": "solid", "color": "{color}" }} }}"##)
    }

    fn layer(id: &str, opacity: f32, objects: &[String]) -> String {
        format!(r#"{{ "id": "{id}", "opacity": {opacity}, "objects": [{}] }}"#, objects.join(","))
    }

    fn scene(layers: &[String]) -> String {
        format!(r##"{{ "id": "s", "background": "#000000", "layers": [{}] }}"##, layers.join(","))
    }

    fn at(p: &Playlist, t: u64) -> RgbImage {
        render_at(&p.scenes[0], &p.global, &opts(), Duration::from_millis(t), &[])
    }

    /// A layer's opacity applies to it as a whole: where its objects overlap, the lower one doesn't
    /// show through. An object's own opacity does let things through.
    #[test]
    fn layer_opacity_is_a_group() {
        let s = Scratch::new("group");
        let red = rect("r", 10, 10, 20, 20, "#ff0000", 1.0);
        let blue = rect("b", 20, 10, 20, 20, "#0000ff", 1.0);
        let p = s.playlist(&scene(&[layer("l", 0.5, &[red.clone(), blue.clone()])]), "");
        let im = at(&p, 0);
        assert_eq!(im.get_pixel(25, 15), &Rgb([0, 0, 128])); // overlap: only blue, at half
        assert_eq!(im.get_pixel(15, 15), &Rgb([128, 0, 0]));
        assert_eq!(im.get_pixel(5, 5), &Rgb([0, 0, 0]));
        // the same two objects, the top one at half opacity in a full layer: red shows through
        let half_blue = rect("b", 20, 10, 20, 20, "#0000ff", 0.5);
        let p = s.playlist(&scene(&[layer("l", 1.0, &[red, half_blue])]), "");
        let im = at(&p, 0);
        assert_eq!(im.get_pixel(25, 15), &Rgb([128, 0, 128]));
        assert_eq!(im.get_pixel(35, 15), &Rgb([0, 0, 128]));
        assert_eq!(im.get_pixel(15, 15), &Rgb([255, 0, 0]));
    }

    /// Images reach past their box as their fill says, across the whole canvas.
    #[test]
    fn fill_modes() {
        let s = Scratch::new("fill");
        let image = |fill: &str| format!(r#"{{ "id": "i", "type": "image", "src": "two.png", "x": 10, "y": 5, "w": 2, "h": 1, "fill": "{fill}" }}"#);
        let red = Rgb([255, 0, 0]);
        let green = Rgb([0, 255, 0]);
        let black = Rgb([0, 0, 0]);
        let row = |p: &Playlist, xs: &[u32]| -> Vec<Rgb<u8>> {
            let im = at(p, 0);
            xs.iter().map(|x| *im.get_pixel(*x, 5)).collect()
        };
        let p = s.playlist(&scene(&[layer("l", 1.0, &[image("none")])]), "");
        assert_eq!(row(&p, &[9, 10, 11, 12]), [black, red, green, black]);
        assert_eq!(at(&p, 0).get_pixel(10, 4), &black);
        let p = s.playlist(&scene(&[layer("l", 1.0, &[image("extend")])]), "");
        assert_eq!(row(&p, &[0, 9, 10, 11, 12, 1599]), [red, red, red, green, green, green]);
        assert_eq!(at(&p, 0).get_pixel(10, 719), &red); // rows stretch too
        let p = s.playlist(&scene(&[layer("l", 1.0, &[image("tile")])]), "");
        assert_eq!(row(&p, &[8, 9, 10, 11, 12, 13]), [red, green, red, green, red, green]);
        let p = s.playlist(&scene(&[layer("l", 1.0, &[image("mirror")])]), "");
        assert_eq!(row(&p, &[8, 9, 10, 11, 12, 13, 14]), [green, red, red, green, green, red, red]);
    }

    /// The cycle is the loops' least common multiple, capped at a minute, and frames fall wherever any
    /// object's frame changes; one animation gives exactly its own frames.
    #[test]
    fn timeline_follows_the_loops() {
        let ms = Duration::from_millis;
        assert_eq!(cycle([ms(300), ms(200)].into_iter()), ms(600));
        assert_eq!(cycle([ms(100)].into_iter()), ms(100));
        assert_eq!(cycle([Duration::from_micros(33333), ms(100)].into_iter()), MAX_CYCLE);
        assert_eq!(cycle([ms(7000), ms(11000)].into_iter()), MAX_CYCLE); // 77 s, capped
        let s = Scratch::new("timeline");
        let gif = |id: &str, name: &str| format!(r#"{{ "id": "{id}", "type": "image", "src": "{name}", "w": 4, "h": 4 }}"#);
        let p = s.playlist(&scene(&[layer("l", 1.0, &[gif("a", "a.gif")])]), "");
        let times = times(&p.scenes[0], &p.global, &opts());
        assert_eq!(times, [(ms(0), ms(100)), (ms(100), ms(100)), (ms(200), ms(100))]);
        let p = s.playlist(&scene(&[layer("l", 1.0, &[gif("a", "a.gif"), gif("b", "b.gif")])]), "");
        let times = super::times(&p.scenes[0], &p.global, &opts());
        assert_eq!(times, [(ms(0), ms(100)), (ms(100), ms(50)), (ms(150), ms(50)), (ms(200), ms(100))]);
        // a still scene is one frame
        let p = s.playlist(&scene(&[layer("l", 1.0, &[rect("r", 0, 0, 5, 5, "#ffffff", 1.0)])]), "");
        assert_eq!(super::times(&p.scenes[0], &p.global, &opts()).len(), 1);
        // the frames show the right animation frames: b's second frame from 150 ms
        let p = s.playlist(&scene(&[layer("l", 1.0, &[gif("b", "b.gif")])]), "");
        assert_eq!(at(&p, 0).get_pixel(0, 0), &Rgb([200, 0, 0]));
        assert_eq!(at(&p, 149).get_pixel(0, 0), &Rgb([200, 0, 0]));
        assert_eq!(at(&p, 150).get_pixel(0, 0), &Rgb([250, 0, 0]));
        assert_eq!(at(&p, 300).get_pixel(0, 0), &Rgb([200, 0, 0])); // looped
    }

    /// Live text sits at its true place when only still layers are above it: those are kept as a still
    /// over it, out of the prepared frames. With animation above, it goes on top instead.
    #[test]
    fn live_text_keeps_its_place() {
        if !Path::new(DEFAULT_FONT).exists() {
            return;
        }
        let s = Scratch::new("live");
        let live = r#"{ "id": "t", "type": "text", "text": "{time}", "x": 100, "y": 100, "size": 60 }"#.to_string();
        let below = layer("below", 1.0, &[rect("r", 0, 0, 50, 50, "#ff0000", 1.0)]);
        let cover = layer("cover", 1.0, &[rect("c", 90, 90, 400, 100, "#00ff00", 1.0)]);
        let p = s.playlist(&scene(&[below.clone(), layer("live", 1.0, &[live.clone()]), cover.clone()]), "");
        let prepared = prepare(&p.scenes[0], &p.global, &opts()).unwrap();
        assert_eq!(prepared.frames.len(), 1);
        let [Over::Text { index: 0, fold: 1.0 }, Over::Still(above)] = &prepared.over[..] else {
            panic!("the cover should be kept as a still over the text: {:?}", prepared.over)
        };
        assert_eq!((above.left, above.top, above.image.dimensions()), (90, 90, (400, 100)));
        let frame = image::load_from_memory(&prepared.frames[0].jpeg).unwrap().into_rgb8();
        assert!(frame.get_pixel(200, 120)[1] < 50, "the cover was drawn into the frames");
        assert!(frame.get_pixel(10, 10)[0] > 200);
        // drawn where it belongs: under the cover
        let filled = Text { text: "12:34".into(), x: 100.0, y: 100.0, size: 60.0, kind: "text".into(), ..Text::default() };
        let im = render_at(&p.scenes[0], &p.global, &opts(), Duration::ZERO, &[filled.clone()]);
        assert_eq!(im.get_pixel(200, 120), &Rgb([0, 255, 0]));
        // the global layer counts as above too
        let p = s.playlist(&scene(&[below.clone(), layer("live", 1.0, &[live.clone()])]), &cover);
        let prepared = prepare(&p.scenes[0], &p.global, &opts()).unwrap();
        assert!(matches!(&prepared.over[..], [Over::Text { .. }, Over::Still(_)]));
        // an animation above it: nothing kept, the text goes on top
        let gif = r#"{ "id": "g", "type": "image", "src": "a.gif", "x": 90, "y": 90, "w": 400, "h": 100 }"#.to_string();
        let p = s.playlist(&scene(&[below, layer("live", 1.0, &[live]), layer("anim", 1.0, &[gif])]), "");
        let prepared = prepare(&p.scenes[0], &p.global, &opts()).unwrap();
        assert_eq!(prepared.over, [Over::Text { index: 0, fold: 1.0 }]);
        assert_eq!(prepared.frames.len(), 3);
        let im = render_at(&p.scenes[0], &p.global, &opts(), Duration::ZERO, &[filled]);
        assert!(im.enumerate_pixels().any(|(x, y, p)| (90..490).contains(&x) && (90..190).contains(&y) && p[1] > 200), "text not on top");
    }

    /// Keys change with the scene, the global layers, the files and the settings, but not with live
    /// text's current words.
    #[test]
    fn cache_keys() {
        let s = Scratch::new("keys");
        let img = r#"{ "id": "i", "type": "image", "src": "two.png", "w": 20, "h": 10 }"#.to_string();
        let base = s.playlist(&scene(&[layer("l", 1.0, &[img.clone()])]), "");
        let k = |p: &Playlist, o: &Options| key(&p.scenes[0], &p.global, o);
        let same = s.playlist(&scene(&[layer("l", 1.0, &[img.clone()])]), "");
        assert_eq!(k(&base, &opts()), k(&same, &opts()));
        let moved = s.playlist(&scene(&[layer("l", 1.0, &[img.replace("\"w\": 20", "\"w\": 21")])]), "");
        assert_ne!(k(&base, &opts()), k(&moved, &opts()));
        let with_global = s.playlist(&scene(&[layer("l", 1.0, &[img.clone()])]), &layer("g", 1.0, &[rect("r", 0, 0, 5, 5, "#fff", 1.0)]));
        assert_ne!(k(&base, &opts()), k(&with_global, &opts()));
        assert_ne!(k(&base, &opts()), k(&base, &Options { quality: 80, ..opts() }));
        assert_ne!(k(&base, &opts()), k(&base, &Options { rotate180: true, ..opts() }));
        // live text: its presence and place count, its words don't
        let live = |t: &str| format!(r#"{{ "id": "t", "type": "text", "text": "{t}", "x": 1 }}"#);
        let a = s.playlist(&scene(&[layer("l", 1.0, &[img.clone(), live("{time}")])]), "");
        let b = s.playlist(&scene(&[layer("l", 1.0, &[img.clone(), live("{pc.cpu.load}")])]), "");
        let c = s.playlist(&scene(&[layer("l", 1.0, &[img.clone(), live("{time}").replace("\"x\": 1", "\"x\": 2")])]), "");
        assert_eq!(k(&a, &opts()), k(&b, &opts()));
        assert_ne!(k(&a, &opts()), k(&c, &opts()));
        assert_ne!(k(&a, &opts()), k(&base, &opts()));
        // the file replaced
        let before = k(&base, &opts());
        std::thread::sleep(Duration::from_millis(20));
        RgbaImage::new(3, 1).save(s.dir.join("two.png")).unwrap();
        assert_ne!(k(&base, &opts()), before);
    }

    /// Prepare times for the scenes of the playlist in $WV_TIMING_PLAYLIST, cold and then with the
    /// static layers cached: `cargo test timing -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn timing() {
        let Some(path) = std::env::var_os("WV_TIMING_PLAYLIST") else { return };
        let ffmpeg = std::env::var_os("WVPLAY_FFMPEG").map_or(PathBuf::from("ffmpeg"), PathBuf::from);
        let p = playlist::read(Path::new(&path), 30.0, &ffmpeg).unwrap();
        let o = Options { rotate180: true, quality: 95, ffmpeg };
        for scene in &p.scenes {
            let mut took = Vec::new();
            for _ in 0..3 {
                let before = RENDERS.with(Cell::get);
                let start = std::time::Instant::now();
                let frames = prepare(scene, &p.global, &o).unwrap().frames.len();
                took.push(format!("{} ms ({} layer draws, {frames} frames)", start.elapsed().as_millis(), RENDERS.with(Cell::get) - before));
            }
            println!("{}: {}", scene.name, took.join(", then "));
        }
    }

    /// Only layers whose content changes are drawn: static ones once per edit of their own content,
    /// animated ones only at the moments their frames change.
    #[test]
    fn layers_are_drawn_only_when_they_change() {
        let s = Scratch::new("renders");
        let gif = |id: &str, name: &str| format!(r#"{{ "id": "{id}", "type": "image", "src": "{name}", "w": 4, "h": 4 }}"#);
        let bottom = layer("bottom", 1.0, &[rect("r1", 0, 0, 50, 50, "#ff0000", 1.0)]);
        let anim = layer("anim", 1.0, &[gif("a", "a.gif")]);
        let top = |colour: &str| layer("top", 1.0, &[rect("r2", 100, 0, 50, 50, colour, 1.0)]);
        let count = |f: &dyn Fn()| {
            let before = RENDERS.with(Cell::get);
            f();
            RENDERS.with(Cell::get) - before
        };
        let p = s.playlist(&scene(&[bottom.clone(), anim.clone(), top("#00ff00")]), "");
        let o = opts();
        // first time: both static layers, and the animation's three frames
        assert_eq!(count(&|| { prepare(&p.scenes[0], &p.global, &o).unwrap(); }), 2 + 3);
        // again: the static layers come from the cache
        assert_eq!(count(&|| { prepare(&p.scenes[0], &p.global, &o).unwrap(); }), 3);
        // the top layer edited: only it is drawn again
        let p2 = s.playlist(&scene(&[bottom.clone(), anim.clone(), top("#0000ff")]), "");
        assert_eq!(count(&|| { prepare(&p2.scenes[0], &p2.global, &o).unwrap(); }), 1 + 3);
        // two animations in their own layers: each drawn only when its own frame changes (3 + 2 over
        // the 4 frames of the cycle, not 4 + 4)
        let p3 = s.playlist(&scene(&[bottom, layer("a", 1.0, &[gif("a", "a.gif")]), layer("b", 1.0, &[gif("b", "b.gif")])]), "");
        let frames = count(&|| { assert_eq!(prepare(&p3.scenes[0], &p3.global, &o).unwrap().frames.len(), 4); });
        assert_eq!(frames, 3 + 2);
    }
}

/// Groups, clips, rounding, keep_aspect and tint (docs/widgets.md, Drawing). Owned by work package P2.
///
/// The leaves of copies are built by hand here, as the reader would make them: ordinary objects with a
/// clip and the groups they sit in.
#[cfg(test)]
mod drawing_tests {
    use super::*;
    use crate::playlist::{self, Playlist, Rect};
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Rgba};
    use jpeg_encoder::SamplingFactor;
    use serde_json::{Value, json};
    use std::path::PathBuf;

    /// A folder of its own for each test's media.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("wvplay-drawing-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch { dir }
        }

        fn png(&self, name: &str, w: u32, h: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) {
            RgbaImage::from_fn(w, h, |x, y| Rgba(pixel(x, y))).save(self.dir.join(name)).unwrap();
        }

        /// `two.png`: red, green side by side.
        fn two(&self) {
            self.png("two.png", 2, 1, |x, _| if x == 0 { [255, 0, 0, 255] } else { [0, 255, 0, 255] });
        }

        /// Scenes of the given layers, bottom first, each a list of objects at an opacity.
        fn playlist(&self, scenes: &[&[(f64, Vec<Value>)]], global: &[(f64, Vec<Value>)]) -> Playlist {
            let layers = |list: &[(f64, Vec<Value>)]| -> Vec<Value> {
                list.iter().enumerate().map(|(i, (opacity, objects))| json!({ "id": format!("l{i}"), "opacity": opacity, "objects": objects })).collect()
            };
            let scenes: Vec<Value> = scenes.iter().enumerate()
                .map(|(i, l)| json!({ "id": format!("s{i}"), "background": "#000000", "layers": layers(l) }))
                .collect();
            let file = json!({ "version": 2, "global": { "layers": layers(global) }, "scenes": scenes });
            playlist::parse(file.to_string().as_bytes(), &self.dir, 30.0, Path::new("ffmpeg")).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn opts() -> Options {
        Options { rotate180: false, quality: 95, ffmpeg: PathBuf::from("ffmpeg") }
    }

    fn rect(x: f64, y: f64, w: f64, h: f64, color: &str) -> Value {
        json!({ "id": "r", "type": "shape", "x": x, "y": y, "w": w, "h": h, "crisp": true, "fill": { "type": "solid", "color": color } })
    }

    fn image(src: &str, x: f64, y: f64, w: f64, h: f64) -> Value {
        json!({ "id": "i", "type": "image", "src": src, "x": x, "y": y, "w": w, "h": h })
    }

    /// Sets a key on an object made by `rect` or `image`.
    fn with(mut o: Value, key: &str, value: Value) -> Value {
        o[key] = value;
        o
    }

    /// Makes objects the leaves of a copy, as the reader does: each inside `clip`, in `groups` (outermost
    /// first, as `(id, opacity, rect)`).
    fn leaves(objects: &mut [Object], clip: Rect, groups: &[(&str, f32, Rect)]) {
        for o in objects {
            o.clip = Some(clip);
            o.groups = groups.iter().map(|(id, opacity, rect)| Group { id: id.to_string(), opacity: *opacity, rect: *rect }).collect();
            if let Kind::Text(t) = &mut o.kind {
                t.clip = Some(clip);
            }
        }
    }

    fn media(o: &mut Object) -> &mut playlist::Media {
        match &mut o.kind {
            Kind::Media(m) => m,
            _ => panic!("{} isn't an image", o.id),
        }
    }

    /// Scene `i` as the cooler shows it at `t`.
    fn draw_at(p: &Playlist, i: usize, t: u64) -> RgbImage {
        render_at(&p.scenes[i], &p.global, &opts(), Duration::from_millis(t), &[])
    }

    fn draw(p: &Playlist) -> RgbImage {
        draw_at(p, 0, 0)
    }

    /// Layer `i` of the first scene drawn afresh (not from the layer cache), and the group buffers that took.
    fn layer_buf(p: &Playlist, i: usize) -> (Buf, Vec<(u32, u32)>) {
        GROUP_SIZES.with(|s| s.take());
        let buf = slots(&p.scenes[0], &p.global, &opts())[i].render(Duration::ZERO);
        (buf, GROUP_SIZES.with(|s| s.take()))
    }

    fn px(im: &RgbImage, x: u32, y: u32) -> [u8; 3] {
        im.get_pixel(x, y).0
    }

    /// A copy below 100% is drawn as a whole, like a layer: where its parts overlap, the lower one doesn't
    /// show through. Its buffer covers its box, not the panel.
    #[test]
    fn copy_opacity_is_a_group() {
        let s = Scratch::new("copy-group");
        let parts = vec![rect(10.0, 10.0, 20.0, 20.0, "#ff0000"), rect(20.0, 10.0, 20.0, 20.0, "#0000ff")];
        let alone = rect(50.0, 10.0, 10.0, 10.0, "#00ff00");
        let mut p = s.playlist(&[&[(1.0, vec![parts[0].clone(), parts[1].clone(), alone.clone()])]], &[]);
        leaves(&mut p.scenes[0].layers[0].objects[..2], [5, 5, 45, 40], &[("c", 0.5, [5, 5, 45, 40])]);
        let im = draw(&p);
        assert_eq!(px(&im, 25, 15), [0, 0, 128]); // overlap: only blue, at half
        assert_eq!(px(&im, 15, 15), [128, 0, 0]);
        assert_eq!(px(&im, 55, 15), [0, 255, 0]); // not in the copy: as it is
        // exactly what a layer at 0.5 holding the two draws
        let layered = s.playlist(&[&[(0.5, parts.clone()), (1.0, vec![alone.clone()])]], &[]);
        assert!(draw(&layered) == im);
        assert_eq!(layer_buf(&p, 0).1, [(40, 35)]);
        // an object's own opacity still multiplies within the copy: red shows through a half blue
        let half_blue = with(parts[1].clone(), "opacity", json!(0.5));
        let mut p = s.playlist(&[&[(1.0, vec![parts[0].clone(), half_blue.clone()])]], &[]);
        leaves(&mut p.scenes[0].layers[0].objects, [5, 5, 45, 40], &[("c", 0.5, [5, 5, 45, 40])]);
        let layered = s.playlist(&[&[(0.5, vec![parts[0].clone(), half_blue])]], &[]);
        assert!(draw(&p) == draw(&layered));
        assert!(px(&draw(&p), 25, 15)[0] > 0);
        // two copies side by side are two groups; neighbours given the same id by hand are one
        let mut p = s.playlist(&[&[(1.0, parts.clone())]], &[]);
        let objects = &mut p.scenes[0].layers[0].objects;
        leaves(&mut objects[..1], [0, 0, 100, 100], &[("c", 0.5, [0, 0, 100, 100])]);
        leaves(&mut objects[1..], [0, 0, 100, 100], &[("d", 0.5, [0, 0, 100, 100])]);
        let (_, sizes) = layer_buf(&p, 0);
        assert_eq!(sizes.len(), 2);
        assert!(px(&draw(&p), 25, 15)[0] > 0); // two groups: red shows under the second
        p.scenes[0].layers[0].objects[1].groups[0].id = "c".into();
        assert_eq!(layer_buf(&p, 0).1.len(), 1);
        assert_eq!(px(&draw(&p), 25, 15), [0, 0, 128]);
    }

    /// A widget layer below 100% is a group inside its copy, as a scene layer is; groups nest.
    #[test]
    fn widget_layer_opacity_is_a_group() {
        let s = Scratch::new("layer-group");
        let parts = vec![rect(10.0, 10.0, 20.0, 20.0, "#ff0000"), rect(20.0, 10.0, 20.0, 20.0, "#0000ff")];
        let top = rect(35.0, 20.0, 20.0, 20.0, "#00ff00");
        // an opaque copy whose widget has a layer at 0.5 (red, blue) under one at 1 (green)
        let mut p = s.playlist(&[&[(1.0, vec![parts[0].clone(), parts[1].clone(), top.clone()])]], &[]);
        let objects = &mut p.scenes[0].layers[0].objects;
        leaves(&mut objects[..2], [0, 0, 60, 50], &[("c/l1", 0.5, [0, 0, 60, 50])]);
        leaves(&mut objects[2..], [0, 0, 60, 50], &[]);
        let im = draw(&p);
        assert_eq!(px(&im, 25, 15), [0, 0, 128]);
        assert_eq!(px(&im, 15, 15), [128, 0, 0]);
        assert_eq!(px(&im, 37, 25), [0, 255, 0]);
        let layered = s.playlist(&[&[(0.5, parts.clone()), (1.0, vec![top.clone()])]], &[]);
        assert!(draw(&layered) == im);
        // the same copy at 0.8: its layer's group sits inside the copy's, and the two fade together
        let mut p = s.playlist(&[&[(1.0, vec![parts[0].clone(), parts[1].clone(), top.clone()])]], &[]);
        let objects = &mut p.scenes[0].layers[0].objects;
        leaves(&mut objects[..2], [0, 0, 60, 50], &[("c", 0.8, [0, 0, 60, 50]), ("c/l1", 0.5, [0, 0, 60, 50])]);
        leaves(&mut objects[2..], [0, 0, 60, 50], &[("c", 0.8, [0, 0, 60, 50])]);
        let im = draw(&p);
        assert_eq!(px(&im, 25, 15)[2].abs_diff(102), 1); // 0.8 x 0.5 of blue, to a level
        assert_eq!(px(&im, 37, 25), [0, 204, 0]); // green over half blue, both in the copy at 0.8
        assert_eq!(layer_buf(&p, 0).1, [(60, 50), (60, 50)]);
    }

    /// A copy at 100% isn't a buffer of its own: its parts are drawn straight into the layer, giving the
    /// same bytes as the same objects placed directly.
    #[test]
    fn opaque_copy_costs_nothing() {
        let s = Scratch::new("opaque");
        s.two();
        s.png("half.png", 3, 2, |x, y| [x as u8 * 80, y as u8 * 120, 40, 100 + 70 * x as u8]);
        let mut objects = vec![
            image("two.png", 5.0, 5.0, 8.0, 4.0),
            with(image("half.png", 20.0, 3.0, 9.0, 6.0), "opacity", json!(0.7)),
            json!({ "id": "e", "type": "shape", "shape": "ellipse", "x": 12, "y": 2, "w": 30, "h": 20,
                    "fill": { "type": "linear", "angle": 30, "stops": [ { "at": 0, "color": "#ff0000" }, { "at": 1, "color": "#0000ff" } ] },
                    "stroke": { "color": "#ffffff", "width": 3 }, "opacity": 0.8 }),
        ];
        if Path::new(DEFAULT_FONT).exists() {
            objects.push(json!({ "id": "t", "type": "text", "text": "Hi", "x": 30, "y": 10, "size": 40, "outline": { "width": 2 } }));
        }
        let direct = s.playlist(&[&[(1.0, objects.clone())]], &[]);
        let mut copy = s.playlist(&[&[(1.0, objects)]], &[]);
        leaves(&mut copy.scenes[0].layers[0].objects, [0, 0, 300, 200], &[]);
        let (want, _) = layer_buf(&direct, 0);
        let (got, groups) = layer_buf(&copy, 0);
        assert!(!want.is_empty() && got == want);
        assert!(groups.is_empty());
        // one opaque picture: the layer is the picture itself, in a copy or not
        let direct = s.playlist(&[&[(1.0, vec![image("two.png", 5.0, 5.0, 8.0, 4.0)])]], &[]);
        let mut copy = direct.clone();
        leaves(&mut copy.scenes[0].layers[0].objects, [5, 5, 13, 9], &[]);
        let (want, got) = (layer_buf(&direct, 0).0, layer_buf(&copy, 0).0);
        assert!((want.left, want.top, want.image.dimensions()) == (5, 5, (8, 4)) && got == want);
    }

    /// A copy draws only inside its box, and a copy inside another inside both; fills cover the box;
    /// a translucent copy hanging off the panel's edges draws what's on the panel.
    #[test]
    fn copy_clips() {
        let s = Scratch::new("clips");
        s.two();
        // a rect and a picture reaching past the box are cut at its edge
        let mut p = s.playlist(&[&[(1.0, vec![rect(150.0, 100.0, 100.0, 40.0, "#ffffff"), image("two.png", 190.0, 140.0, 20.0, 20.0)])]], &[]);
        leaves(&mut p.scenes[0].layers[0].objects, [100, 100, 200, 150], &[]);
        let im = draw(&p);
        assert_eq!((px(&im, 199, 120), px(&im, 200, 120), px(&im, 150, 99)), ([255; 3], [0; 3], [0; 3]));
        assert_eq!((px(&im, 190, 149), px(&im, 200, 149), px(&im, 190, 150)), ([255, 0, 0], [0; 3], [0; 3]));
        // a tiled picture fills exactly the box, its pattern anchored to the picture; so do mirror and extend
        for (fill, at_100, at_101) in [("tile", [255, 0, 0], [0, 255, 0]), ("mirror", [0, 255, 0], [255, 0, 0]), ("extend", [255, 0, 0], [255, 0, 0])] {
            let mut p = s.playlist(&[&[(1.0, vec![with(image("two.png", 110.0, 110.0, 2.0, 1.0), "fill", json!(fill))])]], &[]);
            leaves(&mut p.scenes[0].layers[0].objects, [100, 100, 140, 120], &[]);
            let im = draw(&p);
            for (x, y, c) in im.enumerate_pixels() {
                let inside = (100..140).contains(&x) && (100..120).contains(&y);
                assert_eq!(c.0 != [0; 3], inside, "{fill} at {x},{y}");
            }
            assert_eq!((px(&im, 100, 105), px(&im, 101, 105)), (at_100, at_101), "{fill}");
            assert_eq!((px(&im, 110, 110), px(&im, 111, 110)), ([255, 0, 0], [0, 255, 0]), "{fill}");
        }
        // copies inside copies: the clip is the boxes' intersection, and the groups nest, each its own size
        let mut p = s.playlist(&[&[(1.0, vec![rect(0.0, 0.0, 1600.0, 720.0, "#ffffff")])]], &[]);
        leaves(&mut p.scenes[0].layers[0].objects, [150, 250, 200, 300], &[("c", 0.5, [100, 200, 200, 300]), ("c/w", 0.5, [150, 250, 200, 300])]);
        let im = draw(&p);
        assert_eq!(px(&im, 175, 275), [64; 3]);
        for (x, y) in [(149, 275), (175, 249), (200, 275), (175, 300), (120, 220)] {
            assert_eq!(px(&im, x, y), [0; 3], "at {x},{y}");
        }
        assert_eq!(layer_buf(&p, 0).1, [(100, 100), (50, 50)]);
        // hanging off the panel's top-left and bottom-right, at half: no panic, and the buffers are what's
        // on the panel
        for (clip, inside, outside, size) in [([-50, -30, 40, 40], (0, 0), (40, 10), (40, 40)), ([1580, 700, 1700, 800], (1599, 719), (1579, 710), (20, 20))] {
            let objects = vec![rect(-60.0, -60.0, 1800.0, 900.0, "#ffffff"), with(image("two.png", -45.0, -25.0, 2.0, 1.0), "fill", json!("tile"))];
            let mut p = s.playlist(&[&[(1.0, objects)]], &[]);
            leaves(&mut p.scenes[0].layers[0].objects, clip, &[("c", 0.5, clip)]);
            let im = draw(&p);
            assert!(px(&im, inside.0, inside.1) != [0; 3] && px(&im, outside.0, outside.1) == [0; 3], "{clip:?}");
            assert_eq!(layer_buf(&p, 0).1, [size]);
        }
    }

    /// A copy's picture is resampled once, from the file, at its final size: pixel art in a 3x copy is the
    /// same as the picture placed at 3x in a scene.
    #[test]
    fn one_resample() {
        let s = Scratch::new("resample");
        s.two();
        // the widget's picture is at 0, 0, 2 x 1; the copy is at 30, 30 at scale 3
        let mut copy = s.playlist(&[&[(1.0, vec![image("two.png", 30.0, 30.0, 6.0, 3.0)])]], &[]);
        leaves(&mut copy.scenes[0].layers[0].objects, [30, 30, 36, 33], &[("c", 0.999, [30, 30, 36, 33])]);
        let placed = s.playlist(&[&[(0.999, vec![image("two.png", 30.0, 30.0, 6.0, 3.0)])]], &[]);
        let im = draw(&copy);
        assert!(im == draw(&placed));
        let row: Vec<[u8; 3]> = (29..37).map(|x| px(&im, x, 31)).collect();
        assert_eq!(row, [[0; 3], [255, 0, 0], [255, 0, 0], [255, 0, 0], [0, 255, 0], [0, 255, 0], [0, 255, 0], [0; 3]]);
    }

    /// A copy's text is laid out again at its scaled size, so it stays sharp: a 2x copy's text is the text
    /// placed directly at the scaled anchor and size, not the 1x text enlarged.
    #[test]
    fn text_in_copies_is_redrawn() {
        if !Path::new(DEFAULT_FONT).exists() {
            return;
        }
        let s = Scratch::new("text");
        let text = |x: f64, y: f64, size: f64, outline: f64| {
            json!({ "id": "t", "type": "text", "text": "Hg", "x": x, "y": y, "size": size, "color": "#ffcc00", "outline": { "color": "#0000ff", "width": outline } })
        };
        // the widget's text is at 10, 5, size 30, outline 1; copies at 200, 100, at scales 2 and 1
        let mut copy = s.playlist(&[&[(1.0, vec![text(220.0, 110.0, 60.0, 2.0)])], &[(1.0, vec![text(210.0, 105.0, 30.0, 1.0)])]], &[]);
        leaves(&mut copy.scenes[0].layers[0].objects, [200, 100, 600, 300], &[]);
        leaves(&mut copy.scenes[1].layers[0].objects, [200, 100, 400, 200], &[]);
        let placed = s.playlist(&[&[(1.0, vec![text(220.0, 110.0, 60.0, 2.0)])]], &[]);
        let big = draw_at(&copy, 0, 0);
        assert!(big == draw(&placed));
        // the 1x copy enlarged 2x isn't it
        let small = draw_at(&copy, 1, 0);
        let enlarged = |x: u32, y: u32| px(&small, 200 + (x - 200) / 2, 100 + (y - 100) / 2);
        let differ = (200..600).flat_map(|x| (100..300).map(move |y| (x, y))).filter(|&(x, y)| enlarged(x, y) != px(&big, x, y)).count();
        assert!(differ > 50, "{differ}");
    }

    /// A GIF in a copy animates the scene like any other, and layers are drawn only when they change.
    #[test]
    fn copy_animation() {
        let s = Scratch::new("anim");
        let mut gif = GifEncoder::new(std::fs::File::create(s.dir.join("a.gif")).unwrap());
        for shade in [40u8, 80, 120] {
            let im = RgbaImage::from_pixel(4, 4, Rgba([shade, 0, 0, 255]));
            gif.encode_frame(image::Frame::from_parts(im, 0, 0, Delay::from_numer_denom_ms(100, 1))).unwrap();
        }
        drop(gif);
        let bottom = (1.0, vec![rect(0.0, 0.0, 50.0, 50.0, "#0000ff")]);
        let top = (1.0, vec![rect(100.0, 0.0, 50.0, 50.0, "#00ff00")]);
        let anim = (1.0, vec![rect(60.0, 0.0, 10.0, 10.0, "#ffffff"), image("a.gif", 60.0, 0.0, 4.0, 4.0)]);
        let mut p = s.playlist(&[&[bottom, anim, top]], &[]);
        // the copy at 0.5 is 60, 0 to 63, 3: the GIF's last row and column are past it
        leaves(&mut p.scenes[0].layers[1].objects, [60, 0, 63, 3], &[("c", 0.5, [60, 0, 63, 3])]);
        let ms = Duration::from_millis;
        assert_eq!(times(&p.scenes[0], &p.global, &opts()), [(ms(0), ms(100)), (ms(100), ms(100)), (ms(200), ms(100))]);
        let count = |f: &dyn Fn()| {
            let before = RENDERS.with(Cell::get);
            f();
            RENDERS.with(Cell::get) - before
        };
        // first time: the two static layers, and the copy's layer for each of the GIF's frames; then only those
        assert_eq!(count(&|| { prepare(&p.scenes[0], &p.global, &opts()).unwrap(); }), 2 + 3);
        assert_eq!(count(&|| { prepare(&p.scenes[0], &p.global, &opts()).unwrap(); }), 3);
        // each frame shows the GIF's, at half, inside the copy's box
        for (t, shade) in [(0, 20), (100, 40), (250, 60)] {
            let im = draw_at(&p, 0, t);
            assert_eq!((px(&im, 60, 0), px(&im, 63, 0), px(&im, 60, 3)), ([shade, 0, 0], [0; 3], [0; 3]), "at {t} ms");
        }
    }

    /// Positions round half up, as the studio's do: a picture at x -0.5 lands at 0 (it was at -1).
    #[test]
    fn rounding_at_the_top() {
        let s = Scratch::new("rounding");
        s.two();
        let red = [255, 0, 0];
        let green = [0, 255, 0];
        for (x, first) in [(-0.5, red), (-1.5, green), (-1.4, green), (0.5, [0; 3]), (-0.6, green)] {
            // one opaque picture (the layer is the picture itself), and the same tiled (drawn)
            let p = s.playlist(&[&[(1.0, vec![image("two.png", x, -0.5, 2.0, 1.0)])], &[(1.0, vec![with(image("two.png", x, 0.0, 2.0, 1.0), "fill", json!("tile"))])]], &[]);
            assert_eq!(px(&draw_at(&p, 0, 0), 0, 0), first, "at {x}");
            let tiled = draw_at(&p, 1, 0);
            let anchor = rnd(x);
            assert_eq!(px(&tiled, 0, 0), if anchor.rem_euclid(2) == 0 { red } else { green }, "tiled at {x}");
        }
        // sizes round the same way
        let p = s.playlist(&[&[(1.0, vec![image("two.png", 0.0, 0.0, 2.5, 1.5)])]], &[]);
        assert_eq!((px(&draw(&p), 2, 1), px(&draw(&p), 3, 1), px(&draw(&p), 0, 2)), (green, [0; 3], [0; 3]));
    }

    /// JPEG frames keep full colour unless the scene's own layers draw video, a copy's video leaf
    /// included. A video in the global layers alone doesn't count, as before widgets.
    #[test]
    fn sampling_sees_the_scenes_video() {
        let s = Scratch::new("sampling");
        let still = (1.0, vec![image("a.png", 0.0, 0.0, 10.0, 10.0)]);
        let video = (1.0, vec![json!({ "id": "v", "type": "video", "src": "v.mp4", "w": 10, "h": 10 })]);
        let p = s.playlist(&[&[still.clone()]], &[]);
        assert_eq!(art::sampling(&p.scenes[0], &p.global), SamplingFactor::R_4_4_4);
        let mut p = s.playlist(&[&[still.clone()]], &[video.clone()]);
        assert_eq!(art::sampling(&p.scenes[0], &p.global), SamplingFactor::R_4_4_4);
        leaves(&mut p.global[0].objects, [0, 0, 10, 10], &[("c", 0.5, [0, 0, 10, 10])]);
        assert_eq!(art::sampling(&p.scenes[0], &p.global), SamplingFactor::R_4_4_4);
        let p = s.playlist(&[&[still.clone(), video.clone()]], &[]);
        assert_eq!(art::sampling(&p.scenes[0], &p.global), SamplingFactor::R_4_2_0);
        let mut p = s.playlist(&[&[still, video]], &[]);
        leaves(&mut p.scenes[0].layers[1].objects, [0, 0, 10, 10], &[("c", 0.5, [0, 0, 10, 10]), ("c/l1", 0.8, [0, 0, 10, 10])]);
        assert_eq!(art::sampling(&p.scenes[0], &p.global), SamplingFactor::R_4_2_0);
    }

    /// The shared vectors: the box a file of its own size takes inside its placed box.
    #[test]
    fn fit_aspect_vectors() {
        let rows: Vec<Value> = serde_json::from_str(include_str!("../tests/widgets/aspect.json")).unwrap();
        assert!(rows.len() > 10);
        let n = |v: &Value, i: usize| v[i].as_i64().unwrap();
        for row in &rows {
            let (b, native) = (&row["box"], &row["native"]);
            let (fw, fh) = match row["rotate"].as_i64().unwrap() {
                90 | 270 => (n(native, 1), n(native, 0)),
                _ => (n(native, 0), n(native, 1)),
            };
            let got = fit_aspect(n(b, 0), n(b, 1), n(b, 2), n(b, 3), fw, fh);
            let r = &row["result"];
            assert_eq!(got, (n(r, 0), n(r, 1), n(r, 2), n(r, 3)), "{row}");
        }
    }

    /// keep_aspect is worked out when the file is opened: a 30 x 10 picture in a 40 x 40 box draws at
    /// 40 x 13, 13 down, and a tiled fill is anchored to that box.
    #[test]
    fn keep_aspect_fits_when_opened() {
        let s = Scratch::new("aspect");
        // each row its own colour
        s.png("wide.png", 30, 10, |_, y| [y as u8 * 20, 255 - y as u8 * 20, 0, 255]);
        let row = |r: u8| [r * 20, 255 - r * 20, 0];
        let objects = vec![image("wide.png", 100.0, 200.0, 40.0, 40.0), with(image("wide.png", 300.0, 200.0, 40.0, 40.0), "rotate", json!(90)),
                           with(image("wide.png", 500.0, 200.0, 40.0, 40.0), "fill", json!("tile"))];
        let mut p = s.playlist(&[&[(1.0, objects)]], &[]);
        for o in &mut p.scenes[0].layers[0].objects {
            media(o).keep_aspect = true;
        }
        // the tiled one is a copy's leaf, filling its box
        leaves(&mut p.scenes[0].layers[0].objects[2..], [500, 200, 540, 240], &[]);
        let im = draw(&p);
        // 40 x 13 at 100, 213: rows 0 to 9 of the file, nearest
        let drawn: Vec<u32> = (190..250).filter(|y| px(&im, 120, *y) != [0; 3]).collect();
        assert_eq!(drawn, (213..226).collect::<Vec<_>>());
        assert_eq!((px(&im, 100, 213), px(&im, 139, 225), px(&im, 99, 220), px(&im, 140, 220)), (row(0), row(9), [0; 3], [0; 3]));
        // turned a quarter: 13 x 40 at 313, 200
        let drawn: Vec<u32> = (290..350).filter(|x| px(&im, *x, 220) != [0; 3]).collect();
        assert_eq!(drawn, (313..326).collect::<Vec<_>>());
        // tiled from the fitted box: the row above it is the fitted picture's last row, file row 9
        assert_eq!((px(&im, 520, 213), px(&im, 520, 212), px(&im, 520, 200), px(&im, 520, 239), px(&im, 520, 240)), (row(0), row(9), row(0), row(0), [0; 3]));
    }

    /// A tint recolours every pixel the file draws and keeps its transparency, after the one resample; the
    /// same file untinted, in the same scene, keeps its own colours.
    #[test]
    fn tint_recolours() {
        let s = Scratch::new("tint");
        // white, clear; half-transparent white, black
        let icon = |x: u32, y: u32| match (x, y) {
            (0, 0) => [255, 255, 255, 255],
            (1, 0) => [0, 0, 0, 0],
            (0, _) => [255, 255, 255, 128],
            _ => [0, 0, 0, 255],
        };
        s.png("a.png", 2, 2, icon);
        s.png("b.png", 2, 2, icon);
        // tinted first, then the same file untinted; and the other way round with another file
        for (file, tinted_first) in [("a.png", true), ("b.png", false)] {
            let objects = vec![image(file, 0.0, 0.0, 2.0, 2.0), image(file, 10.0, 0.0, 2.0, 2.0), image(file, 20.0, 0.0, 4.0, 4.0)];
            let mut p = s.playlist(&[&[(1.0, objects)]], &[]);
            let (red, plain) = if tinted_first { (0, 1) } else { (1, 0) };
            let layer = &mut p.scenes[0].layers[0].objects;
            media(&mut layer[red]).tint = Some("#ff0000".into());
            media(&mut layer[2]).tint = Some("#00ff00".into());
            layer[red].x = 0.0;
            layer[plain].x = 10.0;
            let im = draw(&p);
            assert_eq!([px(&im, 0, 0), px(&im, 1, 0), px(&im, 0, 1), px(&im, 1, 1)], [[255, 0, 0], [0; 3], [128, 0, 0], [255, 0, 0]], "{file}");
            assert_eq!([px(&im, 10, 0), px(&im, 11, 0), px(&im, 10, 1), px(&im, 11, 1)], [[255; 3], [0; 3], [128; 3], [0; 3]], "{file}");
            // scaled first (2x, nearest), then tinted
            assert_eq!([px(&im, 21, 1), px(&im, 22, 1), px(&im, 21, 2)], [[0, 255, 0], [0; 3], [0, 128, 0]], "{file}");
            // the layer keeps the half-transparent pixel's alpha
            let (buf, _) = layer_buf(&p, 0);
            let alpha = |x: u32, y: u32| buf.image.get_pixel(x - buf.left, y - buf.top)[3];
            assert_eq!((alpha(0, 0), alpha(1, 0), alpha(0, 1)), (255, 0, 128));
        }
    }

    /// A video is fitted by its own size and tinted like a picture, frame by frame. Needs ffmpeg: the
    /// one in $WV_TEST_FFMPEG.
    #[test]
    fn video_fits_and_tints() {
        let Some(ffmpeg) = std::env::var_os("WV_TEST_FFMPEG").map(PathBuf::from) else {
            eprintln!("video_fits_and_tints skipped: set WV_TEST_FFMPEG to an ffmpeg to run it");
            return;
        };
        let s = Scratch::new("video");
        let made = std::process::Command::new(&ffmpeg)
            .args(["-v", "error", "-f", "lavfi", "-i", "color=c=white:s=30x10:r=10:d=0.3", "-c:v", "mpeg4", "-pix_fmt", "yuv420p"])
            .arg(s.dir.join("v.mp4"))
            .status()
            .unwrap();
        assert!(made.success());
        let video = |x: f64| json!({ "id": "v", "type": "video", "src": "v.mp4", "x": x, "y": 200, "w": 40, "h": 40 });
        let mut p = s.playlist(&[&[(1.0, vec![video(100.0), video(300.0)])]], &[]);
        let objects = &mut p.scenes[0].layers[0].objects;
        media(&mut objects[0]).keep_aspect = true;
        media(&mut objects[0]).tint = Some("#ff0000".into());
        media(&mut objects[1]).keep_aspect = true;
        let o = Options { ffmpeg, ..opts() };
        let im = render_at(&p.scenes[0], &p.global, &o, Duration::from_millis(100), &[]);
        let drawn: Vec<u32> = (190..250).filter(|y| px(&im, 120, *y) != [0; 3]).collect();
        assert_eq!(drawn, (213..226).collect::<Vec<_>>());
        assert!((213..226).all(|y| (100..140).all(|x| px(&im, x, y) == [255, 0, 0])));
        assert!((213..226).all(|y| (300..340).all(|x| px(&im, x, y).iter().all(|c| *c > 240))));
    }

    /// Groups already open are the caller's: drawn below `depth`, a copy's parts give what the copy's own
    /// buffer held, so its group can be laid over later.
    #[test]
    fn groups_below_depth_are_the_callers() {
        let s = Scratch::new("depth");
        let parts = vec![rect(10.0, 10.0, 20.0, 20.0, "#ff0000"), rect(20.0, 10.0, 20.0, 20.0, "#0000ff"), rect(30.0, 30.0, 5.0, 5.0, "#ffffff")];
        let mut p = s.playlist(&[&[(1.0, parts)]], &[]);
        let objects = &mut p.scenes[0].layers[0].objects;
        leaves(&mut objects[..2], [5, 5, 45, 40], &[("c", 0.5, [5, 5, 45, 40]), ("c/l1", 0.5, [5, 5, 45, 40])]);
        leaves(&mut objects[2..], [5, 5, 45, 40], &[("c", 0.5, [5, 5, 45, 40])]);
        let whole = layer_buf(&p, 0).0;
        let mut slot = slots(&p.scenes[0], &p.global, &opts()).remove(0);
        let inner = render_objects(&mut slot.objects, Duration::ZERO, 1, (5, 5), (40, 35));
        let mut canvas = RgbaImage::new(WIDTH, HEIGHT);
        let touched = inner.onto_rgba(&mut canvas, (0, 0), 0.5);
        assert!(Buf::take(canvas, (0, 0), touched) == whole);
    }

    /// Editing a widget changes the keys of exactly the scenes that draw it; a live input's words change
    /// none. (Leaves built by hand: these are the scenes as the reader expands them.)
    #[test]
    fn cache_keys_follow_widgets() {
        let s = Scratch::new("keys");
        let copy = vec![rect(0.0, 0.0, 50.0, 20.0, "#102030"), json!({ "id": "t", "type": "text", "text": "{pc.cpu.temp:d}°C", "x": 5, "y": 2 })];
        let p = s.playlist(&[&[(1.0, copy)], &[(1.0, vec![rect(0.0, 0.0, 5.0, 5.0, "#ffffff")])]], &[]);
        let expanded = |edit: &dyn Fn(&mut Vec<Object>)| {
            let mut p = p.clone();
            leaves(&mut p.scenes[0].layers[0].objects, [0, 0, 50, 20], &[("c", 0.5, [0, 0, 50, 20])]);
            edit(&mut p.scenes[0].layers[0].objects);
            (key(&p.scenes[0], &p.global, &opts()), key(&p.scenes[1], &p.global, &opts()))
        };
        let (user, other) = expanded(&|_| {});
        // the widget's panel recoloured, moved inside the copy, or the copy faded
        let edits: [&dyn Fn(&mut Vec<Object>); 3] = [
            &|o: &mut Vec<Object>| {
                o[0].kind = Kind::Shape(playlist::Shape { fill: Some(playlist::Paint::Solid { color: "#203040".into() }), ..playlist::Shape::default() })
            },
            &|o: &mut Vec<Object>| o[0].x = 1.0,
            &|o: &mut Vec<Object>| o.iter_mut().for_each(|o| o.groups[0].opacity = 0.6),
        ];
        for edit in edits {
            assert_eq!(expanded(edit).1, other);
            assert_ne!(expanded(edit).0, user);
        }
        // a copy whose live input reads another sensor: the same place, other words
        let words = expanded(&|o: &mut Vec<Object>| match &mut o[1].kind {
            Kind::Text(t) => t.text = "{pc.gpu.temp:d}°C".into(),
            _ => panic!(),
        });
        assert_eq!(words, (user, other));
    }

    /// The same, end to end: the playlist and its widgets read from their files. Editing a widget.json
    /// changes the keys of exactly the scenes that draw it, directly or inside another widget, and so does
    /// replacing a file it draws. A copy's live input reading another sensor changes no key; a static
    /// input's words do.
    #[test]
    fn cache_keys_follow_widget_files() {
        let s = Scratch::new("key-files");
        let (list, widgets) = (s.dir.join("playlist").join("playlist.json"), s.dir.join("widgets"));
        std::fs::create_dir_all(s.dir.join("playlist")).unwrap();
        for id in ["badge", "card", "plain"] {
            std::fs::create_dir_all(widgets.join(id).join("media")).unwrap();
        }
        let write = |path: &Path, value: Value| std::fs::write(path, value.to_string()).unwrap();
        let icon = widgets.join("badge").join("media").join("icon.png");
        RgbaImage::from_pixel(2, 2, Rgba([255, 0, 0, 255])).save(&icon).unwrap();
        let badge = |color: &str| json!({ "version": 1, "w": 60, "h": 20,
            "inputs": [{ "id": "label", "type": "text", "default": "CPU" }, { "id": "value", "type": "text", "default": "" }],
            "layers": [{ "id": "l1", "objects": [
                { "id": "r", "type": "shape", "w": 60, "h": 20, "fill": { "type": "solid", "color": color } },
                { "id": "i", "type": "image", "src": "media/icon.png", "w": 20, "h": 20 },
                { "id": "t1", "type": "text", "text": "{$label}", "x": 22, "y": 2, "size": 12 },
                { "id": "t2", "type": "text", "text": "{$value}", "x": 40, "y": 2, "size": 12 } ] }] });
        let card = |x: f64| json!({ "version": 1, "w": 100, "h": 40,
            "layers": [{ "id": "l1", "objects": [{ "id": "b", "type": "widget", "widget": "badge", "x": x, "y": 10, "scale": 1 }] }] });
        write(&widgets.join("badge").join("widget.json"), badge("#102030"));
        write(&widgets.join("card").join("widget.json"), card(10.0));
        write(&widgets.join("plain").join("widget.json"), json!({ "version": 1, "w": 30, "h": 30,
            "layers": [{ "id": "l1", "objects": [{ "id": "r", "type": "shape", "w": 30, "h": 30 }] }] }));
        let playlist = |label: &str, value: &str| json!({ "version": 2, "scenes": [
            { "id": "direct", "layers": [{ "id": "l", "objects": [
                { "id": "c", "type": "widget", "widget": "badge", "x": 10, "y": 10, "inputs": { "label": label, "value": value } }] }] },
            { "id": "nested", "layers": [{ "id": "l", "objects": [{ "id": "c", "type": "widget", "widget": "card", "x": 100, "y": 100 }] }] },
            { "id": "other", "layers": [{ "id": "l", "objects": [{ "id": "c", "type": "widget", "widget": "plain" }] }] } ] });
        write(&list, playlist("GPU", "{pc.gpu.temp:d}°C"));
        let keys = || {
            let p = playlist::read(&list, 30.0, Path::new("ffmpeg")).unwrap();
            p.scenes.iter().map(|scene| key(scene, &p.global, &opts())).collect::<Vec<u64>>()
        };
        let p = playlist::read(&list, 30.0, Path::new("ffmpeg")).unwrap();
        let ids = |i: usize| p.scenes[i].layers[0].objects.iter().map(|o| o.id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids(0), ["c/r", "c/i", "c/t1", "c/t2"]);
        assert_eq!(ids(1), ["c/b/r", "c/b/i", "c/b/t1", "c/b/t2"]);
        let base = keys();
        // which scenes' keys an edit changed: direct, nested, other
        let changed = |before: &[u64]| -> Vec<bool> { keys().iter().zip(before).map(|(a, b)| a != b).collect() };
        write(&list, playlist("GPU", "{pc.cpu.temp:d}°C"));
        assert_eq!(changed(&base), [false, false, false], "a live input's words");
        write(&list, playlist("Graphics", "{pc.gpu.temp:d}°C"));
        assert_eq!(changed(&base), [true, false, false], "a static input's words");
        write(&list, playlist("GPU", "{pc.gpu.temp:d}°C"));
        assert_eq!(changed(&base), [false, false, false]);
        write(&widgets.join("badge").join("widget.json"), badge("#203040"));
        assert_eq!(changed(&base), [true, true, false], "the widget recoloured");
        let recoloured = keys();
        RgbaImage::from_pixel(3, 3, Rgba([0, 0, 255, 255])).save(&icon).unwrap();
        assert_eq!(changed(&recoloured), [true, true, false], "the widget's image replaced");
        let replaced = keys();
        write(&widgets.join("card").join("widget.json"), card(12.0));
        assert_eq!(changed(&replaced), [false, true, false], "the outer widget edited");
    }
}

/// Live text laid over the frames: the over list (docs/widgets.md, Live text). Owned by work package P3.
#[cfg(test)]
mod over_tests {
    use super::*;
    use crate::playlist::{self, Group, Media, Paint};
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Rgba};
    use serde_json::{Value, json};
    use std::path::PathBuf;

    /// A folder of its own with `a.gif`: 3 frames of 100 ms, 4 x 4, dark red.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("wvplay-over-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let mut gif = GifEncoder::new(std::fs::File::create(dir.join("a.gif")).unwrap());
            for shade in [40u8, 80, 120] {
                let im = RgbaImage::from_pixel(4, 4, Rgba([shade, 0, 0, 255]));
                gif.encode_frame(image::Frame::from_parts(im, 0, 0, Delay::from_numer_denom_ms(100, 1))).unwrap();
            }
            Scratch { dir }
        }

        fn gif(&self, id: &str, x: f64, y: f64, w: f64, h: f64) -> Object {
            object(id, x, y, w, h, Kind::Media(Media { src: self.dir.join("a.gif"), ..Media::default() }))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn opts() -> Options {
        Options { rotate180: false, quality: 95, ffmpeg: PathBuf::from("ffmpeg") }
    }

    fn have_font() -> bool {
        Path::new(DEFAULT_FONT).exists()
    }

    fn object(id: &str, x: f64, y: f64, w: f64, h: f64, kind: Kind) -> Object {
        Object { id: id.into(), visible: true, opacity: 1.0, x, y, w, h, rotate: 0, flip: false, kind, clip: None, groups: Vec::new() }
    }

    fn rect(id: &str, x: f64, y: f64, w: f64, h: f64, color: &str) -> Object {
        let shape = Shape { fill: Some(Paint::Solid { color: color.into() }), crisp: true, ..Shape::default() };
        object(id, x, y, w, h, Kind::Shape(shape))
    }

    /// Text at `x`, `y`: live when it has a placeholder.
    fn text(id: &str, x: f64, y: f64, words: &str) -> Object {
        let w = Text { id: id.into(), kind: "text".into(), text: words.into(), size: 40.0, x: x as f32, y: y as f32,
                       live: crate::data::is_live(words), ..Text::default() };
        object(id, x, y, 0.0, 0.0, Kind::Text(w))
    }

    /// The object as a copy's leaf: inside `clip`, in `groups` (id, opacity, rect), outermost first.
    fn leaf(mut o: Object, clip: Rect, groups: &[(&str, f32, Rect)]) -> Object {
        o.clip = Some(clip);
        o.groups = groups.iter().map(|(id, opacity, rect)| Group { id: id.to_string(), opacity: *opacity, rect: *rect }).collect();
        if let Kind::Text(w) = &mut o.kind {
            w.clip = Some(clip);
        }
        o
    }

    fn with_opacity(mut o: Object, opacity: f32) -> Object {
        o.opacity = opacity;
        if let Kind::Text(w) = &mut o.kind {
            w.opacity = opacity;
        }
        o
    }

    fn layer(id: &str, opacity: f32, objects: Vec<Object>) -> Layer {
        Layer { id: id.into(), name: id.into(), visible: true, opacity, objects }
    }

    fn scene(layers: Vec<Layer>) -> Scene {
        Scene { id: "s".into(), name: "s".into(), seconds: 10.0, background: "#000000".into(), layers }
    }

    /// The over list in short: `S` a still, `T<index>` a text (`*fold` unless 1), `G<opacity>[...]` a group.
    fn outline(over: &[Over]) -> String {
        over.iter()
            .map(|node| match node {
                Over::Still(_) => "S".to_string(),
                Over::Text { index, fold } if *fold == 1.0 => format!("T{index}"),
                Over::Text { index, fold } => format!("T{index}*{fold}"),
                Over::Group { opacity, children, .. } => format!("G{opacity}[{}]", outline(children)),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn over_of(scene: &Scene, global: &[Layer]) -> Vec<Over> {
        prepare(scene, global, &opts()).unwrap().over
    }

    /// The live texts filled in with `values`, in turn.
    fn filled(scene: &Scene, global: &[Layer], values: &[&str]) -> Vec<Text> {
        let live = playlist::live_texts(scene, global);
        assert_eq!(live.len(), values.len(), "values for every live text");
        live.into_iter().zip(values).map(|(w, v)| Text { text: v.to_string(), ..w }).collect()
    }

    /// The scene and the global layers with their live texts written in as static text with `values`.
    fn written_in(scene: &Scene, global: &[Layer], values: &[&str]) -> (Scene, Vec<Layer>) {
        let mut values = values.iter();
        let mut write = |layers: &[Layer]| -> Vec<Layer> {
            let mut layers = layers.to_vec();
            for o in layers.iter_mut().filter(|l| l.visible).flat_map(|l| l.objects.iter_mut()).filter(|o| o.visible) {
                if let Kind::Text(w) = &mut o.kind
                    && w.live
                {
                    (w.text, w.live) = (values.next().unwrap().to_string(), false);
                }
            }
            layers
        };
        let layers = write(&scene.layers);
        (Scene { layers, ..scene.clone() }, write(global))
    }

    /// The largest difference in any channel.
    fn worst(a: &RgbImage, b: &RgbImage) -> u8 {
        a.pixels().zip(b.pixels()).flat_map(|(p, q)| (0..3).map(move |c| p[c].abs_diff(q[c]))).max().unwrap_or(0)
    }

    /// The shapes of the over list, one rule of docs/widgets.md (Live text) at a time: where S starts,
    /// what goes into the frames, and what's laid over them. Leaves are built as a copy's expansion
    /// makes them.
    #[test]
    fn over_list_shapes() {
        let g1 = [0, 0, 100, 40];
        let g2 = [50, 0, 150, 40];
        // two opaque gauge copies in one layer: S is the first value alone; the second gauge's panel is
        // laid over it, then the second value
        let gauges = scene(vec![layer("l", 1.0, vec![
            rect("bg", 0.0, 0.0, 400.0, 200.0, "#102030"),
            leaf(rect("g1/background", 0.0, 0.0, 100.0, 40.0, "#202020"), g1, &[]),
            leaf(text("g1/v", 4.0, 4.0, "{pc.cpu.temp}"), g1, &[]),
            leaf(rect("g1/u", 4.0, 30.0, 92.0, 6.0, "#3ddc84"), g1, &[]),
            leaf(rect("g2/background", 50.0, 0.0, 100.0, 40.0, "#202020"), g2, &[]),
            leaf(text("g2/v", 54.0, 4.0, "{pc.gpu.temp}"), g2, &[]),
            leaf(rect("g2/u", 54.0, 30.0, 92.0, 6.0, "#3ddc84"), g2, &[]),
        ])]);
        let planned = plan(&gauges, &[], &opts());
        assert_eq!(planned.placement, Some(Placement { on_top: false, cut: "g1/v".into() }));
        let below: Vec<Vec<&str>> = planned.below.iter().map(|s| s.objects.iter().map(|o| o.object.id.as_str()).collect()).collect();
        assert_eq!(below, [["bg", "g1/background"]]);
        assert_eq!(outline(&over_of(&gauges, &[])), "T0 S T1 S");

        // a copy at 0.5: S is the copy, whose live text is laid over in its group
        let c = [10, 10, 110, 50];
        let faded = |extra: Vec<Object>| {
            let mut objects = vec![
                rect("r", 0.0, 0.0, 20.0, 20.0, "#ff0000"),
                leaf(rect("h/background", 10.0, 10.0, 100.0, 40.0, "#202020"), c, &[("h", 0.5, c)]),
                leaf(text("h/v", 14.0, 14.0, "{pc.cpu.temp}"), c, &[("h", 0.5, c)]),
                leaf(rect("h/u", 14.0, 40.0, 92.0, 6.0, "#3ddc84"), c, &[("h", 0.5, c)]),
            ];
            objects.extend(extra);
            scene(vec![layer("l", 1.0, objects)])
        };
        let one = faded(Vec::new());
        assert_eq!(placement(&one, &[], &opts()).unwrap().cut, "h/background");
        assert_eq!(outline(&over_of(&one, &[])), "G0.5[S T0 S]");
        // and whatever follows it in the layer, then later layers, and the global ones, are stills
        let after = faded(vec![rect("z", 0.0, 0.0, 5.0, 5.0, "#ffffff")]);
        let global = [layer("g", 0.5, vec![rect("bar", 0.0, 700.0, 1600.0, 20.0, "#101010")])];
        assert_eq!(outline(&over_of(&after, &global)), "G0.5[S T0 S] S");

        // a widget layer at 0.5 inside a copy at 0.8: the group in the group holds the text
        let nested = scene(vec![layer("l", 1.0, vec![
            leaf(rect("k/background", 10.0, 10.0, 100.0, 40.0, "#202020"), c, &[("k", 0.8, c)]),
            leaf(rect("k/back", 12.0, 12.0, 50.0, 20.0, "#404040"), c, &[("k", 0.8, c), ("k/l1", 0.5, c)]),
            leaf(text("k/v", 14.0, 14.0, "{pc.cpu.temp}"), c, &[("k", 0.8, c), ("k/l1", 0.5, c)]),
            leaf(rect("k/top", 14.0, 40.0, 92.0, 6.0, "#3ddc84"), c, &[("k", 0.8, c)]),
        ])]);
        assert_eq!(placement(&nested, &[], &opts()).unwrap().cut, "k/background");
        assert_eq!(outline(&over_of(&nested, &[])), "G0.8[S G0.5[S T0] S]");

        // a scene layer at 0.5: S is the whole layer, which is a group over the panel
        let layered = scene(vec![
            layer("under", 1.0, vec![rect("bg", 0.0, 0.0, 400.0, 200.0, "#102030")]),
            layer("l", 0.5, vec![rect("a", 0.0, 0.0, 20.0, 20.0, "#ff0000"), text("t", 10.0, 10.0, "{time}"), rect("b", 30.0, 10.0, 20.0, 20.0, "#00ff00")]),
            layer("over", 1.0, vec![rect("o", 0.0, 0.0, 20.0, 20.0, "#0000ff")]),
            layer("later", 0.5, vec![text("t2", 10.0, 100.0, "{time}")]),
        ]);
        let planned = plan(&layered, &[], &opts());
        assert_eq!(planned.placement, Some(Placement { on_top: false, cut: "a".into() }));
        assert_eq!(planned.below.len(), 1);
        let over = over_of(&layered, &[]);
        assert_eq!(outline(&over), "G0.5[S T0 S] S G0.5[T1]");
        let Over::Group { rect: whole, .. } = &over[0] else { unreachable!() };
        assert_eq!(*whole, PANEL_RECT);

        // a copy hanging off the panel draws nothing, and its live text keeps its number
        let off = [-300, 0, -200, 40];
        let gone = scene(vec![layer("l", 1.0, vec![
            text("first", 0.0, 0.0, "{time}"),
            leaf(text("o/v", -290.0, 4.0, "{time}"), off, &[("o", 0.5, off)]),
            text("last", 0.0, 100.0, "{time}"),
        ])]);
        assert_eq!(outline(&over_of(&gone, &[])), "T0 T2");

        // no live text: nothing over the frames
        let none = scene(vec![layer("l", 1.0, vec![rect("r", 0.0, 0.0, 20.0, 20.0, "#ff0000")])]);
        assert_eq!(placement(&none, &[], &opts()), None);
        assert!(over_of(&none, &[]).is_empty());
        // only the global layers' live text: P is there
        let clock = [layer("g", 1.0, vec![text("clock", 0.0, 680.0, "{time}")])];
        assert_eq!(placement(&none, &clock, &opts()).unwrap().cut, "clock");
        assert_eq!(outline(&over_of(&none, &clock)), "T0");
    }

    /// With something animated from S up, every live text goes on top: one text per live text, folding in
    /// its layer's and its groups' opacities, drawn over the animation and only inside its clip.
    #[test]
    fn live_on_top() {
        if !have_font() {
            return;
        }
        let s = Scratch::new("on-top");
        let c = [100, 100, 300, 180];
        let scene = scene(vec![
            layer("l", 0.5, vec![
                leaf(rect("c/background", 100.0, 100.0, 200.0, 80.0, "#202020"), c, &[("c", 0.5, c)]),
                leaf(with_opacity(text("c/v", 110.0, 110.0, "{pc.cpu.temp}"), 0.8), c, &[("c", 0.5, c)]),
                text("t", 600.0, 300.0, "{time}"),
            ]),
            layer("anim", 1.0, vec![s.gif("g", 90.0, 90.0, 400.0, 100.0)]),
        ]);
        assert_eq!(placement(&scene, &[], &opts()), Some(Placement { on_top: true, cut: "c/background".into() }));
        let prepared = prepare(&scene, &[], &opts()).unwrap();
        assert_eq!(prepared.over, [Over::Text { index: 0, fold: 0.25 }, Over::Text { index: 1, fold: 0.5 }]);
        assert_eq!(prepared.frames.len(), 3);
        // white text, longer than its copy is wide, at 0.8 × 0.25 over the animation
        let shown = render_at(&scene, &[], &opts(), Duration::ZERO, &filled(&scene, &[], &["88888888888888", "12:34"]));
        let blank = render_at(&scene, &[], &opts(), Duration::ZERO, &filled(&scene, &[], &["", ""]));
        let changed: Vec<(u32, u32)> =
            shown.enumerate_pixels().zip(blank.pixels()).filter(|((_, _, p), q)| p != q).map(|((x, y, _), _)| (x, y)).collect();
        let inside = |(x, y): (u32, u32)| (c[0]..c[2]).contains(&(x as i64)) && (c[1]..c[3]).contains(&(y as i64));
        let near_t = |(x, y): (u32, u32)| (590..900).contains(&x) && (290..380).contains(&y);
        assert!(changed.iter().all(|p| inside(*p) || near_t(*p)), "drawn outside its clip");
        // over the animation: it shows where the GIF is
        let over_gif = changed.iter().filter(|(x, y)| inside((*x, *y)) && (90..490).contains(x) && (90..190).contains(y)).count();
        assert!(over_gif > 50, "{over_gif} pixels of the text over the animation");
        let (x, y) = changed.iter().copied().find(|p| inside(*p)).unwrap();
        let (p, q) = (shown.get_pixel(x, y), blank.get_pixel(x, y));
        assert!(p[1] > q[1] && p[1] - q[1] <= 52, "{p:?} over {q:?}: about a fifth of white"); // 0.2 × 255 at most
    }

    /// In place, the frames and the baked over list draw exactly what the scene would with its live text
    /// written in as static text, for every kind of group the text can sit in.
    #[test]
    fn live_in_place_is_exact() {
        if !have_font() {
            return;
        }
        let bg = || rect("bg", 0.0, 0.0, 1600.0, 720.0, "#203040");
        let c = [100, 100, 300, 180];
        let panel = |id: &str, groups: &[(&str, f32, Rect)]| leaf(rect(id, 100.0, 100.0, 200.0, 80.0, "#606060"), c, groups);
        let value = |id: &str, groups: &[(&str, f32, Rect)]| leaf(text(id, 110.0, 110.0, "{pc.cpu.temp}"), c, groups);
        let bar = |id: &str, groups: &[(&str, f32, Rect)]| leaf(rect(id, 110.0, 150.0, 180.0, 20.0, "#00ff00"), c, groups);
        let cover = || layer("cover", 1.0, vec![rect("cover", 150.0, 90.0, 60.0, 100.0, "#ff0000")]);
        let g2 = [150, 100, 350, 180];
        let cases: Vec<(&str, Scene, Vec<&str>)> = vec![
            ("(a) an opaque copy under a still layer",
             scene(vec![layer("l", 1.0, vec![bg(), panel("c/p", &[]), value("c/v", &[]), bar("c/u", &[])]), cover()]), vec!["88°"]),
            ("(b) a copy at 0.5 over its own background",
             scene(vec![layer("l", 1.0, vec![bg(), panel("c/background", &[("c", 0.5, c)]), value("c/v", &[("c", 0.5, c)]), bar("c/u", &[("c", 0.5, c)])])]),
             vec!["88°"]),
            ("(c) a widget layer at 0.5 in a copy at 0.8",
             scene(vec![layer("l", 1.0, vec![
                 bg(),
                 panel("c/background", &[("c", 0.8, c)]),
                 bar("c/back", &[("c", 0.8, c), ("c/l1", 0.5, c)]),
                 value("c/v", &[("c", 0.8, c), ("c/l1", 0.5, c)]),
                 leaf(rect("c/top", 120.0, 105.0, 40.0, 70.0, "#ffff00"), c, &[("c", 0.8, c)]),
             ])]),
             vec!["88°"]),
            ("(d) a scene layer at 0.5",
             scene(vec![layer("under", 1.0, vec![bg()]),
                        layer("l", 0.5, vec![rect("a", 100.0, 100.0, 200.0, 80.0, "#ff00ff"), text("t", 110.0, 110.0, "{time}"),
                                             rect("b", 150.0, 90.0, 60.0, 100.0, "#00ffff")])]),
             vec!["12:34"]),
            ("(e) two live texts in different layers, a still layer between",
             scene(vec![layer("l", 1.0, vec![bg(), text("t1", 110.0, 110.0, "{time}")]), cover(),
                        layer("top", 1.0, vec![text("t2", 140.0, 130.0, "{pc.cpu.load}"), rect("o", 200.0, 120.0, 30.0, 60.0, "#ffffff")])]),
             vec!["12:34", "56%"]),
            ("(f) two gauge copies in one layer",
             scene(vec![layer("l", 1.0, vec![
                 bg(),
                 panel("g1/p", &[]), value("g1/v", &[]),
                 leaf(rect("g2/p", 150.0, 100.0, 200.0, 80.0, "#606060"), g2, &[]),
                 leaf(text("g2/v", 160.0, 110.0, "{pc.gpu.temp}"), g2, &[]),
             ])]),
             vec!["88°", "77°"]),
        ];
        let mut wrong = Vec::new();
        for (name, scene, values) in &cases {
            let over = over_of(scene, &[]);
            if over.is_empty() {
                wrong.push(format!("{name}: nothing over the frames"));
            }
            assert!(!placement(scene, &[], &opts()).unwrap().on_top, "{name}");
            let live = render_at(scene, &[], &opts(), Duration::ZERO, &filled(scene, &[], values));
            let (written, global) = written_in(scene, &[], values);
            let still = render_at(&written, &global, &opts(), Duration::ZERO, &[]);
            let d = worst(&live, &still);
            if d > 1 {
                wrong.push(format!("{name}: differs by {d}"));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("; "));
    }

    /// The expected leaves of a fixture case, in the layers of its playlist that hold them (a leaf belongs
    /// where its top-level object, the first part of its id, is), as a scene's or the global layers would
    /// be read. Only what placement looks at is filled in: ids, opacities, clips, groups, live text and
    /// media files.
    fn fixture_layers(case: &Path, layers: &Value, leaves: &Value) -> Vec<Layer> {
        let mut out: Vec<Layer> = layers
            .as_array()
            .into_iter()
            .flatten()
            .map(|l| Layer {
                id: l["id"].as_str().unwrap_or_default().into(),
                name: String::new(),
                visible: l["visible"].as_bool().unwrap_or(true),
                opacity: l["opacity"].as_f64().unwrap_or(1.0).clamp(0.0, 1.0) as f32,
                objects: Vec::new(),
            })
            .collect();
        let rect = |v: &Value| -> Option<Rect> { Some([0, 1, 2, 3].map(|i| v[i].as_i64().unwrap_or(0))).filter(|_| v.is_array()) };
        for leaf in leaves.as_array().into_iter().flatten() {
            let id = leaf["id"].as_str().unwrap().to_string();
            let top = id.split('/').next().unwrap();
            let at = layers.as_array().into_iter().flatten().position(|l| {
                l["objects"].as_array().into_iter().flatten().any(|o| o["id"].as_str() == Some(top))
            });
            let at = at.unwrap_or_else(|| panic!("{}: no layer holds {top}, for {id}", case.display()));
            let num = |k: &str| leaf[k].as_f64().unwrap_or(0.0);
            let kind = match leaf["type"].as_str() {
                Some("image") => Kind::Media(Media { src: case.join(leaf["src"].as_str().unwrap()), ..Media::default() }),
                Some("text") => {
                    let words = leaf["text"].as_str().unwrap_or_default();
                    Kind::Text(Text { id: id.clone(), kind: "text".into(), text: words.into(), live: leaf["live"] == json!(true),
                                      x: num("x") as f32, y: num("y") as f32, size: num("size") as f32, clip: rect(&leaf["clip"]),
                                      ..Text::default() })
                }
                _ => Kind::Shape(Shape::default()),
            };
            let groups = leaf["groups"].as_array().into_iter().flatten().map(|g| Group {
                id: g[0].as_str().unwrap().into(),
                opacity: g[1].as_f64().unwrap() as f32,
                rect: rect(&g[2]).unwrap(),
            });
            out[at].objects.push(Object { opacity: num("opacity") as f32, clip: rect(&leaf["clip"]), groups: groups.collect(),
                                          ..object(&id, num("x"), num("y"), num("w"), num("h"), kind) });
        }
        out
    }

    fn placement_json(p: Option<Placement>) -> Value {
        match p {
            Some(p) => json!({ "on_top": p.on_top, "cut": p.cut }),
            None => json!({ "on_top": false, "cut": null }),
        }
    }

    /// Every scene of every expand/<case> fixture has the placement its `expected.json` gives: worked out
    /// from its expected leaves, and, once the expansion gives exactly those leaves, from the playlist as
    /// it's read.
    #[test]
    fn fixtures_placement() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("widgets").join("expand");
        let mut cases: Vec<PathBuf> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().path()).filter(|p| p.is_dir()).collect();
        cases.sort();
        assert!(!cases.is_empty(), "no fixtures in {}", root.display());
        let mut checked = 0;
        for case in &cases {
            let read = |name: &str| -> Value { serde_json::from_slice(&std::fs::read(case.join(name)).unwrap()).unwrap() };
            let (expected, file) = (read("expected.json"), read("playlist/playlist.json"));
            let global = fixture_layers(case, &file["global"]["layers"], &expected["global"]["leaves"]);
            let list = case.join("playlist").join("playlist.json");
            let as_read = playlist::read(&list, 30.0, Path::new("ffmpeg")).ok();
            for want in expected["scenes"].as_array().unwrap() {
                let id = want["id"].as_str().unwrap();
                let listed = file["scenes"].as_array().unwrap().iter().find(|s| s["id"].as_str() == Some(id)).unwrap();
                let layers = fixture_layers(case, &listed["layers"], &want["leaves"]);
                let got = placement_json(placement(&scene(layers), &global, &opts()));
                let name = format!("{}, scene {id}", case.file_name().unwrap().to_string_lossy());
                assert_eq!(&want["placement"], &got, "{name}: expected.json's placement (add it as {got} when it has none)");
                checked += 1;
                // the same from the expansion itself, once it gives the expected leaves
                let drawn = |layers: &[Layer]| -> Vec<String> {
                    let objects = layers.iter().filter(|l| l.visible && l.opacity > 0.0).flat_map(|l| &l.objects);
                    objects.filter(|o| o.visible).map(|o| o.id.clone()).collect()
                };
                let ids: Vec<String> = want["leaves"].as_array().unwrap().iter().map(|l| l["id"].as_str().unwrap().to_string()).collect();
                let global_ids: Vec<String> =
                    expected["global"]["leaves"].as_array().unwrap().iter().map(|l| l["id"].as_str().unwrap().to_string()).collect();
                match as_read.as_ref().and_then(|p| Some((p, p.scenes.iter().find(|s| s.id == id)?))) {
                    Some((p, s)) if drawn(&s.layers) == ids && drawn(&p.global) == global_ids => {
                        assert_eq!(placement_json(placement(s, &p.global, &opts())), got, "{name}, as read");
                    }
                    _ => println!("{name}: the expansion doesn't give the expected leaves (yet): placement checked on them alone"),
                }
            }
        }
        assert!(checked > 0);
    }
}
