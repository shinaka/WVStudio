//! Plays a playlist on the panel: each scene for its time, prepared ahead, reconnecting when needed.

use std::collections::HashMap;
use std::error::Error;
use std::fmt::Display;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{JoinHandle, sleep};
use std::time::{Duration, Instant, SystemTime};

use crate::panel::Panel;
use crate::playlist::{self, Playlist, Scene, Widget};
use crate::text::Layer;
use crate::{art, cache, data, scene};

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

/// One scene of the playlist, ready to send.
#[derive(Clone)]
struct Loaded {
    index: usize,
    frames: Arc<art::Prepared>,
    /// What it was prepared from (scene::key), so a changed version is told apart.
    key: u64,
    /// The frames with the live text on, when the scene has any.
    live: Option<Live>,
}

/// A scene's frames with its live text drawn over them (docs/data.md), kept until the text changes, so
/// an animation is encoded again once per change rather than every loop.
#[derive(Clone)]
struct Live {
    /// The live widgets with their text filled in, as drawn.
    widgets: Vec<Widget>,
    frames: Arc<Vec<art::Frame>>,
}

/// New live frames being drawn in the background, over `base`, while the old ones carry on.
struct Composing {
    base: Arc<art::Prepared>,
    widgets: Vec<Widget>,
    job: JoinHandle<Arc<Vec<art::Frame>>>,
}

/// Prepared frames of recently played scenes, so switching back to one costs nothing. Pixel art is small
/// (a 48-frame loop is about 2 MB); bigger scenes like videos come back from the disk cache instead.
/// Keyed by scene::key, so editing a scene prepares it again.
type Cache = Arc<Mutex<HashMap<u64, Arc<art::Prepared>>>>;
const CACHE_BYTES: usize = 128 << 20;

/// The playlist and its files, each with its stamp: changes whenever any of them does.
type Fingerprint = Vec<(PathBuf, Option<(SystemTime, u64)>)>;
/// How often to look for edits to the playlist and its files.
const CHECK_EVERY: Duration = Duration::from_millis(500);
/// An edit is read once nothing has changed for this long, so a burst of typing in the studio (which
/// saves as you type) is prepared once.
const SETTLE: Duration = Duration::from_millis(1500);
/// Edits closer together than this are one editing session. Meanwhile the next scene waits (edits would
/// only make it stale), and re-prepared scenes stay out of the disk cache: only the last version is
/// written, once the edits stop.
const QUIET: Duration = Duration::from_secs(10);

struct Player {
    target: PathBuf,
    opts: art::Options,
    seconds: f64,
    playlist: Playlist,
    /// What the playlist and its files were like when last read.
    fingerprint: Fingerprint,
    /// A different fingerprint seen since, and when: read once it has held still for `settle`.
    changing: Option<(Fingerprint, Instant)>,
    settle: Duration,
    current: Loaded,
    cache: Cache,
    /// The scene after this one, being prepared in the background while this one plays.
    next: Option<JoinHandle<Option<Loaded>>>,
    /// Whether `next` still has to be started: it waits while the playlist is being edited.
    want_next: bool,
    /// A new version of the scene at this index after an edit, being prepared in the background while
    /// the old one plays. A newer edit replaces it, and the stale result is dropped.
    reload: Option<(usize, JoinHandle<Option<Loaded>>)>,
    /// When an edit was last read.
    edited: Option<Instant>,
    quiet: Duration,
    /// The newest re-prepared version that isn't in the disk cache yet.
    unsaved: Option<Loaded>,
    composing: Option<Composing>,
}

impl Player {
    fn new(target: &Path, opts: art::Options, seconds: f64) -> Result<Player, Box<dyn Error>> {
        let playlist = playlist::read(target, seconds, &opts.ffmpeg)?;
        let cache = Cache::default();
        let current = load_from(&playlist, 0, &opts, &cache, true).ok_or("none of the scenes could be loaded")?;
        let mut player = Player { target: target.to_path_buf(), opts, seconds, fingerprint: Vec::new(),
                                  changing: None, settle: SETTLE, playlist, current, cache, next: None,
                                  want_next: false, reload: None, edited: None, quiet: QUIET, unsaved: None,
                                  composing: None };
        player.fingerprint = playlist::fingerprint(target, &player.playlist);
        player.prepare_next();
        Ok(player)
    }

    fn scene(&self) -> &Scene {
        &self.playlist.scenes[self.current.index]
    }

