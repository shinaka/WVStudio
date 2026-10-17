//! Plays a playlist on the panel: each scene for its time, prepared ahead, reconnecting when needed.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt::Display;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{JoinHandle, sleep};
use std::time::{Duration, Instant, SystemTime};

use crate::panel::Panel;
use crate::playlist::{self, Playlist, Scene, Text};
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
    /// The read it was prepared for, and the scene's place in it: its live text is numbered as its over
    /// list is only within the same read.
    playlist: Arc<Playlist>,
    index: usize,
    frames: Arc<art::Prepared>,
    /// What it was prepared from (scene::key), so a changed version is told apart.
    key: u64,
    /// The frames with the live text on, when the scene has any.
    live: Option<Live>,
}

impl Loaded {
    fn scene(&self) -> &Scene {
        &self.playlist.scenes[self.index]
    }
}

/// A scene's frames with its live text drawn over them (docs/data.md), kept until the text changes, so
/// an animation is encoded again once per change rather than every loop.
#[derive(Clone)]
struct Live {
    /// The live text with its placeholders filled in, as drawn.
    texts: Vec<Text>,
    frames: Arc<Vec<art::Frame>>,
    /// When drawing them started, and the work it took: the next redraw waits on it (`redraw_wait`).
    started: Instant,
    cost: Duration,
}

/// New live frames being drawn in the background, over `base`, while the old ones carry on.
struct Composing {
    base: Arc<art::Prepared>,
    texts: Vec<Text>,
    started: Instant,
    /// The frames, and the work they took.
    job: JoinHandle<(Arc<Vec<art::Frame>>, Duration)>,
}

/// The share of one processor core that redrawing live text over an animation takes, at most, on
/// average (docs/widgets.md, Live text).
const LIVE_SHARE: f64 = 0.02;
/// Live text is redrawn at most this often: as often as it's looked at.
const LIVE_EVERY: Duration = Duration::from_millis(500);

/// How long after a redraw of live text started the next may start, when it took `cost` of work: over a
/// still scene (one frame) every 0.5 s, as values are looked at; over an animation no sooner than
/// max(0.5 s, cost ÷ LIVE_SHARE), so the redraws take about LIVE_SHARE of one core.
fn redraw_wait(frames: usize, cost: Duration) -> Duration {
    if frames <= 1 { LIVE_EVERY } else { LIVE_EVERY.max(cost.div_f64(LIVE_SHARE)) }
}

/// A wait as the log shows it: `45s`, `4m 15s`.
fn span(d: Duration) -> String {
    let s = d.as_secs_f64().round() as u64;
    if s < 60 { format!("{s}s") } else { format!("{}m {}s", s / 60, s % 60) }
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
    /// The latest read. The scene showing keeps the read it was prepared for until its new version is.
    playlist: Arc<Playlist>,
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
    /// A new version of the scene showing after an edit, being prepared in the background while the old
    /// one plays, with the frames it replaces. A newer edit replaces it, and the stale result is dropped.
    reload: Option<(Arc<art::Prepared>, JoinHandle<Option<Loaded>>)>,
    /// When an edit was last read.
    edited: Option<Instant>,
    quiet: Duration,
    /// The newest re-prepared version that isn't in the disk cache yet.
    unsaved: Option<Loaded>,
    composing: Option<Composing>,
    /// The versions of scenes (by key) whose slow live text has been logged.
    slow: HashSet<u64>,
}

impl Player {
    fn new(target: &Path, opts: art::Options, seconds: f64) -> Result<Player, Box<dyn Error>> {
        let playlist = Arc::new(playlist::read(target, seconds, &opts.ffmpeg)?);
        let cache = Cache::default();
        let current = load_from(&playlist, 0, &opts, &cache, true).ok_or("none of the scenes could be loaded")?;
        let mut player = Player { target: target.to_path_buf(), opts, seconds, fingerprint: Vec::new(),
                                  changing: None, settle: SETTLE, playlist, current, cache, next: None,
                                  want_next: false, reload: None, edited: None, quiet: QUIET, unsaved: None,
                                  composing: None, slow: HashSet::new() };
        player.fingerprint = as_read(target, &player.playlist);
        player.prepare_next();
        Ok(player)
    }

