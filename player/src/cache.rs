//! Prepared frames kept on disk, so a scene is only ever drawn and encoded once, even across restarts.
//!
//! One file per scene in %LOCALAPPDATA%\wvplay\cache, named after the scene's key (see scene::key: its
//! JSON, the files it uses and the settings it was prepared with). Format: `WVC2`, then the still kept
//! over live text (left, top, PNG length and PNG bytes; a length of 0 means none), the frame count, then
//! per frame its duration in microseconds, JPEG length and JPEG bytes, all u32 little-endian.

use std::io::Cursor;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use image::ImageFormat;

use crate::art::{Frame, Prepared};
use crate::scene::Buf;

const MAGIC: &[u8; 4] = b"WVC2";
const MAX_BYTES: u64 = 2 << 30;

fn dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map_or_else(std::env::temp_dir, PathBuf::from).join("wvplay").join("cache")
}

fn file_for(key: u64) -> PathBuf {
    dir().join(format!("{key:016x}.wvc"))
}

pub fn read(key: u64) -> Option<Prepared> {
    let data = std::fs::read(file_for(key)).ok()?;
    let u32_at = |at: usize| Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?) as usize);
    if data.get(..4)? != MAGIC {
        return None;
    }
    let (left, top, png_len) = (u32_at(4)?, u32_at(8)?, u32_at(12)?);
    let mut at = 16;
    let above = if png_len > 0 {
        let png = data.get(at..at + png_len)?;
        at += png_len;
        let image = image::load_from_memory_with_format(png, ImageFormat::Png).ok()?.into_rgba8();
        Some(Buf { left: left as u32, top: top as u32, image })
    } else {
        None
    };
    let count = u32_at(at)?;
    at += 4;
    let mut frames = Vec::with_capacity(count);
    for _ in 0..count {
        let (micros, len) = (u32_at(at)?, u32_at(at + 4)?);
        let jpeg = data.get(at + 8..at + 8 + len)?.to_vec();
        frames.push(Frame { jpeg, duration: Duration::from_micros(micros as u64) });
        at += 8 + len;
    }
    Some(Prepared { frames, above })
}

/// Keeps a scene's frames, then trims the cache if that took it over its cap.
pub fn write(key: u64, prepared: &Prepared) {
    let file = file_for(key);
    if file.exists() {
        return; // the same scene, prepared the same way, is already there
    }
    let mut png = Vec::new();
    let (left, top) = match &prepared.above {
        Some(above) if !above.is_empty() => {
            if above.image.write_to(&mut Cursor::new(&mut png), ImageFormat::Png).is_err() {
                return;
            }
            (above.left, above.top)
        }
        _ => (0, 0),
    };
    let frames = &prepared.frames;
    let mut data = Vec::with_capacity(20 + png.len() + frames.iter().map(|f| 8 + f.jpeg.len()).sum::<usize>());
    data.extend_from_slice(MAGIC);
    for n in [left, top, png.len() as u32] {
        data.extend_from_slice(&n.to_le_bytes());
    }
    data.extend_from_slice(&png);
    data.extend_from_slice(&(frames.len() as u32).to_le_bytes());
    for f in frames {
        data.extend_from_slice(&(f.duration.as_micros() as u32).to_le_bytes());
        data.extend_from_slice(&(f.jpeg.len() as u32).to_le_bytes());
        data.extend_from_slice(&f.jpeg);
    }
    let tmp = file.with_extension("tmp");
    let ok = std::fs::create_dir_all(dir()).is_ok() && std::fs::write(&tmp, &data).is_ok();
    if !ok || std::fs::rename(&tmp, &file).is_err() {
        let _ = std::fs::remove_file(&tmp); // a cache is optional: carry on without it
        return;
    }
    prune();
}

/// Keeps the cache under its size cap by deleting the files written longest ago.
pub fn prune() {
    let Ok(entries) = std::fs::read_dir(dir()) else { return };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .filter_map(|e| {
            let e = e.ok()?;
            let m = e.metadata().ok()?;
            Some((m.modified().ok()?, m.len(), e.path()))
        })
        .collect();
    files.sort();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    for (_, size, path) in files {
        if total <= MAX_BYTES {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= size;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::RgbaImage;

    /// A scene with a still above its live text comes back exactly, and keys tell files apart.
    #[test]
    fn round_trip() {
        let image = RgbaImage::from_fn(3, 2, |x, y| image::Rgba([x as u8, y as u8, 7, 200]));
        let prepared = Prepared {
            frames: vec![Frame { jpeg: vec![1, 2, 3], duration: Duration::from_millis(40) }, Frame { jpeg: vec![], duration: Duration::ZERO }],
            above: Some(Buf { left: 10, top: 20, image }),
        };
        let key = 0xfeed_0000_0000_0000u64 + std::process::id() as u64;
        let _ = std::fs::remove_file(file_for(key));
        write(key, &prepared);
        let back = read(key).expect("not written");
        std::fs::remove_file(file_for(key)).unwrap();
        assert_eq!(back.frames.len(), 2);
        assert_eq!((back.frames[0].jpeg.as_slice(), back.frames[0].duration), (&[1, 2, 3][..], Duration::from_millis(40)));
        assert_eq!(back.above, prepared.above);
        assert!(read(key + 1).is_none());
    }
}