    /// The frames to send: with the live text on, when there is some.
    fn showing(&self) -> &[art::Frame] {
        self.current.live.as_ref().map_or(&self.current.frames.frames[..], |live| &live.frames[..])
    }

    /// Keeps the live text current: takes in finished redraws, and with `look`, starts one if the text
    /// (filled in from the values as they are now) has changed. Cheap, and free for scenes without live
    /// text. Returns whether the frames showing changed.
    fn update_live(&mut self, look: bool) -> bool {
        let mut changed = false;
        if self.composing.as_ref().is_some_and(|c| c.job.is_finished()) {
            let Composing { base, widgets, job } = self.composing.take().unwrap();
            // dropped if the scene has moved on meanwhile
            if let Ok(frames) = job.join() {
                if Arc::ptr_eq(&base, &self.current.frames) {
                    self.current.live = Some(Live { widgets, frames });
                    changed = true;
                }
            }
        }
        if !look {
            return changed;
        }
        let widgets = filled(&self.playlist, self.current.index);
        if widgets.is_empty() {
            self.composing = None;
            return self.current.live.take().is_some() || changed;
        }
        let base = &self.current.frames;
        let showing = self.current.live.as_ref().is_some_and(|l| l.widgets == widgets);
        let coming = self.composing.as_ref().is_some_and(|c| c.widgets == widgets && Arc::ptr_eq(&c.base, base));
        if !showing && !coming {
            let (scene, base, opts, drawn) = (self.scene().clone(), base.clone(), self.opts.clone(), widgets.clone());
            let job = std::thread::spawn(move || overlay(&scene, &base, &drawn, &opts));
            self.composing = Some(Composing { base: self.current.frames.clone(), widgets, job });
        }
        changed
    }

    fn editing(&self) -> bool {
        self.edited.is_some_and(|at| at.elapsed() < self.quiet)
    }

    fn prepare_next(&mut self) {
        if self.playlist.scenes.len() > 1 {
            let (playlist, from) = (self.playlist.clone(), self.current.index + 1);
            let (opts, cache) = (self.opts.clone(), self.cache.clone());
            self.next = Some(std::thread::spawn(move || load_from(&playlist, from, &opts, &cache, true)));
        }
    }

    /// Moves on to the next scene, if it has been prepared. Returns false while it's still being prepared
    /// (or waits for edits to stop): this one carries on meanwhile, rather than the screen freezing.
    fn advance(&mut self) -> bool {
        if !self.next.as_ref().is_some_and(|h| h.is_finished()) {
            return false;
        }
        if let Some(loaded) = self.next.take().and_then(|h| h.join().ok().flatten()) {
            self.current = loaded;
        }
        self.want_next = true;
        true
    }

    /// Picks up edits to the playlist or its files, once they've held still for `settle` (going by the
    /// times it's called with). The scene showing carries on meanwhile, and while a changed version of it
    /// is prepared. Returns whether the frames showing changed.
    fn refresh(&mut self, now: Instant) -> bool {
        let found = playlist::fingerprint(&self.target, &self.playlist);
        if found == self.fingerprint {
            self.changing = None;
            return false;
        }
        match &self.changing {
            Some((seen, since)) if *seen == found => {
                if now - *since < self.settle {
                    return false;
                }
            }
            _ => {
                self.changing = Some((found, now));
                return false;
            }
        }
        self.changing = None;
        self.edited = Some(now);
        let mut changed = false;
        match playlist::read(&self.target, self.seconds, &self.opts.ffmpeg) {
            Ok(playlist) => {
                // carry on with the same scene if it's still listed
                let id = self.scene().id.clone();
                let at = playlist.scenes.iter().position(|s| s.id == id).unwrap_or(0);
                self.playlist = playlist;
                // what comes next may have changed too: it's prepared again once the edits stop
                self.next = None;
                self.want_next = true;
                match cached(&self.cache, &self.playlist, at, &self.opts) {
                    // unchanged, or back to a version still in memory: nothing to wait for
                    Some(mut loaded) => {
                        self.reload = None;
                        changed = !Arc::ptr_eq(&loaded.frames, &self.current.frames);
                        if !changed {
                            loaded.live = self.current.live.take(); // only the live text changed, if anything
                        }
                        self.current = loaded;
                    }
                    None => {
                        self.current.index = at;
                        let (playlist, opts, cache) = (self.playlist.clone(), self.opts.clone(), self.cache.clone());
                        let reload = std::thread::spawn(move || load_from(&playlist, at, &opts, &cache, false));
                        self.reload = Some((at, reload));
                    }
                }
            }
            Err(e) => log(format_args!("couldn't read {}, carrying on: {e}", self.target.display())),
        }
        self.fingerprint = playlist::fingerprint(&self.target, &self.playlist);
        changed
    }