    /// The scene showing, as the read it was prepared for has it.
    fn scene(&self) -> &Scene {
        self.current.scene()
    }

    /// The frames to send: with the live text on, when there is some.
    fn showing(&self) -> &[art::Frame] {
        self.current.live.as_ref().map_or(&self.current.frames.frames[..], |live| &live.frames[..])
    }

    /// Keeps the live text current: takes in finished redraws, and with `look`, starts one if the text
    /// (filled in from the values as they are now) has changed and the redraw budget allows it (going by
    /// `now`); else the change waits, and the redraw that starts later draws the values as they are then.
    /// Cheap, and free for scenes without live text. Returns whether the frames showing changed.
    fn update_live(&mut self, look: bool, now: Instant) -> bool {
        let mut changed = false;
        if self.composing.as_ref().is_some_and(|c| c.job.is_finished()) {
            let Composing { base, texts, started, job } = self.composing.take().unwrap();
            // dropped if the scene has moved on meanwhile
            if let Ok((frames, cost)) = job.join() {
                if Arc::ptr_eq(&base, &self.current.frames) {
                    self.current.live = Some(Live { texts, frames, started, cost });
                    changed = true;
                }
            }
        }
        if !look {
            return changed;
        }
        // the showing scene's own read: its texts and its over list are numbered alike
        let texts = filled(&self.current.playlist, self.current.index);
        if texts.is_empty() {
            self.composing = None;
            return self.current.live.take().is_some() || changed;
        }
        let base = &self.current.frames;
        let showing = self.current.live.as_ref().is_some_and(|l| l.texts == texts);
        let coming = self.composing.as_ref().is_some_and(|c| c.texts == texts && Arc::ptr_eq(&c.base, base));
        if !showing && !coming && self.may_redraw(now) {
            let (playlist, index, base, opts, drawn) =
                (self.current.playlist.clone(), self.current.index, self.current.frames.clone(), self.opts.clone(), texts.clone());
            let job = std::thread::spawn(move || {
                let (frames, _, cost) = timed_overlay(&playlist, index, &base, &drawn, &opts);
                (frames, cost)
            });
            self.composing = Some(Composing { base: self.current.frames.clone(), texts, started: now, job });
        }
        changed
    }

    /// Whether a redraw of the live text may start at `now`: the last one started at least `redraw_wait`
    /// ago (docs/widgets.md, Live text). A wait past 5 s is logged once per version of a scene.
    fn may_redraw(&mut self, now: Instant) -> bool {
        // when a redraw over these frames that's still on its way started
        let coming = self.composing.as_ref().filter(|c| Arc::ptr_eq(&c.base, &self.current.frames)).map(|c| c.started);
        // no live frames yet (a version taken from the memory cache): its cost isn't known, so one at a time
        let Some(live) = &self.current.live else { return coming.is_none() };
        // the last start: a redraw still on its way, else the one showing; the cost is the last known
        let started = coming.unwrap_or(live.started);
        let frames = self.current.frames.frames.len();
        let wait = redraw_wait(frames, live.cost);
        if wait > Duration::from_secs(5) && self.slow.insert(self.current.key) {
            log(format_args!("live text on {}: {frames} frames, {:.1} s of work: values update at most every {}",
                             self.scene().name, live.cost.as_secs_f64(), span(wait)));
        }
        now.saturating_duration_since(started) >= wait
    }

    fn editing(&self) -> bool {
        self.edited.is_some_and(|at| at.elapsed() < self.quiet)
    }

