//! Image, animation and video files for scene objects: each decoded once per file, and turned, flipped
//! and scaled once per size, then shared between objects and across re-prepares.
//!
//! Images are kept whole (a pixel-art loop is small). Video is streamed from ffmpeg at its own size as
//! frames are asked for in order, since a minute of 1080p doesn't fit in memory.

use std::collections::HashMap;
use std::io::{Cursor as IoCursor, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, SystemTime};

use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::imageops::{self, FilterType};
use image::{AnimationDecoder, ImageFormat, RgbaImage};

use crate::art::VIDEO_TYPES;
use crate::playlist::stamp;

const VIDEO_FPS: u32 = 30;
const VIDEO_MAX_SECONDS: f64 = 60.0;
/// Decoded files kept in memory: a 1600x720 frame is 4.6 MB, so this is a few big loops.
const SOURCE_BYTES: usize = 512 << 20;
/// Scaled frames kept in memory.
const SCALED_BYTES: usize = 384 << 20;

type Stamp = Option<(SystemTime, u64)>;

/// A decoded file.
pub struct Source {
    pub width: u32,
    pub height: u32,
    kind: Kind,
}

enum Kind {
    Frames(Vec<(Arc<RgbaImage>, Duration)>),
    /// Read again from ffmpeg each time its frames are wanted; `count` is what the file's length says.
    Video { path: PathBuf, ffmpeg: PathBuf, count: usize },
}

impl Source {
    pub fn count(&self) -> usize {
        match &self.kind {
            Kind::Frames(f) => f.len(),
            Kind::Video { count, .. } => *count,
        }
    }

    pub fn animated(&self) -> bool {
        self.count() > 1
    }

    pub fn is_video(&self) -> bool {
        matches!(self.kind, Kind::Video { .. })
    }

    /// How long each frame shows.
    pub fn durations(&self) -> Vec<Duration> {
        match &self.kind {
            Kind::Frames(f) => f.iter().map(|(_, d)| *d).collect(),
            Kind::Video { count, .. } => vec![frame_time(); *count],
        }
    }

    fn bytes(&self) -> usize {
        match &self.kind {
            Kind::Frames(f) => f.iter().map(|(im, _)| im.len()).sum(),
            Kind::Video { .. } => 0,
        }
    }
}

fn frame_time() -> Duration {
    Duration::from_secs(1) / VIDEO_FPS
}

pub fn is_video(path: &Path) -> bool {
    path.extension().is_some_and(|e| VIDEO_TYPES.iter().any(|t| e.eq_ignore_ascii_case(t)))
}

/// How an object shows its file: turned, flipped, scaled to `w`x`h`, then tinted. It keys the scaled
/// frames and the video streams, so a tinted and an untinted view of one file are kept apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct View {
    pub rotate: u16,
    pub flip: bool,
    pub w: u32,
    pub h: u32,
    pub smooth: bool,
    /// Every pixel then takes this colour, keeping its own alpha (docs/widgets.md, Images in a box).
    pub tint: Option<[u8; 3]>,
}

/// Decoded files by path and stamp, with the scaled versions of each view. `used` orders evictions.
/// Video streams are shared by the objects showing the same file the same way, for as long as any does.
#[derive(Default)]
struct Caches {
    sources: HashMap<(PathBuf, Stamp), (u64, Arc<Source>)>,
    scaled: HashMap<(PathBuf, Stamp, View), (u64, Arc<Scaled>)>,
    videos: HashMap<(PathBuf, Stamp, View), Weak<Mutex<Shared>>>,
    tick: u64,
}

/// One ffmpeg stream of a video in a view, walked in time order by every object that shows it (they
/// ask for the same frame at the same moment).
#[derive(Default)]
struct Shared {
    stream: Option<(Stream, usize)>,
    last: Option<(usize, Arc<RgbaImage>)>,
}

/// A view's frames, made as they're first asked for.
struct Scaled {
    frames: Mutex<Vec<Option<Arc<RgbaImage>>>>,
}

impl Scaled {
    fn bytes(&self) -> usize {
        self.frames.lock().unwrap().iter().flatten().map(|im| im.len()).sum()
    }
}

fn caches() -> &'static Mutex<Caches> {
    static CACHES: OnceLock<Mutex<Caches>> = OnceLock::new();
    CACHES.get_or_init(Default::default)
}

/// Drops the least recently used entries until the cache fits, keeping the one just used.
fn trim<K: Clone + Eq + std::hash::Hash, V>(map: &mut HashMap<K, (u64, V)>, size: impl Fn(&V) -> usize, cap: usize, keep: &K) {
    while map.len() > 1 && map.values().map(|(_, v)| size(v)).sum::<usize>() > cap {
        let victim = map.iter().filter(|(k, _)| *k != keep).min_by_key(|(_, (used, _))| *used).map(|(k, _)| k.clone());
        match victim {
            Some(k) => map.remove(&k),
            None => break,
        };
    }
}