    /// Takes in background work that has finished, and starts what waits for edits to stop. Cheap, so
    /// it runs between frames. Returns whether the frames showing changed.
    fn tick(&mut self) -> bool {
        let mut changed = false;
        if self.reload.as_ref().is_some_and(|(_, h)| h.is_finished()) {
            let (index, reload) = self.reload.take().unwrap();
            if let Some(loaded) = reload.join().ok().flatten() {
                self.unsaved = Some(loaded.clone());
                // swapped in, unless the playlist has moved on to another scene meanwhile
                if self.current.index == index {
                    self.current = loaded;
                    changed = true;
                }
            }
        }
        if self.reload.is_none() && !self.editing() {
            if self.want_next {
                self.want_next = false;
                self.prepare_next();
            }
            if let Some(loaded) = self.unsaved.take() {
                // into the disk cache, if it's still the version in the playlist
                let listed = (0..self.playlist.scenes.len()).any(|i| key(&self.playlist, i, &self.opts) == loaded.key);
                if listed {
                    std::thread::spawn(move || cache::write(loaded.key, &loaded.frames));
                }
            }
        }
        changed
    }

    /// Sends frames on their own schedule until sending fails (the panel has gone away).
    fn stream(&mut self, send: &mut dyn FnMut(&[u8]) -> std::io::Result<()>) -> std::io::Result<()> {
        let mut i = 0;
        let mut due = Instant::now();
        let mut ends = due + Duration::from_secs_f64(self.scene().seconds);
        let mut next_check = due + CHECK_EVERY;
        loop {
            if Instant::now() >= due {
                let frames = self.showing();
                send(&frames[i].jpeg)?;
                due += if frames.len() == 1 { STILL_REFRESH } else { frames[i].duration };
                i = (i + 1) % frames.len();
                let now = Instant::now();
                if due + Duration::from_millis(250) < now {
                    due = now; // fell well behind (the PC slept, say): carry on from here, don't race to catch up
                }
            }
            let now = Instant::now();
            let mut restart = false;
            if now >= ends {
                if self.playlist.scenes.len() > 1 && self.advance() {
                    restart = true;
                } else {
                    // nothing else to play, or it isn't ready: look again shortly (and don't wake for a
                    // time that's gone, which would send frames as fast as the panel takes them)
                    ends = now + CHECK_EVERY;
                }
            }
            let look = now >= next_check;
            if look {
                next_check = now + CHECK_EVERY;
                restart |= self.refresh(now);
            }
            restart |= self.tick();
            if self.update_live(look) && !restart && self.showing().len() == 1 {
                due = Instant::now(); // a still shows its new text at once, an animation with its next frame
            }
            if restart {
                i = 0;
                due = Instant::now();
                ends = due + Duration::from_secs_f64(self.scene().seconds);
                continue;
            }
            // wake for the next frame, the scene's end, or the next look for edits
            sleep(due.min(ends).min(next_check).saturating_duration_since(Instant::now()));
        }
    }
}

fn key(playlist: &Playlist, index: usize, opts: &art::Options) -> u64 {
    scene::key(&playlist.scenes[index], &playlist.global, opts)
}

/// A scene, if it's in memory as it is now (its key covers its files' stamps).
fn cached(cache: &Cache, playlist: &Playlist, index: usize, opts: &art::Options) -> Option<Loaded> {
    let key = key(playlist, index, opts);
    let frames = cache.lock().unwrap().get(&key)?.clone();
    Some(Loaded { index, frames, key, live: None })
}

/// A scene's live widgets (its own, then the global layers'), with their text filled in from the values
/// as they are now.
fn filled(playlist: &Playlist, index: usize) -> Vec<Widget> {
    playlist.scenes[index]
        .layers
        .iter()
        .chain(&playlist.global)
        .flat_map(|l| l.live().collect::<Vec<_>>())
        .map(|w| Widget { text: data::resolve(&w.text), ..w })
        .collect()
}

