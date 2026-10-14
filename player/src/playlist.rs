//! What to play: one file, every image in a folder, or a playlist file.
//!
//! A playlist is JSON, with files relative to the playlist's own folder:
//!
//! ```json
//! { "seconds": 30, "items": [ { "file": "out/cat.png", "seconds": 20 }, { "file": "out/rain.gif" } ] }
//! ```

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

const IMAGE_TYPES: [&str; 7] = ["png", "gif", "webp", "jpg", "jpeg", "bmp", "apng"];

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub path: PathBuf,
    pub seconds: f64,
}

#[derive(Deserialize)]
struct File {
    seconds: Option<f64>,
    items: Vec<FileItem>,
}

#[derive(Deserialize)]
struct FileItem {
    file: PathBuf,
    seconds: Option<f64>,
}

pub fn read(target: &Path, default_seconds: f64) -> Result<Vec<Item>, Box<dyn Error>> {
    let items = if target.is_dir() {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(target)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file() && is_image(p))
            .collect();
        paths.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
        paths.into_iter().map(|path| Item { path, seconds: default_seconds }).collect()
    } else if target.extension().is_some_and(|e| e.eq_ignore_ascii_case("json")) {
        let file: File = serde_json::from_slice(&std::fs::read(target)?)?;
        let base = target.parent().unwrap_or(Path::new("."));
        let fallback = file.seconds.unwrap_or(default_seconds);
        file.items
            .into_iter()
            .map(|i| Item { path: base.join(i.file), seconds: i.seconds.unwrap_or(fallback).max(1.0) })
            .collect()
    } else {
        vec![Item { path: target.to_path_buf(), seconds: default_seconds }]
    };
    if items.is_empty() {
        return Err(format!("nothing to play in {}", target.display()).into());
    }
    Ok(items)
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| IMAGE_TYPES.iter().chain(&crate::art::VIDEO_TYPES).any(|t| e.eq_ignore_ascii_case(t)))
}

/// Changes whenever the playlist, the folder's contents or any listed file changes.
pub fn fingerprint(target: &Path, items: &[Item]) -> Vec<(PathBuf, Option<(SystemTime, u64)>)> {
    let mut paths = vec![target.to_path_buf()];
    if target.is_dir() {
        if let Ok(entries) = std::fs::read_dir(target) {
            paths.extend(entries.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| is_image(p)));
        }
    }
    paths.extend(items.iter().map(|i| i.path.clone()));
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .map(|p| {
            let s = stamp(&p);
            (p, s)
        })
        .collect()
}

/// A file's modified time and size, to notice when it's been replaced.
pub fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    std::fs::metadata(path).ok().and_then(|m| Some((m.modified().ok()?, m.len())))
}
