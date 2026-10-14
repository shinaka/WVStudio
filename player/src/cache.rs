//! Prepared frames kept on disk, so a piece is only ever decoded and encoded once, even across restarts.
//!
//! One file per piece in %LOCALAPPDATA%\wvplay\cache, named after the piece's path, size and modified
//! time plus the settings it was prepared with. Format: `WVC1`, frame count, then per frame its duration
//! in microseconds, JPEG length and JPEG bytes, all u32 little-endian.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::art::{Frame, Options};

const MAGIC: &[u8; 4] = b"WVC1";
const MAX_BYTES: u64 = 2 << 30;

fn dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map_or_else(std::env::temp_dir, PathBuf::from).join("wvplay").join("cache")
}

fn file_for(path: &Path, opts: &Options) -> Option<PathBuf> {
    let meta = std::fs::metadata(path).ok()?;
    let mut h = DefaultHasher::new(); // fixed keys, so the same piece hashes the same every run
    (std::fs::canonicalize(path).ok()?, meta.len(), meta.modified().ok()?, opts.rotate180, opts.quality).hash(&mut h);
    Some(dir().join(format!("{:016x}.wvc", h.finish())))
}

pub fn read(path: &Path, opts: &Options) -> Option<Vec<Frame>> {
    let data = std::fs::read(file_for(path, opts)?).ok()?;
    let u32_at = |at: usize| Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?) as usize);
    if data.get(..4)? != MAGIC {
        return None;
    }
    let mut frames = Vec::with_capacity(u32_at(4)?);
    let mut at = 8;
    for _ in 0..frames.capacity() {
        let (micros, len) = (u32_at(at)?, u32_at(at + 4)?);
        let jpeg = data.get(at + 8..at + 8 + len)?.to_vec();
        frames.push(Frame { jpeg, duration: Duration::from_micros(micros as u64) });
        at += 8 + len;
    }
    Some(frames)
}

pub fn write(path: &Path, opts: &Options, frames: &[Frame]) {
    let Some(file) = file_for(path, opts) else { return };
    let mut data = Vec::with_capacity(8 + frames.iter().map(|f| 8 + f.jpeg.len()).sum::<usize>());
    data.extend_from_slice(MAGIC);
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
    }
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