    fn prepare_next(&mut self) {
        if self.playlist.scenes.len() > 1 {
            // after the scene showing, where the latest read has it
            let at = self.playlist.scenes.iter().position(|s| s.id == self.scene().id).unwrap_or(self.current.index);
            let (playlist, from) = (self.playlist.clone(), at + 1);
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
                self.playlist = Arc::new(playlist);
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
                    // the old version, with its own read, carries on until this one is ready
                    None => {
                        let (playlist, opts, cache) = (self.playlist.clone(), self.opts.clone(), self.cache.clone());
                        let reload = std::thread::spawn(move || load_from(&playlist, at, &opts, &cache, false));
                        self.reload = Some((self.current.frames.clone(), reload));
                    }
                }
                self.fingerprint = as_read(&self.target, &self.playlist);
            }
            Err(e) => {
                log(format_args!("couldn't read {}, carrying on: {e}", self.target.display()));
                // as the files are now, so a broken file isn't read again until it changes
                self.fingerprint = playlist::fingerprint(&self.target, &self.playlist);
            }
        }
        changed
    }

    /// Takes in background work that has finished, and starts what waits for edits to stop. Cheap, so
    /// it runs between frames. Returns whether the frames showing changed.
    fn tick(&mut self) -> bool {
        let mut changed = false;
        if self.reload.as_ref().is_some_and(|(_, h)| h.is_finished()) {
            let (replaces, reload) = self.reload.take().unwrap();
            if let Some(loaded) = reload.join().ok().flatten() {
                self.unsaved = Some(loaded.clone());
                // swapped in, with its own read, unless the playlist has moved on to another scene meanwhile
                if Arc::ptr_eq(&replaces, &self.current.frames) {
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
            if self.update_live(look, now) && !restart && self.showing().len() == 1 {
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

/// The playlist and its files as the read that made `playlist` saw them: the stamps it took as it read
/// each (Playlist.watch), so a save that lands during the read counts as a change. A read that took none
/// is stamped now.
fn as_read(target: &Path, playlist: &Playlist) -> Fingerprint {
    if playlist.watch.is_empty() { playlist::fingerprint(target, playlist) } else { playlist.watch.clone() }
}

fn key(playlist: &Playlist, index: usize, opts: &art::Options) -> u64 {
    scene::key(&playlist.scenes[index], &playlist.global, opts)
}

/// A scene, if it's in memory as it is now (its key covers its files' stamps).
fn cached(cache: &Cache, playlist: &Arc<Playlist>, index: usize, opts: &art::Options) -> Option<Loaded> {
    let key = key(playlist, index, opts);
    let frames = cache.lock().unwrap().get(&key)?.clone();
    Some(Loaded { playlist: playlist.clone(), index, frames, key, live: None })
}

/// A scene's live text (its own, then the global layers', in drawing order: the order its over list
/// numbers them in), filled in from the values as they are now.
fn filled(playlist: &Playlist, index: usize) -> Vec<Text> {
    playlist::live_texts(&playlist.scenes[index], &playlist.global)
        .into_iter()
        .map(|w| Text { text: data::resolve(&w.text), ..w })
        .collect()
}

/// The frames with `texts` drawn in their over list and laid over them; the same frames when that draws
/// nothing.
fn overlay(playlist: &Playlist, index: usize, prepared: &art::Prepared, texts: &[Text], opts: &art::Options) -> Arc<Vec<art::Frame>> {
    Arc::new(art::overlay(&playlist.scenes[index], &playlist.global, prepared, texts, opts))
}

/// `overlay`, with when it started and the work it took: its time on each of the threads it ran on.
fn timed_overlay(playlist: &Playlist, index: usize, prepared: &art::Prepared, texts: &[Text], opts: &art::Options)
    -> (Arc<Vec<art::Frame>>, Instant, Duration) {
    let started = Instant::now();
    let frames = overlay(playlist, index, prepared, texts, opts);
    (frames, started, started.elapsed() * art::workers(prepared.frames.len()) as u32)
}

/// Loads the first scene that works, starting at `from` and wrapping around, with its live text drawn on
/// as the values are now (never held back by the redraw budget). `save` keeps newly prepared scenes in
/// the disk cache too.
fn load_from(playlist: &Arc<Playlist>, from: usize, opts: &art::Options, cache: &Cache, save: bool) -> Option<Loaded> {
    let n = playlist.scenes.len();
    (0..n).map(|k| (from + k) % n).find_map(|index| {
        let mut loaded = cached(cache, playlist, index, opts).or_else(|| prepare(playlist, index, opts, cache, save))?;
        let texts = filled(playlist, index);
        if !texts.is_empty() {
            let (frames, started, cost) = timed_overlay(playlist, index, &loaded.frames, &texts, opts);
            loaded.live = Some(Live { texts, frames, started, cost });
        }
        Some(loaded)
    })
}

/// Drops scenes other than `keep` from the in-memory cache until what it holds (frames and over lists)
/// is within `cap`.
fn trim(cache: &mut HashMap<u64, Arc<art::Prepared>>, keep: u64, cap: usize) {
    while cache.values().map(|p| p.bytes()).sum::<usize>() > cap && cache.len() > 1 {
        let victim = cache.keys().find(|k| **k != keep).copied().unwrap();
        cache.remove(&victim);
    }
}

/// Prepares a scene and keeps it in the in-memory cache.
fn prepare(playlist: &Arc<Playlist>, index: usize, opts: &art::Options, cache: &Cache, save: bool) -> Option<Loaded> {
    let scene = &playlist.scenes[index];
    let key = key(playlist, index, opts);
    let start = Instant::now();
    match art::load(scene, &playlist.global, opts, save) {
        Ok(prepared) => {
            log(format_args!("ready: {} ({} frame(s), {} KB, {} ms)", scene.name, prepared.frames.len(), prepared.bytes() / 1024,
                             start.elapsed().as_millis()));
            let frames = Arc::new(prepared);
            let mut cache = cache.lock().unwrap();
            cache.insert(key, frames.clone());
            trim(&mut cache, key, CACHE_BYTES);
            Some(Loaded { playlist: playlist.clone(), index, frames, key, live: None })
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

    /// A time by which any redraw budget has long run out.
    fn whenever() -> Instant {
        Instant::now() + Duration::from_secs(24 * 3600)
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
        assert_eq!(first.texts[0].text, "n=1");
        assert_eq!(first.frames.len(), base.len());
        assert!(first.frames.iter().zip(&base).all(|(live, base)| live.jpeg != *base));
        assert!(std::ptr::eq(p.showing().as_ptr(), first.frames.as_ptr()));

        // the same text: nothing to draw, however often it looks
        for _ in 0..5 {
            assert!(!p.update_live(true, whenever()));
            assert!(p.composing.is_none());
        }
        // a new value: drawn once, in the background, with the old frames showing meanwhile
        set(2.0);
        assert!(!p.update_live(true, whenever()));
        assert!(p.composing.is_some());
        assert!(!p.update_live(true, whenever())); // already on its way
        assert!(std::ptr::eq(p.showing().as_ptr(), first.frames.as_ptr()));
        assert!(eventually(|| p.update_live(false, whenever())));
        let second = p.current.live.clone().unwrap();
        assert_eq!(second.texts[0].text, "n=2");
        assert!(!Arc::ptr_eq(&second.frames, &first.frames));
        assert!(!p.update_live(true, whenever()));
        assert!(p.composing.is_none());

        // the live text gone: back to the prepared frames as they are, once that version is swapped in
        s.write("");
        read(&mut p);
        assert!(eventually(|| {
            p.tick();
            p.reload.is_none()
        }));
        p.update_live(true, whenever());
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

    /// Sets a value live text can show, as a source would.
    fn set(key: &str, n: f64) {
        data::store().put("wvtest", Duration::from_secs(180), vec![(key.into(), data::Value::Number(n))]);
    }

    /// The indices of the over list's texts, in the order they're drawn.
    fn numbered(over: &[art::Over], out: &mut Vec<usize>) {
        for node in over {
            match node {
                art::Over::Text { index, .. } => out.push(*index),
                art::Over::Group { children, .. } => numbered(children, out),
                art::Over::Still(_) => {}
            }
        }
    }

    /// The object as one of a copy's leaves: inside `clip`, and in `groups`.
    fn as_leaf(o: &mut playlist::Object, clip: playlist::Rect, groups: &[(&str, f32)]) {
        o.clip = Some(clip);
        o.groups = groups.iter().map(|(id, opacity)| playlist::Group { id: id.to_string(), opacity: *opacity, rect: clip }).collect();
        if let Kind::Text(w) = &mut o.kind {
            w.clip = Some(clip);
        }
    }

    /// filled() lists the live text in the order the over list numbers it: drawing order, into copies and
    /// translucent layers, then the global layers; in place and on top alike.
    #[test]
    fn filled_matches_over() {
        let s = Scratch::new("numbered");
        let json = r##"{ "version": 2, "global": { "layers": [ { "id": "g", "objects": [ { "id": "clock", "type": "text", "text": "{time}" } ] } ] },
            "scenes": [ { "id": "s", "layers": [
              { "id": "l1", "objects": [
                { "id": "bg", "type": "shape", "shape": "rect", "w": 1600, "h": 720, "fill": { "type": "solid", "color": "#203040" } },
                { "id": "a", "type": "text", "text": "{time}" },
                { "id": "c/v", "type": "text", "text": "{pc.cpu.temp}", "x": 100, "y": 100 },
                { "id": "c/u", "type": "shape", "shape": "rect", "x": 100, "y": 130, "w": 50, "h": 5 },
                { "id": "d/v", "type": "text", "text": "{pc.gpu.temp}", "x": 300, "y": 100 } ] },
              { "id": "l2", "opacity": 0.5, "objects": [ { "id": "b", "type": "text", "text": "{pc.cpu.load}", "y": 300 } ] },
              { "id": "anim", "visible": ANIMATED, "objects": [ { "id": "gif", "type": "image", "src": "a.gif", "w": 160, "h": 72 } ] } ] } ] }"##;
        for animated in [false, true] {
            let mut p = playlist::parse(json.replace("ANIMATED", &animated.to_string()).as_bytes(), &s.dir, 30.0, Path::new("ffmpeg")).unwrap();
            let objects = &mut p.scenes[0].layers[0].objects;
            as_leaf(&mut objects[2], [100, 100, 250, 140], &[("c", 0.5)]);
            as_leaf(&mut objects[3], [100, 100, 250, 140], &[("c", 0.5)]);
            as_leaf(&mut objects[4], [300, 100, 450, 140], &[]);
            let ids: Vec<String> = filled(&p, 0).into_iter().map(|w| w.id).collect();
            assert_eq!(ids, ["a", "c/v", "d/v", "b", "clock"]);
            let over = scene::prepare(&p.scenes[0], &p.global, &Scratch::opts()).unwrap().over;
            let mut indices = Vec::new();
            numbered(&over, &mut indices);
            assert_eq!(indices, [0, 1, 2, 3, 4], "on top: {animated}");
            assert_eq!(over.iter().all(|n| matches!(n, art::Over::Text { .. })), animated);
        }
    }

    /// An edit that adds a live text (before the others, so every number moves) while the old frames are
    /// still showing: the live text keeps coming from the showing scene's own read, matching its over
    /// list, until the new version is taken in, which shows the new text too.
    #[test]
    fn edit_adds_live_text_while_showing() {
        if !Path::new(text::DEFAULT_FONT).exists() {
            return;
        }
        let s = Scratch::new("adds");
        let json = |extra: bool| {
            let c = r#"{ "id": "c", "type": "text", "text": "{wvtest.add_c}", "x": 100, "y": 20 },"#;
            format!(r##"{{ "version": 2, "scenes": [ {{ "id": "s", "seconds": 1, "layers": [ {{ "id": "l", "objects": [
                {{ "id": "bg", "type": "shape", "shape": "rect", "w": 1600, "h": 720, "fill": {{ "type": "solid", "color": "#203040" }} }},
                {} {{ "id": "a", "type": "text", "text": "{{wvtest.add_a}}", "x": 100, "y": 100 }},
                {{ "id": "cover", "type": "shape", "shape": "rect", "x": 120, "y": 90, "w": 30, "h": 80 }},
                {{ "id": "b", "type": "text", "text": "{{wvtest.add_b}}", "x": 100, "y": 300 }} ] }} ] }} ] }}"##,
                    if extra { c } else { "" })
        };
        let ids = |p: &Player| -> Vec<String> { p.current.live.as_ref().unwrap().texts.iter().map(|w| w.id.clone()).collect() };
        let texts_over = |p: &Player| {
            let mut indices = Vec::new();
            numbered(&p.current.frames.over, &mut indices);
            indices.len()
        };
        for (key, n) in [("wvtest.add_a", 1.0), ("wvtest.add_b", 2.0), ("wvtest.add_c", 3.0)] {
            set(key, n);
        }
        std::fs::write(&s.list, json(false)).unwrap();
        let mut p = s.player();
        p.settle = Duration::from_millis(50);
        assert_eq!(ids(&p), ["a", "b"]);
        let showing = p.current.frames.clone();

        std::fs::write(&s.list, json(true)).unwrap();
        read(&mut p);
        assert!(p.reload.is_some(), "the new version should be prepared in the background");
        // a new value meanwhile: drawn with the showing version's own two texts
        set("wvtest.add_a", 10.0);
        p.update_live(true, whenever());
        assert!(eventually(|| p.update_live(false, whenever())));
        assert!(Arc::ptr_eq(&p.current.frames, &showing));
        assert_eq!(ids(&p), ["a", "b"]);
        assert_eq!(p.current.live.as_ref().unwrap().texts[0].text, "10");
        assert_eq!(texts_over(&p), 2);

        // the new version taken in: three texts, numbered as its over list is
        assert!(eventually(|| p.tick()));
        assert!(!Arc::ptr_eq(&p.current.frames, &showing));
        assert_eq!(ids(&p), ["c", "a", "b"]);
        assert_eq!(texts_over(&p), 3);
        p.update_live(true, whenever());
        assert!(p.composing.is_none(), "nothing changed");
    }

    /// Over an animation, a redraw that took `c` of work holds the next back until max(0.5 s, 50 × c)
    /// after it started, and the one that starts then shows the latest value; over a still scene,
    /// values show every 0.5 s.
    #[test]
    fn redraw_budget() {
        if !Path::new(text::DEFAULT_FONT).exists() {
            return;
        }
        let ms = Duration::from_millis;
        assert_eq!(redraw_wait(3, ms(1000)), Duration::from_secs(50));
        assert_eq!(redraw_wait(3, ms(1)), ms(500));
        assert_eq!(span(redraw_wait(1800, Duration::from_secs_f64(5.1))), "4m 15s");
        let s = Scratch::new("budget");
        set("wvtest.budget", 0.0);
        std::fs::write(&s.list, Scratch::json("n={wvtest.budget}")).unwrap();
        let mut p = s.player();
        assert_eq!(p.current.frames.frames.len(), 3);
        // a fake clock, and a fake cost of 1 s for the first drawing
        let t0 = whenever();
        let fake = |p: &mut Player, started: Instant, cost: Duration| {
            let live = p.current.live.as_mut().unwrap();
            (live.started, live.cost) = (started, cost);
        };
        fake(&mut p, t0, ms(1000));
        // changes 0.1 s apart start nothing until 50 s after the last start
        for k in 1..=10u32 {
            set("wvtest.budget", k as f64);
            assert!(!p.update_live(true, t0 + ms(100) * k));
            assert!(p.composing.is_none());
        }
        set("wvtest.budget", 99.0);
        p.update_live(true, t0 + ms(49_900));
        assert!(p.composing.is_none());
        p.update_live(true, t0 + ms(50_000));
        assert_eq!(p.composing.as_ref().expect("no redraw after 50 s").texts[0].text, "n=99");
        assert!(eventually(|| p.update_live(false, t0 + ms(50_000))));
        assert_eq!(p.current.live.as_ref().unwrap().texts[0].text, "n=99");
        assert!(p.slow.contains(&p.current.key), "a wait past 5 s is logged");
        // that redraw's own cost (1 s, faked) holds the next back 50 s from when it started
        fake(&mut p, t0 + ms(50_000), ms(1000));
        set("wvtest.budget", 100.0);
        p.update_live(true, t0 + ms(99_900));
        assert!(p.composing.is_none());
        p.update_live(true, t0 + ms(100_000));
        assert!(p.composing.is_some());

        // a still scene: every 0.5 s, whatever its redraw costs
        RgbaImage::from_pixel(160, 72, Rgba([0, 0, 90, 255])).save(s.dir.join("still.png")).unwrap();
        let still = r#"{ "seconds": 1, "overlay": [ { "id": "clock", "type": "text", "text": "n={wvtest.still}", "size": 100 } ],
                         "items": [ { "file": "still.png", "widgets": [] } ] }"#;
        std::fs::write(&s.list, still).unwrap();
        let mut p = s.player();
        assert_eq!(p.current.frames.frames.len(), 1);
        for (k, cost) in [(1u32, ms(10)), (2, ms(50))] {
            let start = t0 + ms(500) * (k - 1);
            fake(&mut p, start, cost);
            set("wvtest.still", k as f64);
            p.update_live(true, start + ms(400));
            assert!(p.composing.is_none());
            p.update_live(true, start + ms(500));
            assert!(p.composing.is_some(), "no redraw 0.5 s after the last over a still scene");
            assert!(eventually(|| p.update_live(false, start + ms(500))));
        }
        assert!(!p.slow.contains(&p.current.key));
    }

    /// A version taken back from the memory cache has no live frames, so what its redraw costs isn't known
    /// yet: its first redraw starts at once, changes wait while it's on its way (rather than each starting
    /// another over the whole cycle), and from then on its cost holds the next back.
    #[test]
    fn redraw_budget_after_cache_hit() {
        if !Path::new(text::DEFAULT_FONT).exists() {
            return;
        }
        let ms = Duration::from_millis;
        let s = Scratch::new("budget-hit");
        set("wvtest.hit", 0.0);
        let sized = |size: u32| {
            format!(r#"{{ "seconds": 1, "overlay": [ {{ "id": "clock", "type": "text", "text": "n={{wvtest.hit}}", "size": {size} }} ],
                          "items": [ {{ "file": "a.gif", "widgets": [] }} ] }}"#)
        };
        std::fs::write(&s.list, sized(100)).unwrap();
        let mut p = s.player();
        p.settle = ms(50);
        // another version, swapped in, then back to the first: still in memory, with other frames
        std::fs::write(&s.list, sized(80)).unwrap();
        read(&mut p);
        assert!(eventually(|| p.tick()));
        std::fs::write(&s.list, sized(100)).unwrap();
        assert!(read(&mut p));
        assert!(p.reload.is_none(), "the first version isn't taken from the memory cache");
        assert!(p.current.live.is_none());
        assert_eq!(p.current.frames.frames.len(), 3);
        // the first redraw isn't held back; here it's kept on its way, with a fake cost of 1 s
        let t0 = whenever();
        p.update_live(true, t0);
        let coming = p.composing.as_mut().expect("the first redraw was held back");
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let frames = Arc::new(p.current.frames.frames.clone());
        coming.job = std::thread::spawn(move || {
            let _ = wait.recv();
            (frames, ms(1000))
        });
        let first = coming.job.thread().id();
        // values changing every look wait for it
        for k in 1..=6u32 {
            set("wvtest.hit", k as f64);
            p.update_live(true, t0 + ms(500) * k);
            assert_eq!(p.composing.as_ref().map(|c| c.job.thread().id()), Some(first), "another redraw started at look {k}");
        }
        go.send(()).unwrap();
        assert!(eventually(|| p.update_live(false, t0 + ms(3000))));
        // then its cost holds the next back until 50 s after it started, and that one shows the last value
        p.update_live(true, t0 + ms(49_900));
        assert!(p.composing.is_none());
        p.update_live(true, t0 + ms(50_000));
        assert_eq!(p.composing.as_ref().expect("no redraw after 50 s").texts[0].text, "n=6");
    }

    /// The memory cache counts the stills laid over a scene's frames, not only its JPEGs, so scenes with
    /// big translucent copies can't hold more than its cap.
    #[test]
    fn over_stills_count_in_cache() {
        let still = |n: u32| art::Over::Still(Arc::new(scene::Buf { left: 0, top: 0, image: RgbaImage::new(n, n) }));
        let prepared = |n: u32| {
            let frames = vec![art::Frame { jpeg: vec![0; 1000], duration: Duration::ZERO }];
            let over = vec![still(n), art::Over::Text { index: 0, fold: 1.0 },
                            art::Over::Group { opacity: 0.5, rect: [0, 0, 10, 10], children: vec![still(n)] }];
            Arc::new(art::Prepared { frames, over })
        };
        assert_eq!(prepared(100).bytes(), 1000 + 2 * 100 * 100 * 4);
        let mut cache = HashMap::new();
        cache.insert(1, prepared(100));
        cache.insert(2, prepared(100));
        // 162 KB in all, nearly all of it stills
        trim(&mut cache, 2, 200_000);
        assert_eq!(cache.len(), 2);
        trim(&mut cache, 2, 100_000);
        assert_eq!(cache.keys().collect::<Vec<_>>(), [&2]);
    }

    /// Editing a widget.json while playing reads the playlist again: the scene using the widget is
    /// prepared again, and the other comes from the cache, its key unchanged. Needs work package P1's
    /// reading of widgets.
    #[test]
    #[ignore = "until P1 merges"]
    fn widget_edit_reprepares_users() {
        let root = std::env::temp_dir().join(format!("wvplay-test-widget-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("playlist")).unwrap();
        std::fs::create_dir_all(root.join("widgets").join("w")).unwrap();
        let widget = |color: &str| {
            format!(r#"{{ "version": 1, "name": "W", "w": 100, "h": 40, "layers": [ {{ "id": "l1", "objects": [
                {{ "id": "r", "type": "shape", "shape": "rect", "x": 0, "y": 0, "w": 100, "h": 40, "fill": {{ "type": "solid", "color": "{color}" }} }} ] }} ] }}"#)
        };
        let def = root.join("widgets").join("w").join("widget.json");
        std::fs::write(&def, widget("#ff0000")).unwrap();
        let list = root.join("playlist").join("playlist.json");
        std::fs::write(&list, r#"{ "version": 2, "scenes": [
            { "id": "uses", "seconds": 1, "layers": [ { "id": "l", "objects": [ { "id": "c", "type": "widget", "widget": "w", "x": 10, "y": 10, "scale": 1 } ] } ] },
            { "id": "other", "seconds": 1, "layers": [ { "id": "l", "objects": [ { "id": "r", "type": "shape", "shape": "rect", "w": 50, "h": 50 } ] } ] } ] }"#).unwrap();
        let mut p = Player::new(&list, Scratch::opts(), 30.0).unwrap();
        p.settle = Duration::from_millis(50);
        assert!(p.playlist.scenes[0].layers[0].objects.iter().any(|o| o.id == "c/r"), "the copy isn't expanded");
        // the other scene, prepared ahead, is in memory
        assert!(eventually(|| p.next.as_ref().is_some_and(|h| h.is_finished())));
        assert!(cached(&p.cache, &p.playlist, 1, &p.opts).is_some());
        let (uses, other) = (key(&p.playlist, 0, &p.opts), key(&p.playlist, 1, &p.opts));
        sleep(Duration::from_millis(20));
        std::fs::write(&def, widget("#00ff00")).unwrap();
        read(&mut p);
        assert_ne!(key(&p.playlist, 0, &p.opts), uses);
        assert_eq!(key(&p.playlist, 1, &p.opts), other);
        assert!(p.reload.is_some(), "the scene using the widget isn't prepared again");
        assert!(eventually(|| p.tick()));
        assert_eq!(p.current.key, key(&p.playlist, 0, &p.opts));
        assert!(cached(&p.cache, &p.playlist, 1, &p.opts).is_some(), "the other scene isn't taken from the cache");
        let _ = std::fs::remove_dir_all(&root);
    }
}
