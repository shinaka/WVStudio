//! Draws scenes (docs/scenes.md): the background, then each visible layer's objects into a transparent
//! buffer, blended over the frame at the layer's opacity, with the global layers on top.
//!
//! Static work happens once. Before the frames are made, the stack is sorted into static and animated
//! parts: everything below the first animated layer is flattened into one RGB base, each static run
//! above into one RGBA still, and only animated layers are drawn per frame, and then only when one of
//! their objects' frames has changed. Static layer buffers are also kept across re-prepares, keyed on
//! the layer's content, so editing one layer draws only that layer again.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::cell::Cell;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use image::{RgbImage, RgbaImage};

use crate::art::{self, HEIGHT, Options, Prepared, WIDTH};
use crate::media::{self, Cursor, Source, View};
use crate::playlist::{Fill, Kind, Layer, Object, Scene, Shape, Text, stamp};
use crate::text::{self, DEFAULT_FONT};
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

    /// The part of a whole-canvas buffer inside the pixel box.
    fn crop(canvas: &RgbaImage, rect: Option<(i64, i64, i64, i64)>) -> Buf {
        let Some((l, t, r, b)) = rect else { return Buf::empty() };
        let (l, t) = (l.clamp(0, WIDTH as i64) as u32, t.clamp(0, HEIGHT as i64) as u32);
        let (r, b) = (r.clamp(0, WIDTH as i64) as u32, b.clamp(0, HEIGHT as i64) as u32);
        if l >= r || t >= b {
            return Buf::empty();
        }
        Buf { left: l, top: t, image: image::imageops::crop_imm(canvas, l, t, r - l, b - t).to_image() }
    }

    /// `crop`, taking the canvas itself when the whole of it was touched.
    fn take(canvas: RgbaImage, rect: Option<(i64, i64, i64, i64)>) -> Buf {
        match rect {
            Some((l, t, r, b)) if l <= 0 && t <= 0 && r >= WIDTH as i64 && b >= HEIGHT as i64 => Buf { left: 0, top: 0, image: canvas },
            _ => Buf::crop(&canvas, rect),
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
        // the usual video layer: one opaque picture in its box is the buffer itself, with no drawing
        if let [obj] = &mut self.objects[..]
            && obj.object.opacity >= 1.0
            && let index = obj.index_at(t)
            && let ObjKind::Media { cursor, fill: Fill::None, .. } = &mut obj.kind
        {
            let frame = cursor.frame(index);
            if frame.pixels().all(|p| p[3] == 255) {
                let (x, y) = (obj.object.x.round() as i64, obj.object.y.round() as i64);
                let (l, t) = (x.max(0), y.max(0));
                let (r, b) = ((x + frame.width() as i64).min(WIDTH as i64), (y + frame.height() as i64).min(HEIGHT as i64));
                if l >= r || t >= b {
                    return Buf::empty();
                }
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
    let _ = depth; // no object is in a group yet
    let mut canvas = RgbaImage::new(size.0, size.1);
    let mut touched: Option<(i64, i64, i64, i64)> = None;
    for obj in objs {
        let index = obj.index_at(t);
        let object = obj.object;
        let rect = match &mut obj.kind {
            ObjKind::Media { cursor, fill, .. } => {
                let frame = cursor.frame(index);
                draw_media(&mut canvas, origin, object, &frame, *fill)
            }
            ObjKind::Text(w) => {
                let layer = text::Layer::render([*w]);
                layer.bounds().map(|b| {
                    let buf = Buf::crop(layer.image(), Some((b.left as i64, b.top as i64, b.right as i64, b.bottom as i64)));
                    buf.onto_rgba(&mut canvas, origin, 1.0);
                    (b.left as i64, b.top as i64, b.right as i64, b.bottom as i64)
                })
            }
            ObjKind::Shape(s) => shape::draw(&mut canvas, origin, object, s),
        };
        touched = union(touched, rect);
    }
    Buf::take(canvas, touched)
}

fn union(a: Option<(i64, i64, i64, i64)>, b: Option<(i64, i64, i64, i64)>) -> Option<(i64, i64, i64, i64)> {
    match (a, b) {
        (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3))),
        (a, None) => a,
        (None, b) => b,
    }
}

/// Draws a turned and scaled frame at the object's box, repeated outward as its fill mode says, at the
/// object's opacity, into `canvas`, whose top-left is `origin` on the panel. Returns the pixel box touched,
/// in panel pixels.
fn draw_media(canvas: &mut RgbaImage, origin: (i64, i64), o: &Object, frame: &RgbaImage, fill: Fill) -> Option<(i64, i64, i64, i64)> {
    let (x, y) = (o.x.round() as i64, o.y.round() as i64);
    let (w, h) = (frame.width() as i64, frame.height() as i64);
    let k = (o.opacity.clamp(0.0, 1.0) * 256.0).round() as u32;
    if w == 0 || h == 0 || k == 0 {
        return None;
    }
    // the canvas, in panel pixels
    let (cl, ct) = origin;
    let (cr, cb) = (cl + canvas.width() as i64, ct + canvas.height() as i64);
    let (l, t, r, b) = match fill {
        Fill::None => (x.max(cl), y.max(ct), (x + w).min(cr), (y + h).min(cb)),
        _ => (cl, ct, cr, cb),
    };
    if l >= r || t >= b {
        return None;
    }
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

/// The scene's layers and the global ones, ready to draw, sorted around the live text (docs/scenes.md):
/// `below` goes into the prepared frames; `above` is what's kept as a still over the live text, when
/// everything above it is static. Live text itself is left out of both.
struct Plan<'a> {
    below: Vec<Slot<'a>>,
    above: Option<Vec<Slot<'a>>>,
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
                    let view = View { rotate: o.rotate, flip: o.flip, w: o.w.round().max(1.0) as u32, h: o.h.round().max(1.0) as u32, smooth: m.smooth,
                                      tint: None };
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

/// Sorts the slots around the live text, which is left out of them, and keys them for the buffer cache.
fn split_live<'a>(slots: Vec<Slot<'a>>, opts: &Options) -> Plan<'a> {
    // where each slot's live text is
    let live: Vec<Vec<usize>> = slots.iter().map(|s| (0..s.objects.len()).filter(|j| s.objects[*j].live()).collect()).collect();
    // the lowest live text, and whether only still things sit above it
    let first_live = live.iter().position(|l| !l.is_empty());
    let still_above = first_live.is_some_and(|p| slots[p..].iter().all(|s| !s.animated()));
    let mut below = Vec::new();
    let mut above = Vec::new();
    for (i, (mut slot, live)) in slots.into_iter().zip(live).enumerate() {
        let split = match first_live {
            Some(p) if still_above && i == p => live[0],
            _ => usize::MAX,
        };
        let mut upper = Vec::new();
        let mut objects = Vec::new();
        for (j, obj) in slot.objects.into_iter().enumerate() {
            if live.contains(&j) {
                continue;
            }
            if j > split { upper.push(obj) } else { objects.push(obj) }
        }
        slot.objects = objects;
        if first_live.is_some_and(|p| still_above && i > p) {
            above.push(slot);
        } else {
            below.push(slot);
            if split != usize::MAX {
                above.push(Slot { opacity: below.last().unwrap().opacity, objects: upper, key: 0 });
            }
        }
    }
    for slot in below.iter_mut().chain(&mut above) {
        slot.key = slot_key(slot, opts);
    }
    Plan { below, above: still_above.then_some(above) }
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
    Buf::take(canvas, touched)
}