/// The frames with `widgets` drawn over them (and the still above them back on top); the same frames
/// when that draws nothing.
fn overlay(scene: &Scene, prepared: &Arc<art::Prepared>, widgets: &[Widget], opts: &art::Options) -> Arc<Vec<art::Frame>> {
    let layer = Layer::render(widgets);
    if layer.is_empty() { Arc::new(prepared.frames.clone()) } else { Arc::new(art::overlay(scene, prepared, &layer, opts)) }
}

/// Loads the first scene that works, starting at `from` and wrapping around, with its live text drawn on
/// as the values are now. `save` keeps newly prepared scenes in the disk cache too.
fn load_from(playlist: &Playlist, from: usize, opts: &art::Options, cache: &Cache, save: bool) -> Option<Loaded> {
    let n = playlist.scenes.len();
    (0..n).map(|k| (from + k) % n).find_map(|index| {
        let mut loaded = cached(cache, playlist, index, opts).or_else(|| prepare(playlist, index, opts, cache, save))?;
        let widgets = filled(playlist, index);
        if !widgets.is_empty() {
            let frames = overlay(&playlist.scenes[index], &loaded.frames, &widgets, opts);
            loaded.live = Some(Live { widgets, frames });
        }
        Some(loaded)
    })
}

/// Prepares a scene and keeps it in the in-memory cache.
fn prepare(playlist: &Playlist, index: usize, opts: &art::Options, cache: &Cache, save: bool) -> Option<Loaded> {
    let scene = &playlist.scenes[index];
    let key = key(playlist, index, opts);
    let start = Instant::now();
    match art::load(scene, &playlist.global, opts, save) {
        Ok(prepared) => {
            let bytes = prepared.frames.iter().map(|f| f.jpeg.len()).sum::<usize>();
            log(format_args!("ready: {} ({} frame(s), {} KB, {} ms)", scene.name, prepared.frames.len(), bytes / 1024,
                             start.elapsed().as_millis()));
            let frames = Arc::new(prepared);
            let mut cache = cache.lock().unwrap();
            cache.insert(key, frames.clone());
            let size = |c: &HashMap<u64, Arc<art::Prepared>>| -> usize {
                c.values().flat_map(|p| p.frames.iter()).map(|f| f.jpeg.len()).sum()
            };
            while size(&cache) > CACHE_BYTES && cache.len() > 1 {
                let victim = cache.keys().find(|k| **k != key).copied().unwrap();
                cache.remove(&victim);
            }
            Some(Loaded { index, frames, key, live: None })
        }
        Err(e) => {
            log(format_args!("skipping {}: {e}", scene.name));
            None
        }
    }
}

