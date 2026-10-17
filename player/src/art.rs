//! Turns a scene into ready-to-send JPEG frames.
//!
//! Everything expensive (decoding, drawing the layers, rotating, JPEG encoding) happens once, in
//! scene.rs and here, and the result is kept on disk (see cache.rs). Playing is then just sending stored
//! bytes on a timer.

use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use image::imageops;
use image::{ImageFormat, RgbImage};
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};

use crate::cache;
use crate::playlist::{Kind, Layer, Rect, Scene, Text};
use crate::scene::{self, Buf};

/// What the panel takes. It shows this upscaled 1.5x on its 2400x1080 glass; a native 2400x1080
/// frame is accepted but its outer columns flicker.
pub const WIDTH: u32 = 1600;
pub const HEIGHT: u32 = 720;
/// TRCC drops frames of 450 KB or more, so stay under that.
const MAX_JPEG: usize = 400_000;
pub const VIDEO_TYPES: [&str; 6] = ["mp4", "m4v", "mov", "webm", "mkv", "avi"];

#[derive(Clone)]
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

/// A scene's cycle, ready to send, plus what's laid over it with its live text (empty without any).
#[derive(Clone)]
pub struct Prepared {
    pub frames: Vec<Frame>,
    pub over: Vec<Over>,
}

impl Prepared {
    /// What it holds in memory: its JPEGs and the stills of its over list.
    pub fn bytes(&self) -> usize {
        fn stills(over: &[Over]) -> usize {
            over.iter()
                .map(|node| match node {
                    Over::Still(buf) => buf.image.len(),
                    Over::Text { .. } => 0,
                    Over::Group { children, .. } => stills(children),
                })
                .sum()
        }
        self.frames.iter().map(|f| f.jpeg.len()).sum::<usize>() + stills(&self.over)
    }
}

/// What's laid over the prepared frames while playing (docs/widgets.md, Live text), drawn again when a
/// live text changes.
#[derive(Clone, Debug, PartialEq)]
pub enum Over {
    Still(Arc<Buf>),
    /// live_texts()[index], drawn at its own opacity × fold, inside its clip
    Text { index: usize, fold: f32 },
    /// a translucent layer, copy or widget layer holding live text: its children drawn into a buffer of
    /// `rect`, laid over at `opacity`
    Group { opacity: f32, rect: Rect, children: Vec<Over> },
}

/// The over list with `texts` (live_texts(), filled in) drawn in, as one buffer: what's laid over each
/// frame. A text whose index is past the end of `texts` is skipped.
pub fn bake(over: &[Over], texts: &[Text]) -> Buf {
    scene::bake(over, texts)
}

/// A scene's frames, from the disk cache if they're there. `save` keeps newly prepared ones there too; a
/// scene being edited isn't saved yet, as most versions are soon replaced (the caller can cache::write
/// the one that stays).
pub fn load(scene: &Scene, global: &[Layer], opts: &Options, save: bool) -> Result<Prepared, Box<dyn Error>> {
    let key = scene::key(scene, global, opts);
    if let Some(prepared) = cache::read(key) {
        return Ok(prepared);
    }
    let prepared = scene::prepare(scene, global, opts)?;
    if prepared.frames.is_empty() {
        return Err("no frames in it".into());
    }
    if save {
        cache::write(key, &prepared);
    }
    Ok(prepared)
}

/// Full-resolution colour keeps coloured pixel-art edges sharp; a scene with video doesn't need it. Only
/// the scene's own layers are looked at, for now.
pub fn sampling(scene: &Scene, global: &[Layer]) -> SamplingFactor {
    let _ = global;
    let video = scene.layers.iter().flat_map(|l| &l.objects).any(|o| matches!(&o.kind, Kind::Media(m) if crate::media::is_video(&m.src)));
    if video { SamplingFactor::R_4_2_0 } else { SamplingFactor::R_4_4_4 }
}

/// Lays the over list, baked with the live text (`texts`: live_texts(), filled in, docs/data.md), over a
/// scene's prepared frames: baked once, then each frame is decoded, has it laid over (where it belongs on
/// a turned frame) and is encoded again, on every core. A frame that won't decode or encode goes out as
/// it was. With nothing to lay over, the frames are as they are.
pub fn overlay(scene: &Scene, global: &[Layer], prepared: &Prepared, texts: &[Text], opts: &Options) -> Vec<Frame> {
    let baked = bake(&prepared.over, texts);
    if baked.is_empty() {
        return prepared.frames.clone();
    }
    let sampling = sampling(scene, global); // as prepare chose it
    let redraw = |frame: &Frame| {
        let jpeg = image::load_from_memory_with_format(&frame.jpeg, ImageFormat::Jpeg).ok().and_then(|im| {
            let mut rgb = im.into_rgb8();
            baked.onto_rgb(&mut rgb, 1.0, opts.rotate180);
            encode(&rgb, opts.quality, sampling).ok()
        });
        Frame { jpeg: jpeg.unwrap_or_else(|| frame.jpeg.clone()), duration: frame.duration }
    };
    let frames = &prepared.frames;
    let chunk = frames.len().div_ceil(workers(frames.len())).max(1);
    std::thread::scope(|s| {
        let redraw = &redraw;
        let workers: Vec<_> =
            frames.chunks(chunk).map(|part| (part, s.spawn(move || part.iter().map(redraw).collect::<Vec<_>>()))).collect();
        workers.into_iter().flat_map(|(part, w)| w.join().unwrap_or_else(|_| part.to_vec())).collect()
    })
}

/// How many threads `overlay` redraws `frames` frames on: one per core, but no more than the frames.
pub fn workers(frames: usize) -> usize {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    frames.div_ceil(frames.div_ceil(threads).max(1)).max(1)
}

/// Composes (with `make`), rotates and encodes a batch, on every core, keeping the order.
pub fn encode_all<T: Send>(batch: Vec<T>, make: impl Fn(T) -> (RgbImage, Duration) + Sync, opts: &Options, sampling: SamplingFactor)
    -> Result<Vec<Frame>, String> {
    if batch.is_empty() {
        return Ok(Vec::new());
    }
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let count = batch.len();
    let chunk = count.div_ceil(threads).max(1);
    let make = &make;
    let mut parts: Vec<Vec<T>> = Vec::with_capacity(threads);
    let mut batch = batch.into_iter();
    loop {
        let part: Vec<T> = batch.by_ref().take(chunk).collect();
        if part.is_empty() {
            break;
        }
        parts.push(part);
    }
    std::thread::scope(|s| {
        let workers: Vec<_> = parts
            .into_iter()
            .map(|part| {
                s.spawn(move || {
                    part.into_iter()
                        .map(|raw| {
                            let (mut rgb, duration) = make(raw);
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

pub fn encode(rgb: &RgbImage, quality: u8, sampling: SamplingFactor) -> Result<Vec<u8>, String> {
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
