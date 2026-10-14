//! Turns an image, animation or video into ready-to-send JPEG frames.
//!
//! Everything expensive (decoding, fitting, rotating, JPEG encoding) happens once, here, and the result
//! is kept on disk (see cache.rs). Playing is then just sending stored bytes on a timer.

use std::error::Error;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::imageops::{self, FilterType};
use image::{AnimationDecoder, ImageFormat, RgbImage, RgbaImage};
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};

use crate::cache;

/// What the panel takes. It shows this upscaled 1.5x on its 2400x1080 glass; a native 2400x1080
/// frame is accepted but its outer columns flicker.
pub const WIDTH: u32 = 1600;
pub const HEIGHT: u32 = 720;
/// TRCC drops frames of 450 KB or more, so stay under that.
const MAX_JPEG: usize = 400_000;
pub const VIDEO_TYPES: [&str; 6] = ["mp4", "m4v", "mov", "webm", "mkv", "avi"];
const VIDEO_FPS: u32 = 30;
const VIDEO_MAX_SECONDS: u32 = 60;
/// Frames decoded at a time before encoding: enough to keep every core busy, little enough to stay
/// light on memory (a 1600x720 frame is 3.5 MB raw).
const BATCH: usize = 64;

pub struct Frame {
    pub jpeg: Vec<u8>,
    pub duration: Duration,
}

#[derive(Clone)]
pub struct Options {
    pub rotate180: bool,
    pub quality: u8,
    pub ffmpeg: PathBuf,
}

/// A decoded frame waiting to be encoded: images still need fitting to the panel, video comes out fitted.
enum Raw {
    Image(RgbaImage, Duration),
    Fitted(RgbImage, Duration),
}

pub fn load(path: &Path, opts: &Options) -> Result<Vec<Frame>, Box<dyn Error>> {
    if let Some(frames) = cache::read(path, opts) {
        return Ok(frames);
    }
    let video = path.extension().is_some_and(|e| VIDEO_TYPES.iter().any(|t| e.eq_ignore_ascii_case(t)));
    // full-resolution colour keeps coloured pixel-art edges sharp; video doesn't need it
    let sampling = if video { SamplingFactor::R_4_2_0 } else { SamplingFactor::R_4_4_4 };
    let mut frames = Vec::new();
    let mut batch = Vec::with_capacity(BATCH);
    let mut flush = |batch: &mut Vec<Raw>| -> Result<(), Box<dyn Error>> {
        frames.extend(encode_all(std::mem::take(batch), opts, sampling)?);
        Ok(())
    };
    let mut push = |raw: Raw| -> Result<(), Box<dyn Error>> {
        batch.push(raw);
        if batch.len() == BATCH {
            flush(&mut batch)?;
        }
        Ok(())
    };
    if video {
        decode_video(path, opts, &mut push)?;
    } else {
        decode_image(&std::fs::read(path)?, &mut push)?;
    }
    flush(&mut batch)?;
    if frames.is_empty() {
        return Err("no frames in it".into());
    }
    cache::write(path, opts, &frames);
    Ok(frames)
}

/// Encodes a batch on every core.
fn encode_all(mut batch: Vec<Raw>, opts: &Options, sampling: SamplingFactor) -> Result<Vec<Frame>, String> {
    if batch.is_empty() {
        return Ok(Vec::new());
    }
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let (count, chunk) = (batch.len(), batch.len().div_ceil(threads));
    std::thread::scope(|s| {
        let workers: Vec<_> = batch
            .chunks_mut(chunk)
            .map(|part| {
                s.spawn(move || {
                    part.iter_mut()
                        .map(|raw| {
                            let (mut rgb, duration) = match std::mem::replace(raw, Raw::Fitted(RgbImage::new(0, 0), Duration::ZERO)) {
                                Raw::Image(im, d) => (fit(&im), d),
                                Raw::Fitted(rgb, d) => (rgb, d),
                            };
                            if opts.rotate180 {
                                imageops::rotate180_in_place(&mut rgb);
                            }
                            Ok(Frame { jpeg: encode(&rgb, opts.quality, sampling)?, duration })
                        })
                        .collect::<Result<Vec<_>, String>>()
                })
            })
            .collect();
        let mut frames = Vec::with_capacity(count);
        for w in workers {
            frames.extend(w.join().map_err(|_| "encoder thread panicked")??);
        }
        Ok(frames)
    })
}