/// The file decoded, from memory when it's there.
pub fn open(path: &Path, ffmpeg: &Path) -> Result<Arc<Source>, String> {
    let key = (path.to_path_buf(), stamp(path));
    let mut c = caches().lock().unwrap();
    c.tick += 1;
    let tick = c.tick;
    if let Some((used, source)) = c.sources.get_mut(&key) {
        *used = tick;
        return Ok(source.clone());
    }
    drop(c);
    let source = Arc::new(decode(path, ffmpeg).map_err(|e| format!("{}: {e}", path.display()))?);
    let mut c = caches().lock().unwrap();
    let tick = c.tick;
    c.sources.insert(key.clone(), (tick, source.clone()));
    trim(&mut c.sources, |s| s.bytes(), SOURCE_BYTES, &key);
    Ok(source)
}

/// A file's pixel size, without decoding its frames (video is probed with ffmpeg).
pub fn dimensions(path: &Path, ffmpeg: &Path) -> Option<(u32, u32)> {
    if is_video(path) {
        probe(path, ffmpeg).ok().map(|(w, h, _)| (w, h))
    } else {
        image::image_dimensions(path).ok()
    }
}

fn decode(path: &Path, ffmpeg: &Path) -> Result<Source, String> {
    if is_video(path) {
        let (width, height, seconds) = probe(path, ffmpeg)?;
        let count = ((seconds.min(VIDEO_MAX_SECONDS) * VIDEO_FPS as f64).round() as usize).max(1);
        let kind = Kind::Video { path: path.to_path_buf(), ffmpeg: ffmpeg.to_path_buf(), count };
        return Ok(Source { width, height, kind });
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let frames = decode_image(&bytes).map_err(|e| e.to_string())?;
    let (width, height) = frames.first().map(|(im, _)| im.dimensions()).ok_or("no frames in it")?;
    Ok(Source { width, height, kind: Kind::Frames(frames) })
}

fn decode_image(bytes: &[u8]) -> Result<Vec<(Arc<RgbaImage>, Duration)>, Box<dyn std::error::Error>> {
    let animation = |frames: image::Frames| -> Result<Vec<(Arc<RgbaImage>, Duration)>, Box<dyn std::error::Error>> {
        let mut out = Vec::new();
        for f in frames {
            let f = f?;
            let (num, den) = f.delay().numer_denom_ms();
            let ms = num / den.max(1);
            // like browsers: a GIF delay of 0 or 10 ms means 100 ms
            out.push((Arc::new(f.into_buffer()), Duration::from_millis(if ms <= 10 { 100 } else { ms.into() })));
        }
        Ok(out)
    };
    match image::guess_format(bytes)? {
        ImageFormat::Gif => animation(GifDecoder::new(IoCursor::new(bytes))?.into_frames()),
        ImageFormat::Png if PngDecoder::new(IoCursor::new(bytes))?.is_apng()? => {
            animation(PngDecoder::new(IoCursor::new(bytes))?.apng()?.into_frames())
        }
        ImageFormat::WebP if WebPDecoder::new(IoCursor::new(bytes))?.has_animation() => {
            animation(WebPDecoder::new(IoCursor::new(bytes))?.into_frames())
        }
        _ => Ok(vec![(Arc::new(image::load_from_memory(bytes)?.into_rgba8()), Duration::from_secs(1))]),
    }
}

/// A video's size and length in seconds, from what ffmpeg prints about it.
fn probe(path: &Path, ffmpeg: &Path) -> Result<(u32, u32, f64), String> {
    let out = Command::new(ffmpeg)
        .arg("-i")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("couldn't run ffmpeg ({}): {e}", ffmpeg.display()))?;
    let text = String::from_utf8_lossy(&out.stderr);
    let dims = text.lines().find(|l| l.contains("Video:")).and_then(|line| {
        line.split(|c: char| c == ',' || c == ' ').find_map(|tok| {
            let (w, h) = tok.split_once('x')?;
            Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?))
        })
    });
    let seconds = text.lines().find_map(|l| l.trim().strip_prefix("Duration: ")).and_then(|rest| {
        let t = rest.split(',').next()?.trim();
        let mut parts = t.split(':');
        let (h, m, s) = (parts.next()?.parse::<f64>().ok()?, parts.next()?.parse::<f64>().ok()?, parts.next()?.parse::<f64>().ok()?);
        Some(h * 3600.0 + m * 60.0 + s)
    });
    match (dims, seconds) {
        (Some((w, h)), Some(s)) if w > 0 && h > 0 => Ok((w, h, s)),
        _ => Err(format!("ffmpeg can't read it: {}", text.lines().last().unwrap_or("").trim())),
    }
}

