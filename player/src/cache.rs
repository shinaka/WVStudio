//! Prepared frames kept on disk, so a scene is only ever drawn and encoded once, even across restarts.
//!
//! One file per scene in %LOCALAPPDATA%\wvplay\cache, named after the scene's key (see scene::key: its
//! JSON, the files it uses and the settings it was prepared with). Format, all numbers little-endian:
//! `WVC3`, then the over list laid over the frames with the live text (art::Over): a node count (u32),
//! then each node as a tag byte and its fields (0, a still: left and top as i32, PNG length as u32, PNG
//! bytes; 1, a text: index as u32, fold as f32; 2, a group: opacity as f32, rect as 4 i32, then its
//! children as a node count and nodes). Then the frame count, then per frame its duration in
//! microseconds, JPEG length and JPEG bytes, all u32. Files of another format (`WVC2`) aren't read.

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use image::ImageFormat;

use crate::art::{Frame, Over, Prepared};
use crate::scene::Buf;

const MAGIC: &[u8; 4] = b"WVC3";
const MAX_BYTES: u64 = 2 << 30;
/// Groups nest no deeper than this in a file that's read: copies are at most 8 deep, each with a layer.
const MAX_NESTING: usize = 64;

fn dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map_or_else(std::env::temp_dir, PathBuf::from).join("wvplay").join("cache")
}

fn file_for(key: u64) -> PathBuf {
    dir().join(format!("{key:016x}.wvc"))
}

/// Reads a file's numbers in order, never past its end.
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let bytes = self.data.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(bytes)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    /// A node count and the nodes, `depth` groups down.
    fn over(&mut self, depth: usize) -> Option<Vec<Over>> {
        if depth > MAX_NESTING {
            return None;
        }
        let count = self.u32()? as usize;
        let mut nodes = Vec::with_capacity(count.min(self.data.len() - self.at));
        for _ in 0..count {
            nodes.push(match self.u8()? {
                0 => {
                    let (left, top) = (u32::try_from(self.i32()?).ok()?, u32::try_from(self.i32()?).ok()?);
                    let len = self.u32()? as usize;
                    let image = image::load_from_memory_with_format(self.take(len)?, ImageFormat::Png).ok()?.into_rgba8();
                    Over::Still(Arc::new(Buf { left, top, image }))
                }
                1 => Over::Text { index: self.u32()? as usize, fold: self.f32()? },
                2 => {
                    let opacity = self.f32()?;
                    let rect = [self.i32()? as i64, self.i32()? as i64, self.i32()? as i64, self.i32()? as i64];
                    Over::Group { opacity, rect, children: self.over(depth + 1)? }
                }
                _ => return None,
            });
        }
        Some(nodes)
    }
}

pub fn read(key: u64) -> Option<Prepared> {
    let data = std::fs::read(file_for(key)).ok()?;
    let mut r = Reader { data: &data, at: 0 };
    if r.take(4)? != MAGIC {
        return None;
    }
    let over = r.over(0)?;
    let count = r.u32()? as usize;
    let mut frames = Vec::with_capacity(count.min(data.len()));
    for _ in 0..count {
        let (micros, len) = (r.u32()?, r.u32()? as usize);
        frames.push(Frame { jpeg: r.take(len)?.to_vec(), duration: Duration::from_micros(micros as u64) });
    }
    Some(Prepared { frames, over })
}

/// The over list as the file holds it, or None if a still won't encode.
fn write_over(over: &[Over], data: &mut Vec<u8>) -> Option<()> {
    data.extend_from_slice(&(over.len() as u32).to_le_bytes());
    for node in over {
        match node {
            Over::Still(buf) => {
                let mut png = Vec::new();
                buf.image.write_to(&mut Cursor::new(&mut png), ImageFormat::Png).ok()?;
                data.push(0);
                for n in [buf.left as i32, buf.top as i32] {
                    data.extend_from_slice(&n.to_le_bytes());
                }
                data.extend_from_slice(&(png.len() as u32).to_le_bytes());
                data.extend_from_slice(&png);
            }
            Over::Text { index, fold } => {
                data.push(1);
                data.extend_from_slice(&(*index as u32).to_le_bytes());
                data.extend_from_slice(&fold.to_le_bytes());
            }
            Over::Group { opacity, rect, children } => {
                data.push(2);
                data.extend_from_slice(&opacity.to_le_bytes());
                for n in rect {
                    data.extend_from_slice(&(*n as i32).to_le_bytes());
                }
                write_over(children, data)?;
            }
        }
    }
    Some(())
}