pub fn play(target: &Path, opts: art::Options, seconds: f64) -> Result<(), Box<dyn Error>> {
    cache::prune();
    // live values for text, and the API the studio uses, from the data.json beside a playlist
    if target.extension().is_some_and(|e| e.eq_ignore_ascii_case("json")) {
        data::start(target, true);
    }
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
                if let Err(e) = player.stream(&mut |jpeg| panel.send_jpeg(jpeg, art::WIDTH, art::HEIGHT)) {
                    log(format_args!("lost the panel: {e}"));
                }
            }
            Err(e) => log(format_args!("waiting for the panel: {e}")),
        }
        sleep(Duration::from_secs(3));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playlist::Kind;
    use crate::text;
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame, Rgba, RgbaImage};

    /// A playlist of one small animation (3 frames of 100 ms), in a folder of its own.
    struct Scratch {
        dir: PathBuf,
        list: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("wvplay-test-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let mut gif = GifEncoder::new(std::fs::File::create(dir.join("a.gif")).unwrap());
            for shade in [40, 80, 120] {
                let im = RgbaImage::from_pixel(160, 72, Rgba([0, 0, shade, 255]));
                gif.encode_frame(Frame::from_parts(im, 0, 0, Delay::from_numer_denom_ms(100, 1))).unwrap();
            }
            drop(gif);
            let scratch = Scratch { list: dir.join("playlist.json"), dir };
            scratch.write("");
            scratch
        }

        /// The playlist (version 1, converted as it's read), with a clock showing `text` in the global
        /// layer (no text for "").
        fn json(text: &str) -> String {
            let clock = format!(r#"{{ "id": "clock", "type": "text", "text": "{text}", "size": 100 }}"#);
            let overlay = if text.is_empty() { "" } else { &clock };
            format!(r#"{{ "seconds": 1, "overlay": [{overlay}], "items": [ {{ "file": "a.gif", "widgets": [] }} ] }}"#)
        }

        fn write(&self, text: &str) {
            std::fs::write(&self.list, Scratch::json(text)).unwrap();
        }

        fn opts() -> art::Options {
            art::Options { rotate180: true, quality: 90, ffmpeg: PathBuf::from("ffmpeg") }
        }

        fn player(&self) -> Player {
            Player::new(&self.list, Scratch::opts(), 30.0).unwrap()
        }

        /// The key the scene would have with `text` in the clock.
        fn key_with(&self, text: &str) -> u64 {
            let p = playlist::parse(Scratch::json(text).as_bytes(), &self.dir, 30.0, Path::new("ffmpeg")).unwrap();
            key(&p, 0, &Scratch::opts())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Whether `f` comes true within 10 s.
    fn eventually(mut f: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if f() {
                return true;
            }
            sleep(Duration::from_millis(10));
        }
        false
    }

    /// Refreshes until the playlist as it is now has been read. Returns whether the frames changed.
    fn read(p: &mut Player) -> bool {
        let mut changed = false;
        assert!(eventually(|| {
            changed |= p.refresh(Instant::now());
            p.changing.is_none() && playlist::fingerprint(&p.target, &p.playlist) == p.fingerprint
        }));
        changed
    }

    fn clock(p: &Player) -> String {
        match p.playlist.global.first().and_then(|l| l.objects.first()).map(|o| &o.kind) {
            Some(Kind::Text(w)) => w.text.clone(),
            _ => String::new(),
        }
    }

    /// A burst of edits is read once, after it has held still for `settle`.
    #[test]
    fn edits_wait_to_settle() {
        if !Path::new(text::DEFAULT_FONT).exists() {
            return;
        }
        let s = Scratch::new("settle");
        let mut p = s.player();
        p.settle = Duration::from_millis(500);
        assert!(!p.refresh(Instant::now()));
        let mut last = Instant::now();
        for text in ["1", "12", "12:", "12:3"] {
            s.write(text);
            last = Instant::now();
            assert!(!p.refresh(Instant::now()));
            sleep(Duration::from_millis(100));
            assert!(!p.refresh(Instant::now()));
            assert_eq!(clock(&p), "", "read before the edits settled");
        }
        assert!(eventually(|| {
            p.refresh(Instant::now());
            !clock(&p).is_empty()
        }));
        assert!(last.elapsed() >= p.settle);
        assert_eq!(clock(&p), "12:3"); // one read, of the last version
        assert!(p.reload.is_some());
    }

    /// The new version is prepared in the background while the old one carries on, the newest edit wins,
    /// and only the version that stays goes into the disk cache, once the edits stop.
    #[test]
    fn edits_prepare_in_the_background() {
        if !Path::new(text::DEFAULT_FONT).exists() {
            return;
        }
        let s = Scratch::new("background");
        let mut p = s.player();
        p.settle = Duration::from_millis(50);
        let plain = p.current.frames.clone();

        s.write("A");
        assert!(!read(&mut p));
        assert!(p.reload.is_some());
        assert!(Arc::ptr_eq(&plain, &p.current.frames)); // the old version carries on meanwhile
        let first = p.reload.as_ref().unwrap().1.thread().id();

        s.write("BB");
        assert!(!read(&mut p));
        assert_ne!(p.reload.as_ref().unwrap().1.thread().id(), first); // replaced: A won't be swapped in
        assert!(eventually(|| p.tick())); // swapped in between frames once it's ready
        assert!(p.reload.is_none());
        let bb = cached(&p.cache, &p.playlist, 0, &p.opts).unwrap();
        assert!(Arc::ptr_eq(&bb.frames, &p.current.frames));
        assert!(!Arc::ptr_eq(&plain, &p.current.frames));

        // not in the disk cache while it's being edited...
        assert!(p.unsaved.is_some());
        assert!(!p.tick());
        assert!(cache::read(p.current.key).is_none());
        // ...then written, once the edits stop
        p.quiet = Duration::ZERO;
        p.tick();
        assert!(p.unsaved.is_none());
        assert!(eventually(|| cache::read(p.current.key).is_some()));
        // and A, the version replaced, never is (sleep through the time it would take to prepare)
        sleep(Duration::from_millis(500));
        assert!(cache::read(s.key_with("A")).is_none());

        // back to no text: still in memory, so it's swapped in at once
        p.quiet = QUIET;
        s.write("");
        assert!(read(&mut p));
        assert!(p.reload.is_none());
        assert!(Arc::ptr_eq(&plain, &p.current.frames));
    }

    /// Live text goes over the prepared frames, which stay exactly as they are without it, and is drawn
    /// and encoded again only when its text changes.
    #[test]
    fn live_text_redraws_on_change() {
        if !Path::new(text::DEFAULT_FONT).exists() {
            return;
        }
        let s = Scratch::new("live");
        let plain: Vec<Vec<u8>> = s.player().current.frames.frames.iter().map(|f| f.jpeg.clone()).collect();
        let set = |n: f64| data::store().put("wvtest", Duration::from_secs(180), vec![("wvtest.n".into(), data::Value::Number(n))]);
        set(1.0);
        s.write("n={wvtest.n}");
        let mut p = s.player();
        let base: Vec<Vec<u8>> = p.current.frames.frames.iter().map(|f| f.jpeg.clone()).collect();
        assert_eq!(base, plain);
        let first = p.current.live.clone().expect("no live frames");
        assert_eq!(first.widgets[0].text, "n=1");
        assert_eq!(first.frames.len(), base.len());
        assert!(first.frames.iter().zip(&base).all(|(live, base)| live.jpeg != *base));
        assert!(std::ptr::eq(p.showing().as_ptr(), first.frames.as_ptr()));

        // the same text: nothing to draw, however often it looks
        for _ in 0..5 {
            assert!(!p.update_live(true));
            assert!(p.composing.is_none());
        }
        // a new value: drawn once, in the background, with the old frames showing meanwhile
        set(2.0);
        assert!(!p.update_live(true));
        assert!(p.composing.is_some());
        assert!(!p.update_live(true)); // already on its way
        assert!(std::ptr::eq(p.showing().as_ptr(), first.frames.as_ptr()));
        assert!(eventually(|| p.update_live(false)));
        let second = p.current.live.clone().unwrap();
        assert_eq!(second.widgets[0].text, "n=2");
        assert!(!Arc::ptr_eq(&second.frames, &first.frames));
        assert!(!p.update_live(true));
        assert!(p.composing.is_none());

        // the live widget gone: back to the prepared frames as they are
        s.write("");
        read(&mut p);
        p.update_live(true);
        assert!(p.current.live.is_none());
        assert!(std::ptr::eq(p.showing().as_ptr(), p.current.frames.frames.as_ptr()));
    }

    /// While frames stream on their schedule, an edit shows without a gap, and a playlist of one scene
    /// keeps its frame rate after its time is up.
    #[test]
    fn edits_show_while_streaming() {
        if !Path::new(text::DEFAULT_FONT).exists() {
            return;
        }
        let s = Scratch::new("stream");
        let mut p = s.player();
        p.settle = Duration::from_millis(300);
        let old: Vec<Vec<u8>> = p.current.frames.frames.iter().map(|f| f.jpeg.clone()).collect();
        let list = s.list.clone();
        let editor = std::thread::spawn(move || {
            sleep(Duration::from_millis(1200)); // after the scene's 1 s is up
            std::fs::write(&list, Scratch::json("12:34")).unwrap();
            Instant::now()
        });
        let start = Instant::now();
        let mut sent: Vec<(Instant, bool)> = Vec::new(); // when, and whether it was the old version
        let end = p.stream(&mut |jpeg| {
            if start.elapsed() > Duration::from_secs(4) {
                return Err(std::io::Error::other("done"));
            }
            sent.push((Instant::now(), old.iter().any(|o| o == jpeg)));
            Ok(())
        });
        assert_eq!(end.unwrap_err().to_string(), "done");
        let edited = editor.join().unwrap();
        // ten frames a second all along (not as fast as the panel takes them once the 1 s is up)
        assert!((35..=45).contains(&sent.len()), "{} frames sent in 4 s", sent.len());
        let gaps = sent.windows(2).map(|w| w[1].0 - w[0].0);
        assert!(gaps.max().unwrap() < Duration::from_millis(250));
        // the old version until the new one is ready, then only the new one, within settle + a look or two
        let switch = sent.iter().position(|(_, old)| !old).expect("the edit never showed");
        assert!(sent[switch..].iter().all(|(_, old)| !old));
        let took = sent[switch].0 - edited;
        assert!(took >= p.settle && took < p.settle + 3 * CHECK_EVERY, "the edit took {took:?} to show");
    }
}