/// Frames of ffmpeg's output, in order: turned and flipped, and scaled to the box when smooth (ffmpeg's
/// nearest neighbour isn't the spec's rule, so that stays in Rust, at the file's own size here).
struct Stream {
    child: Child,
    out: ChildStdout,
    width: u32,
    height: u32,
}

impl Stream {
    fn start(path: &Path, ffmpeg: &Path, source: &Source, view: View) -> Result<Stream, String> {
        let mut filters = vec![format!("fps={VIDEO_FPS}")];
        match view.rotate {
            90 => filters.push("transpose=1".into()),
            180 => filters.push("hflip,vflip".into()),
            270 => filters.push("transpose=2".into()),
            _ => {}
        }
        if view.flip {
            filters.push("hflip".into());
        }
        let (mut width, mut height) = if view.rotate % 180 == 90 { (source.height, source.width) } else { (source.width, source.height) };
        if view.smooth {
            (width, height) = (view.w.max(1), view.h.max(1));
            filters.push(format!("scale={width}:{height}:flags=lanczos"));
        }
        filters.push("format=rgba".into());
        let mut child = Command::new(ffmpeg)
            .args(["-v", "error", "-t", &VIDEO_MAX_SECONDS.to_string(), "-i"])
            .arg(path)
            .args(["-an", "-vf", &filters.join(","), "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("couldn't run ffmpeg ({}): {e}", ffmpeg.display()))?;
        let out = child.stdout.take().unwrap();
        Ok(Stream { child, out, width, height })
    }

    fn next(&mut self) -> Option<RgbaImage> {
        let mut buf = vec![0u8; (self.width * self.height * 4) as usize];
        self.out.read_exact(&mut buf).ok()?;
        RgbaImage::from_raw(self.width, self.height, buf)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One object's way through its file's frames, turned and scaled. Frames are asked for in time order
/// (a loop restarts the video's stream).
pub struct Cursor {
    source: Arc<Source>,
    view: View,
    scaled: Option<Arc<Scaled>>,
    video: Option<Arc<Mutex<Shared>>>,
    last: Option<(usize, Arc<RgbaImage>)>,
}

impl Cursor {
    pub fn new(path: &Path, source: &Arc<Source>, view: View) -> Cursor {
        let key = (path.to_path_buf(), stamp(path), view);
        let mut c = caches().lock().unwrap();
        c.tick += 1;
        let tick = c.tick;
        let (scaled, video) = if source.is_video() {
            let shared = c.videos.get(&key).and_then(Weak::upgrade).unwrap_or_default();
            c.videos.insert(key, Arc::downgrade(&shared));
            c.videos.retain(|_, w| w.strong_count() > 0);
            (None, Some(shared))
        } else {
            let scaled = c
                .scaled
                .entry(key)
                .or_insert_with(|| (tick, Arc::new(Scaled { frames: Mutex::new(vec![None; source.count()]) })))
                .1
                .clone();
            (Some(scaled), None)
        };
        Cursor { source: source.clone(), view, scaled, video, last: None }
    }

    /// Frame `index` as the object shows it: exactly `view.w` x `view.h`, with the tint applied last.
    /// Past the end of a video (which may be a frame shorter than its length says), the last frame stays.
    pub fn frame(&mut self, index: usize) -> Arc<RgbaImage> {
        if let Some((at, im)) = &self.last
            && *at == index
        {
            return im.clone();
        }
        let im = match &self.source.kind {
            Kind::Frames(frames) => {
                let index = index.min(frames.len() - 1);
                let scaled = self.scaled.as_ref().unwrap();
                let have = scaled.frames.lock().unwrap()[index].clone();
                have.unwrap_or_else(|| {
                    let im = Arc::new(transform(&frames[index].0, self.view));
                    scaled.frames.lock().unwrap()[index] = Some(im.clone());
                    let mut c = caches().lock().unwrap();
                    let keep = c.scaled.iter().find(|(_, (_, s))| Arc::ptr_eq(s, scaled)).map(|(k, _)| k.clone());
                    if let Some(keep) = keep {
                        trim(&mut c.scaled, |s| s.bytes(), SCALED_BYTES, &keep);
                    }
                    im
                })
            }
            Kind::Video { path, ffmpeg, .. } => {
                let shared = self.video.as_ref().unwrap();
                let mut s = shared.lock().unwrap();
                if let Some((at, im)) = &s.last
                    && *at == index
                {
                    return im.clone(); // another object showing the same video just asked for it
                }
                // going back means starting the stream again
                if s.stream.as_ref().is_none_or(|(_, next)| *next > index) {
                    s.stream = Stream::start(path, ffmpeg, &self.source, self.view).ok().map(|st| (st, 0));
                }
                let mut raw = None;
                if let Some((stream, next)) = &mut s.stream {
                    while *next <= index {
                        match stream.next() {
                            Some(frame) => raw = Some(frame),
                            None => break,
                        }
                        *next += 1;
                    }
                }
                let im = match raw {
                    // turned and flipped already; only nearest-neighbour scaling is left to do
                    Some(raw) => Arc::new(transform(&raw, View { rotate: 0, flip: false, ..self.view })),
                    None => match s.last.as_ref().or(self.last.as_ref()) {
                        Some((_, im)) => im.clone(),
                        None => Arc::new(RgbaImage::new(self.view.w.max(1), self.view.h.max(1))),
                    },
                };
                s.last = Some((index, im.clone()));
                im
            }
        };
        self.last = Some((index, im.clone()));
        im
    }
}

/// The file's pixels turned, flipped, scaled and tinted as the object shows them.
pub fn transform(im: &RgbaImage, view: View) -> RgbaImage {
    let turned = match view.rotate {
        90 => imageops::rotate90(im),
        180 => imageops::rotate180(im),
        270 => imageops::rotate270(im),
        _ => im.clone(),
    };
    let turned = if view.flip { imageops::flip_horizontal(&turned) } else { turned };
    let (w, h) = (view.w.max(1), view.h.max(1));
    let mut out = if turned.dimensions() == (w, h) {
        turned
    } else if view.smooth {
        imageops::resize(&turned, w, h, FilterType::Lanczos3)
    } else {
        scale_nearest(&turned, w, h)
    };
    if let Some(colour) = view.tint {
        tint(&mut out, colour);
    }
    out
}

/// Every pixel takes `colour` and keeps its own alpha, so a white or black icon on a transparent
/// background becomes an icon in that colour. Applied to the final pixels, after the one resample.
fn tint(im: &mut RgbaImage, colour: [u8; 3]) {
    for p in im.pixels_mut() {
        p.0 = [colour[0], colour[1], colour[2], p[3]];
    }
}

/// Nearest neighbour by the spec's rule: screen pixel `u` of `w` shows source pixel
/// `floor((2u + 1) * srcw / (2w))`, and the same for rows, so the studio's preview matches exactly.
pub fn scale_nearest(src: &RgbaImage, w: u32, h: u32) -> RgbaImage {
    let (sw, sh) = (src.width() as u64, src.height() as u64);
    let col: Vec<u32> = (0..w as u64).map(|u| ((2 * u + 1) * sw / (2 * w as u64)) as u32).collect();
    let row: Vec<u32> = (0..h as u64).map(|v| ((2 * v + 1) * sh / (2 * h as u64)) as u32).collect();
    let mut out = RgbaImage::new(w, h);
    for (y, &sy) in row.iter().enumerate() {
        let src_row = &src.as_raw()[(sy as usize * sw as usize) * 4..];
        let dst_row = &mut out.as_mut()[y * w as usize * 4..(y + 1) * w as usize * 4];
        for (x, &sx) in col.iter().enumerate() {
            dst_row[x * 4..x * 4 + 4].copy_from_slice(&src_row[sx as usize * 4..sx as usize * 4 + 4]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    /// Each pixel picks its source by the exact rule, checked against a plain float version.
    #[test]
    fn nearest_matches_the_rule() {
        for (sw, sh, w, h) in [(3, 2, 7, 5), (480, 254, 1789, 805), (10, 10, 3, 4), (7, 3, 7, 3), (2, 2, 1, 1)] {
            let src = RgbaImage::from_fn(sw, sh, |x, y| Rgba([x as u8, y as u8, (x * 7 + y) as u8, 255]));
            let out = scale_nearest(&src, w, h);
            for v in 0..h {
                for u in 0..w {
                    let sx = ((2.0 * u as f64 + 1.0) * sw as f64 / (2.0 * w as f64)).floor() as u32;
                    let sy = ((2.0 * v as f64 + 1.0) * sh as f64 / (2.0 * h as f64)).floor() as u32;
                    assert_eq!(out.get_pixel(u, v), src.get_pixel(sx, sy), "{sw}x{sh} -> {w}x{h} at {u},{v}");
                }
            }
        }
    }

    #[test]
    fn turns_then_flips() {
        let src = RgbaImage::from_fn(2, 1, |x, _| Rgba([x as u8 * 100, 0, 0, 255]));
        let view = View { rotate: 90, flip: false, w: 1, h: 2, smooth: false, tint: None };
        let t = transform(&src, view);
        assert_eq!((t.get_pixel(0, 0)[0], t.get_pixel(0, 1)[0]), (0, 100)); // clockwise: left end goes to the top
        let view = View { rotate: 0, flip: true, w: 4, h: 1, smooth: false, tint: None };
        let t = transform(&src, view);
        assert_eq!(t.as_raw().iter().step_by(4).copied().collect::<Vec<_>>(), [100, 100, 0, 0]);
    }
}
