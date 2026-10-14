//! Plays a playlist on the panel: each piece for its time, prepared ahead, reconnecting when needed.

use std::collections::HashMap;
use std::error::Error;
use std::fmt::Display;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{sleep, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use crate::panel::Panel;
use crate::playlist::{self, Item};
use crate::{art, cache};

/// A still picture is re-sent this often, so the screen comes back by itself after the panel resets.
const STILL_REFRESH: Duration = Duration::from_secs(2);

static LOG: OnceLock<Mutex<File>> = OnceLock::new();

/// Sends the log to a file (started afresh) instead of the console.
pub fn log_to(path: &Path) -> std::io::Result<()> {
    let _ = LOG.set(Mutex::new(File::create(path)?));
    Ok(())
}

pub fn log(msg: impl Display) {
    let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let line = format!("[{:02}:{:02}:{:02} UTC] {msg}", secs / 3600 % 24, secs / 60 % 60, secs % 60);
    match LOG.get() {
        Some(file) => {
            let _ = writeln!(file.lock().unwrap(), "{line}");
        }
        None => eprintln!("{line}"),
    }
}

/// One piece of the playlist, ready to send.
struct Loaded {
    index: usize,
    frames: Arc<Vec<art::Frame>>,
}

/// Prepared frames of recently played pieces, so switching back to one costs nothing. Pixel art is small
/// (a 48-frame loop is about 2 MB); bigger pieces like videos come back from the disk cache instead.
type Cache = Arc<Mutex<HashMap<PathBuf, (Option<(SystemTime, u64)>, Arc<Vec<art::Frame>>)>>>;
const CACHE_BYTES: usize = 128 << 20;

struct Player {
    target: PathBuf,
    opts: art::Options,
    seconds: f64,
    items: Vec<Item>,
    fingerprint: Vec<(PathBuf, Option<(SystemTime, u64)>)>,
    current: Loaded,
    cache: Cache,
    /// The piece after this one, being prepared in the background while this one plays.
    next: Option<JoinHandle<Option<Loaded>>>,
}

impl Player {
    fn new(target: &Path, opts: art::Options, seconds: f64) -> Result<Player, Box<dyn Error>> {
        let items = playlist::read(target, seconds)?;
        let cache = Cache::default();
        let current = load_from(&items, 0, &opts, &cache).ok_or("none of the pieces could be loaded")?;
        let mut player = Player { target: target.to_path_buf(), opts, seconds, fingerprint: Vec::new(),
                                  items, current, cache, next: None };
        player.fingerprint = playlist::fingerprint(target, &player.items);
        player.prepare_next();
        Ok(player)
    }

    fn item(&self) -> &Item {
        &self.items[self.current.index]
    }

    fn prepare_next(&mut self) {
        if self.items.len() > 1 {
            let (items, from) = (self.items.clone(), self.current.index + 1);
            let (opts, cache) = (self.opts.clone(), self.cache.clone());
            self.next = Some(std::thread::spawn(move || load_from(&items, from, &opts, &cache)));
        }
    }

    fn advance(&mut self) {
        if let Some(loaded) = self.next.take().and_then(|h| h.join().ok().flatten()) {
            self.current = loaded;
        }
        self.prepare_next();
    }

    /// Picks up edits to the playlist or its files. Returns whether anything changed.
    fn refresh(&mut self) -> bool {
        let now = playlist::fingerprint(&self.target, &self.items);
        if now == self.fingerprint {
            return false;
        }
        match playlist::read(&self.target, self.seconds) {
            Ok(items) => {
                // carry on with the same piece if it's still listed
                let at = items.iter().position(|i| i.path == self.item().path).unwrap_or(0);
                self.items = items;
                self.next = None;
                match load_from(&self.items, at, &self.opts, &self.cache) {
                    Some(loaded) => self.current = loaded,
                    // nothing new loads: keep showing what we have, with an index that's still valid
                    None => self.current.index = at,
                }
                self.prepare_next();
            }
            Err(e) => log(format_args!("couldn't read {}, carrying on: {e}", self.target.display())),
        }
        self.fingerprint = playlist::fingerprint(&self.target, &self.items);
        true
    }

    /// Sends frames on their own schedule until the panel goes away.
    fn stream(&mut self, panel: &mut Panel) -> std::io::Result<()> {
        let mut i = 0;
        let mut due = Instant::now();
        let mut ends = due + Duration::from_secs_f64(self.item().seconds);
        let mut next_check = due + Duration::from_secs(1);
        loop {
            let frames = &self.current.frames;
            panel.send_jpeg(&frames[i].jpeg, art::WIDTH, art::HEIGHT)?;
            due += if frames.len() == 1 { STILL_REFRESH } else { frames[i].duration };
            i = (i + 1) % frames.len();
            let now = Instant::now();
            if due + Duration::from_millis(250) < now {
                due = now; // fell well behind (the PC slept, say): carry on from here, don't race to catch up
            }
            let mut restart = false;
            if self.items.len() > 1 && now >= ends {
                self.advance();
                restart = true;
            }
            if now >= next_check {
                next_check = now + Duration::from_secs(1);
                restart |= self.refresh();
            }
            if restart {
                i = 0;
                due = Instant::now();
                ends = due + Duration::from_secs_f64(self.item().seconds);
                continue;
            }
            sleep(due.min(ends).saturating_duration_since(Instant::now()));
        }
    }
}

/// Loads the first piece that works, starting at `from` and wrapping around.
fn load_from(items: &[Item], from: usize, opts: &art::Options, cache: &Cache) -> Option<Loaded> {
    (0..items.len()).map(|k| (from + k) % items.len()).find_map(|index| {
        let item = &items[index];
        let stamp = playlist::stamp(&item.path);
        if let Some((cached, frames)) = cache.lock().unwrap().get(&item.path) {
            if *cached == stamp {
                return Some(Loaded { index, frames: frames.clone() });
            }
        }
        let start = Instant::now();
        match art::load(&item.path, opts) {
            Ok(frames) => {
                let bytes = frames.iter().map(|f| f.jpeg.len()).sum::<usize>();
                log(format_args!("ready: {} ({} frame(s), {} KB, {} ms)", item.path.display(), frames.len(),
                                 bytes / 1024, start.elapsed().as_millis()));
                let frames = Arc::new(frames);
                let mut cache = cache.lock().unwrap();
                cache.insert(item.path.clone(), (stamp, frames.clone()));
                let size = |c: &HashMap<_, (_, Arc<Vec<art::Frame>>)>| -> usize {
                    c.values().flat_map(|(_, f)| f.iter()).map(|f| f.jpeg.len()).sum()
                };
                while size(&cache) > CACHE_BYTES && cache.len() > 1 {
                    let victim = cache.keys().find(|p| **p != item.path).cloned().unwrap();
                    cache.remove(&victim);
                }
                Some(Loaded { index, frames })
            }
            Err(e) => {
                log(format_args!("skipping {}: {e}", item.path.display()));
                None
            }
        }
    })
}

pub fn play(target: &Path, opts: art::Options, seconds: f64) -> Result<(), Box<dyn Error>> {
    cache::prune();
    // Started with Windows, the playlist (or the drive it's on) may not be there yet: wait for it.
    let mut player = loop {
        match Player::new(target, opts.clone(), seconds) {
            Ok(player) => break player,
            Err(e) => log(format_args!("nothing to play yet: {e}")),
        }
        sleep(Duration::from_secs(5));
    };
    loop {
        match Panel::open() {
            Ok(mut panel) => {
                log(format_args!("connected to {} (SUB 0x{:02X})", panel.info.identifier, panel.info.sub));
                if let Err(e) = player.stream(&mut panel) {
                    log(format_args!("lost the panel: {e}"));
                }
            }
            Err(e) => log(format_args!("waiting for the panel: {e}")),
        }
        sleep(Duration::from_secs(3));
    }
}
