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
/// An image whose header says it's wider or taller than this isn't decoded: media from a shared widget is
/// untrusted, and nothing made for a 1600x720 panel comes near it.
const MAX_SIDE: u32 = 16384;
/// An animation is refused once its decoded frames take more than this.
const MAX_FRAMES_BYTES: usize = 2 << 30;

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

type Frames = Vec<(Arc<RgbaImage>, Duration)>;

/// An animation's frames, refused once they take more than `cap` bytes.
fn decode_frames(frames: image::Frames, cap: usize) -> Result<Frames, Box<dyn std::error::Error>> {
    let mut out = Vec::new();
    let mut size = 0;
    for f in frames {
        let f = f?;
        let (num, den) = f.delay().numer_denom_ms();
        let ms = num / den.max(1);
        let im = f.into_buffer();
        size += im.len();
        if size > cap {
            return Err(format!("its frames take more than {} MiB", cap >> 20).into());
        }
        // like browsers: a GIF delay of 0 or 10 ms means 100 ms
        out.push((Arc::new(im), Duration::from_millis(if ms <= 10 { 100 } else { ms.into() })));
    }
    Ok(out)
}

fn decode_image(bytes: &[u8]) -> Result<Frames, Box<dyn std::error::Error>> {
    let (w, h) = image::ImageReader::new(IoCursor::new(bytes)).with_guessed_format()?.into_dimensions()?;
    if w > MAX_SIDE || h > MAX_SIDE {
        return Err(format!("it says it's {w}x{h}, bigger than {MAX_SIDE}x{MAX_SIDE}").into());
    }
    let animation = |frames| decode_frames(frames, MAX_FRAMES_BYTES);
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

/// The container a video's extension names. ffmpeg reads the file as that and nothing else, so a file
/// named `.mp4` that's really a playlist of other files or addresses can't make it open them.
fn container(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "m4v" | "mov" => Some("mov"),
        "mkv" | "webm" => Some("matroska"),
        "avi" => Some("avi"),
        _ => None,
    }
}

/// What every ffmpeg run on a media file starts with: local files only, read as its extension's container.
fn input(path: &Path) -> Result<[&'static str; 4], String> {
    let format = container(path).ok_or("ffmpeg only reads .mp4, .m4v, .mov, .webm, .mkv and .avi files")?;
    Ok(["-protocol_whitelist", "file", "-f", format])
}

/// A video's size and length in seconds, from what ffmpeg prints about it.
fn probe(path: &Path, ffmpeg: &Path) -> Result<(u32, u32, f64), String> {
    let out = Command::new(ffmpeg)
        .args(input(path)?)
        .arg("-i")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("couldn't run ffmpeg ({}): {e}", ffmpeg.display()))?;
    probed(&String::from_utf8_lossy(&out.stderr))
}