/// Keeps a scene's frames, then trims the cache if that took it over its cap.
pub fn write(key: u64, prepared: &Prepared) {
    let file = file_for(key);
    if file.exists() {
        return; // the same scene, prepared the same way, is already there
    }
    let frames = &prepared.frames;
    let mut data = Vec::with_capacity(12 + frames.iter().map(|f| 8 + f.jpeg.len()).sum::<usize>());
    data.extend_from_slice(MAGIC);
    if write_over(&prepared.over, &mut data).is_none() {
        return;
    }
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

    fn still(left: u32, top: u32, w: u32, h: u32) -> Over {
        let image = RgbaImage::from_fn(w, h, |x, y| image::Rgba([x as u8, y as u8, left as u8, 200]));
        Over::Still(Arc::new(Buf { left, top, image }))
    }

    /// A scene's over list, with every kind of node and a group in a group, comes back exactly with its
    /// frames, and keys tell files apart.
    #[test]
    fn round_trip() {
        let over = vec![
            still(10, 20, 3, 2),
            Over::Text { index: 0, fold: 1.0 },
            Over::Group {
                opacity: 0.5,
                rect: [100, 50, 180, 110],
                children: vec![
                    still(100, 50, 4, 4),
                    Over::Group { opacity: 0.8, rect: [106, 56, 166, 86], children: vec![Over::Text { index: 1, fold: 1.0 }] },
                    Over::Text { index: 2, fold: 0.25 },
                ],
            },
            still(0, 0, 1, 1),
        ];
        let prepared = Prepared {
            frames: vec![Frame { jpeg: vec![1, 2, 3], duration: Duration::from_millis(40) }, Frame { jpeg: vec![], duration: Duration::ZERO }],
            over,
        };
        let key = 0xfeed_0000_0000_0000u64 + std::process::id() as u64;
        let _ = std::fs::remove_file(file_for(key));
        write(key, &prepared);
        let back = read(key).expect("not written");
        assert_eq!(&std::fs::read(file_for(key)).unwrap()[..4], b"WVC3");
        std::fs::remove_file(file_for(key)).unwrap();
        assert_eq!(back.frames.len(), 2);
        assert_eq!((back.frames[0].jpeg.as_slice(), back.frames[0].duration), (&[1, 2, 3][..], Duration::from_millis(40)));
        assert_eq!((back.frames[1].jpeg.len(), back.frames[1].duration), (0, Duration::ZERO));
        assert_eq!(back.over, prepared.over);
        assert!(read(key + 1).is_none());
        // without live text, nothing is laid over
        let plain = Prepared { frames: prepared.frames.clone(), over: Vec::new() };
        write(key + 2, &plain);
        assert!(read(key + 2).expect("not written").over.is_empty());
        std::fs::remove_file(file_for(key + 2)).unwrap();
    }

    /// A file in the format before the over list (`WVC2`: the still above, then the frames) isn't read,
    /// and nor is one cut short or with a node of no known kind.
    #[test]
    fn older_and_broken_files_read_as_none() {
        let key = 0xfeed_1000_0000_0000u64 + std::process::id() as u64;
        std::fs::create_dir_all(dir()).unwrap();
        let mut wvc2 = b"WVC2".to_vec();
        for n in [0u32, 0, 0, 1, 40_000, 3] {
            wvc2.extend_from_slice(&n.to_le_bytes());
        }
        wvc2.extend_from_slice(&[1, 2, 3]);
        std::fs::write(file_for(key), &wvc2).unwrap();
        assert!(read(key).is_none());
        // the same frames written now, then cut short or given a node of tag 9
        let _ = std::fs::remove_file(file_for(key));
        write(key, &Prepared { frames: vec![Frame { jpeg: vec![1, 2, 3], duration: Duration::from_millis(40) }], over: vec![still(1, 2, 2, 2)] });
        let good = std::fs::read(file_for(key)).unwrap();
        assert!(read(key).is_some());
        std::fs::write(file_for(key), &good[..good.len() - 1]).unwrap();
        assert!(read(key).is_none());
        let mut odd = good.clone();
        odd[8] = 9; // the first node's tag
        std::fs::write(file_for(key), &odd).unwrap();
        assert!(read(key).is_none());
        std::fs::remove_file(file_for(key)).unwrap();
    }
}