/// The scene's cycle of frames, encoded for the panel, plus the still kept over its live text.
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
    let above = plan.above.as_mut().map(|slots| flatten(slots)).filter(|b| !b.is_empty());
    Ok(Prepared { frames, above })
}

/// The scene at time `t`, upright, with `live` (its live text, filled in) drawn where it belongs: what
/// the cooler would show, for checking without it.
pub fn render_at(scene: &Scene, global: &[Layer], opts: &Options, t: Duration, live: &[Text]) -> RgbImage {
    let mut plan = plan(scene, global, opts);
    let mut frame = background(scene);
    for slot in &mut plan.below {
        let buf = if slot.animated() { slot.render(t) } else { (*slot.render_cached()).clone() };
        buf.onto_rgb(&mut frame, slot.opacity, false);
    }
    text::Layer::render(live).draw_onto(&mut frame);
    if let Some(slots) = plan.above.as_mut() {
        flatten(slots).onto_rgb(&mut frame, 1.0, false);
    }
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
        let above = prepared.above.as_ref().expect("the cover should be kept as a still");
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
        assert!(prepared.above.is_some());
        // an animation above it: nothing kept, the text goes on top
        let gif = r#"{ "id": "g", "type": "image", "src": "a.gif", "x": 90, "y": 90, "w": 400, "h": 100 }"#.to_string();
        let p = s.playlist(&scene(&[below, layer("live", 1.0, &[live]), layer("anim", 1.0, &[gif])]), "");
        let prepared = prepare(&p.scenes[0], &p.global, &opts()).unwrap();
        assert!(prepared.above.is_none());
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
#[cfg(test)]
mod drawing_tests {}

/// Live text laid over the frames: the over list (docs/widgets.md, Live text). Owned by work package P3.
#[cfg(test)]
mod over_tests {}