/// The size of the first video stream, as ffmpeg decodes it (turned, when the file says it's turned a
/// quarter, as a phone's video is), and the length in seconds, from what `ffmpeg -i` prints.
fn probed(text: &str) -> Result<(u32, u32, f64), String> {
    let lines: Vec<&str> = text.lines().collect();
    let video = lines.iter().position(|l| l.contains("Video:"));
    let dims = video.and_then(|at| {
        lines[at].split(|c: char| c == ',' || c == ' ').find_map(|tok| {
            let (w, h) = tok.split_once('x')?;
            Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?))
        })
    });
    // its metadata and side data follow it, up to the next stream: `rotate : 90` from older ffmpegs,
    // `displaymatrix: rotation of -90.00 degrees` (`Display Matrix: …` from 7.1 on)
    let turned = video.is_some_and(|at| {
        lines[at + 1..].iter().take_while(|l| !l.trim_start().starts_with("Stream #")).any(|l| {
            let Some((key, value)) = l.split_once(':') else { return false };
            let key: String = key.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
            let angle = match key.as_str() {
                "displaymatrix" => value.trim().strip_prefix("rotation of ").and_then(|rest| rest.split_whitespace().next()),
                "rotate" => Some(value.trim()),
                _ => None,
            };
            angle.and_then(|a| a.parse::<f64>().ok()).is_some_and(|a| (a.round() as i64).rem_euclid(180) == 90)
        })
    });
    let seconds = lines.iter().find_map(|l| l.trim().strip_prefix("Duration: ")).and_then(|rest| {
        let t = rest.split(',').next()?.trim();
        let mut parts = t.split(':');
        let (h, m, s) = (parts.next()?.parse::<f64>().ok()?, parts.next()?.parse::<f64>().ok()?, parts.next()?.parse::<f64>().ok()?);
        Some(h * 3600.0 + m * 60.0 + s)
    });
    match (dims, seconds) {
        (Some((w, h)), Some(s)) if w > 0 && h > 0 => Ok(if turned { (h, w, s) } else { (w, h, s) }),
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
        // the size probe gave is the turned one, as ffmpeg turns the frames it decodes
        let mut child = Command::new(ffmpeg)
            .args(["-v", "error", "-t", &VIDEO_MAX_SECONDS.to_string()])
            .args(input(path)?)
            .arg("-i")
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

    /// A scratch folder of its own for one test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wvplay-media-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The ffmpeg tests may run (`WV_TEST_FFMPEG`), or None, said out loud, when there isn't one.
    fn test_ffmpeg(test: &str) -> Option<PathBuf> {
        let ffmpeg = std::env::var_os("WV_TEST_FFMPEG").map(PathBuf::from).filter(|p| p.is_file());
        if ffmpeg.is_none() {
            eprintln!("{test}: SKIPPED, set WV_TEST_FFMPEG to an ffmpeg to run it");
        }
        ffmpeg
    }

    fn gif(frames: &[RgbaImage]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
            for im in frames {
                encoder.encode_frame(image::Frame::new(im.clone())).unwrap();
            }
        }
        bytes
    }

    #[test]
    fn refuses_huge_headers() {
        let mut bytes = gif(&[RgbaImage::from_pixel(1, 1, Rgba([1, 2, 3, 255]))]);
        assert_eq!(decode_image(&bytes).unwrap().len(), 1);
        // the logical screen's width, as a hostile file could claim it
        bytes[6..8].copy_from_slice(&20000u16.to_le_bytes());
        let e = decode_image(&bytes).unwrap_err().to_string();
        assert!(e.contains("20000x1"), "{e}");
        let dir = scratch("huge");
        std::fs::write(dir.join("big.gif"), &bytes).unwrap();
        let e = open(&dir.join("big.gif"), Path::new("ffmpeg")).err().unwrap();
        assert!(e.contains("bigger than 16384x16384"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn animations_are_capped() {
        let frames = [RgbaImage::from_pixel(4, 4, Rgba([9, 0, 0, 255])), RgbaImage::from_pixel(4, 4, Rgba([0, 9, 0, 255]))];
        let bytes = gif(&frames);
        let read = |cap| decode_frames(GifDecoder::new(IoCursor::new(&bytes)).unwrap().into_frames(), cap);
        // 64 bytes a frame
        assert_eq!(read(128).unwrap().len(), 2);
        assert!(read(127).is_err());
    }

    #[test]
    fn bmp_decodes() {
        let src = RgbaImage::from_fn(2, 1, |x, _| Rgba([x as u8 * 200, 10, 20, 255]));
        let mut bmp = Vec::new();
        src.write_to(&mut IoCursor::new(&mut bmp), ImageFormat::Bmp).unwrap();
        let frames = decode_image(&bmp).unwrap();
        assert_eq!((frames.len(), frames[0].0.get_pixel(1, 0)), (1, &Rgba([200, 10, 20, 255])));
        // and the parity fixture's, through open()
        let chip = Path::new(env!("CARGO_MANIFEST_DIR")).join(r"tests\widgets\parity\widgets\chip\media\chip-2f03d674.bmp");
        let source = open(&chip, Path::new("ffmpeg")).unwrap();
        assert!(source.width > 0 && source.height > 0 && !source.animated());
    }

    #[test]
    fn probe_reads_turned_videos() {
        // ffmpeg 8.1, for a 16x8 file whose display matrix says it's turned a quarter
        let now = "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'turned.mp4':
  Metadata:
    major_brand     : isom
    encoder         : Lavf62.12.102
  Duration: 00:00:00.52, start: 0.000000, bitrate: 16 kb/s
  Stream #0:0[0x1](und): Video: mpeg4 (Simple Profile) (mp4v / 0x7634706D), yuv420p, 16x8 [SAR 1:1 DAR 2:1], 1 kb/s, 25 fps, 25 tbr, 12800 tbn (default)
    Metadata:
      handler_name    : VideoHandler
      encoder         : Lavc62.28.102 mpeg4
    Side data:
      Display Matrix: rotation of 90.00 degrees
At least one output file must be specified";
        assert_eq!(probed(now), Ok((8, 16, 0.52)));
        // older ffmpegs: a phone's video, with the rotate tag and a lowercase side data line
        let older = "  Duration: 00:00:02.00, start: 0.000000, bitrate: 9000 kb/s
    Stream #0:0(und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(tv, bt709), 1920x1080, 9000 kb/s, 30 fps (default)
    Metadata:
      rotate          : 90
      handler_name    : VideoHandle
    Side data:
      displaymatrix: rotation of -90.00 degrees
    Stream #0:1(und): Audio: aac (LC) (mp4a / 0x6134706D), 48000 Hz, stereo";
        assert_eq!(probed(older), Ok((1080, 1920, 2.0)));
        let tag_only = older.replace("      displaymatrix: rotation of -90.00 degrees\n", "");
        assert_eq!(probed(&tag_only), Ok((1080, 1920, 2.0)));
        let matrix_only = older.replace("      rotate          : 90\n", "");
        assert_eq!(probed(&matrix_only), Ok((1080, 1920, 2.0)));
        // half a turn keeps the shape; so does a turn that belongs to another stream
        assert_eq!(probed(&older.replace("90", "180")), Ok((1920, 1080, 2.0)));
        let later = "  Duration: 00:00:02.00, start: 0.000000
    Stream #0:0: Video: h264, yuv420p, 640x480, 30 fps
    Stream #0:1: Video: mjpeg, 100x100 (attached pic)
    Side data:
      displaymatrix: rotation of -90.00 degrees";
        assert_eq!(probed(later), Ok((640, 480, 2.0)));
        assert!(probed("Error opening input file x.mp4.").is_err());
    }

    #[test]
    fn formats_by_extension() {
        for (name, format) in [("a.mp4", Some("mov")), ("a.M4V", Some("mov")), ("a.mov", Some("mov")), ("a.mkv", Some("matroska")),
                               ("a.WebM", Some("matroska")), ("a.avi", Some("avi")), ("a.gif", None), ("mp4", None), ("a.mp4.m3u8", None)] {
            assert_eq!(container(Path::new(name)), format, "{name}");
        }
        assert_eq!(input(Path::new("a.webm")), Ok(["-protocol_whitelist", "file", "-f", "matroska"]));
        assert!(input(Path::new("a.m3u8")).is_err());
    }

    /// Videos are read as the container their extension names, at the size ffmpeg decodes them.
    #[test]
    fn videos_are_read_as_their_extension_says() {
        let Some(ffmpeg) = test_ffmpeg("videos_are_read_as_their_extension_says") else { return };
        let dir = scratch("videos");
        let make = |args: &[&str]| {
            let ok = Command::new(&ffmpeg).args(["-v", "error", "-y"]).args(args).current_dir(&dir).status().unwrap().success();
            assert!(ok, "ffmpeg {args:?}");
        };
        make(&["-f", "lavfi", "-i", "color=c=red:s=16x8:d=0.5", "-c:v", "mpeg4", "plain.mp4"]);
        make(&["-display_rotation", "90", "-i", "plain.mp4", "-c", "copy", "turned.mp4"]);
        make(&["-f", "lavfi", "-i", "color=c=blue:s=16x8:d=0.5", "-c:v", "ffv1", "m.mkv"]);
        std::fs::copy(dir.join("m.mkv"), dir.join("mkv.mp4")).unwrap();
        std::fs::write(dir.join("list.mp4"), "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nplain.mp4\n#EXT-X-ENDLIST\n").unwrap();

        let (w, h, seconds) = probe(&dir.join("plain.mp4"), &ffmpeg).unwrap();
        assert_eq!((w, h), (16, 8));
        assert!(seconds > 0.4 && seconds < 0.6, "{seconds}");
        assert!(probe(&dir.join("m.mkv"), &ffmpeg).is_ok());
        // turned: read at the turned size, and its frames come out that shape
        assert_eq!(dimensions(&dir.join("turned.mp4"), &ffmpeg), Some((8, 16)));
        let source = open(&dir.join("turned.mp4"), &ffmpeg).unwrap();
        let view = View { rotate: 0, flip: false, w: 8, h: 16, smooth: false, tint: None };
        let mut stream = Stream::start(&dir.join("turned.mp4"), &ffmpeg, &source, view).unwrap();
        let frame = stream.next().unwrap();
        assert_eq!(frame.dimensions(), (8, 16));
        assert!(frame.get_pixel(4, 8)[0] > 200, "red");
        drop(stream);
        // a Matroska file named .mp4, and a playlist of other files named .mp4, don't open
        for name in ["mkv.mp4", "list.mp4"] {
            assert!(probe(&dir.join(name), &ffmpeg).is_err(), "{name}");
            assert!(open(&dir.join(name), &ffmpeg).is_err(), "{name}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
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