fn decode_image(bytes: &[u8], push: &mut dyn FnMut(Raw) -> Result<(), Box<dyn Error>>) -> Result<(), Box<dyn Error>> {
    let mut animation = |frames: image::Frames| -> Result<(), Box<dyn Error>> {
        for f in frames {
            let f = f?;
            let (num, den) = f.delay().numer_denom_ms();
            let ms = num / den.max(1);
            // like browsers: a GIF delay of 0 or 10 ms means 100 ms
            push(Raw::Image(f.into_buffer(), Duration::from_millis(if ms <= 10 { 100 } else { ms.into() })))?;
        }
        Ok(())
    };
    match image::guess_format(bytes)? {
        ImageFormat::Gif => animation(GifDecoder::new(Cursor::new(bytes))?.into_frames()),
        ImageFormat::Png if PngDecoder::new(Cursor::new(bytes))?.is_apng()? => {
            animation(PngDecoder::new(Cursor::new(bytes))?.apng()?.into_frames())
        }
        ImageFormat::WebP if WebPDecoder::new(Cursor::new(bytes))?.has_animation() => {
            animation(WebPDecoder::new(Cursor::new(bytes))?.into_frames())
        }
        _ => push(Raw::Image(image::load_from_memory(bytes)?.into_rgba8(), Duration::from_secs(1))),
    }
}

/// ffmpeg does the decoding, fitting (letterboxed, never cropped) and frame-rate conversion.
fn decode_video(path: &Path, opts: &Options, push: &mut dyn FnMut(Raw) -> Result<(), Box<dyn Error>>)
    -> Result<(), Box<dyn Error>> {
    let filter = format!(
        "fps={VIDEO_FPS},scale={WIDTH}:{HEIGHT}:force_original_aspect_ratio=decrease:flags=lanczos,\
         pad={WIDTH}:{HEIGHT}:(ow-iw)/2:(oh-ih)/2:black,format=rgb24"
    );
    let mut child = Command::new(&opts.ffmpeg)
        .args(["-v", "error", "-t", &VIDEO_MAX_SECONDS.to_string(), "-i"])
        .arg(path)
        .args(["-an", "-vf", &filter, "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't run ffmpeg ({}): {e}", opts.ffmpeg.display()))?;
    let mut out = child.stdout.take().unwrap();
    let size = (WIDTH * HEIGHT * 3) as usize;
    let duration = Duration::from_secs(1) / VIDEO_FPS;
    loop {
        let mut buf = vec![0u8; size];
        match out.read_exact(&mut buf) {
            Ok(()) => push(Raw::Fitted(RgbImage::from_raw(WIDTH, HEIGHT, buf).unwrap(), duration))?,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
    }
    let result = child.wait_with_output()?;
    if !result.status.success() {
        return Err(format!("ffmpeg: {}", String::from_utf8_lossy(&result.stderr).trim()).into());
    }
    Ok(())
}

/// Puts any image on the panel's 1600x720, over black.
fn fit(im: &RgbaImage) -> RgbImage {
    let (w, h) = im.dimensions();
    let placed = if (w, h) == (WIDTH, HEIGHT) {
        im.clone()
    } else if (w, h) == (WIDTH * 3 / 2, HEIGHT * 3 / 2) {
        // made for the glass's own 2400x1080: the panel will scale it back up
        imageops::resize(im, WIDTH, HEIGHT, FilterType::Triangle)
    } else {
        // smaller art: the biggest whole-number scale that fits, so pixels stay crisp;
        // bigger art: shrink to fit
        let s = (WIDTH / w).min(HEIGHT / h);
        let (nw, nh, filter) = if s >= 1 {
            (w * s, h * s, FilterType::Nearest)
        } else {
            let k = (WIDTH as f64 / w as f64).min(HEIGHT as f64 / h as f64);
            ((w as f64 * k).round() as u32, (h as f64 * k).round() as u32, FilterType::Lanczos3)
        };
        let mut canvas = RgbaImage::from_pixel(WIDTH, HEIGHT, image::Rgba([0, 0, 0, 255]));
        let scaled = imageops::resize(im, nw.max(1), nh.max(1), filter);
        imageops::overlay(&mut canvas, &scaled, ((WIDTH - nw) / 2).into(), ((HEIGHT - nh) / 2).into());
        canvas
    };
    // transparency shows black, which on OLED means pixels off
    let mut rgb = RgbImage::new(WIDTH, HEIGHT);
    for (out, px) in rgb.pixels_mut().zip(placed.pixels()) {
        let a = px[3] as u16;
        *out = image::Rgb([0, 1, 2].map(|c| (px[c] as u16 * a / 255) as u8));
    }
    rgb
}

fn encode(rgb: &RgbImage, quality: u8, sampling: SamplingFactor) -> Result<Vec<u8>, String> {
    let mut q = quality;
    loop {
        let mut jpeg = Vec::new();
        let mut encoder = Encoder::new(&mut jpeg, q);
        encoder.set_sampling_factor(sampling);
        encoder.encode(rgb.as_raw(), WIDTH as u16, HEIGHT as u16, ColorType::Rgb).map_err(|e| e.to_string())?;
        if jpeg.len() < MAX_JPEG || q <= 60 {
            return Ok(jpeg);
        }
        q -= 5;
    }
}
