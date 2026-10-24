//! The state machine behind mod.rs's functions: whether PawnIO is wanted, whether this process may ask,
//! what the checks (§4.2) say, and the reader once it answers. It owns no Windows calls of its own; the
//! slow ones (raising the prompt, connecting the pipe, waiting for the reader to exit) are an `Env`, so
//! tests drive it on a fake one. The process-wide one uses `RealEnv` (elevation.rs), or, in tests, an
//! `InertEnv` that is never elevated and never launches.
//!
//! mod.rs's functions lock the one `Control` and delegate, then, with the lock let go, log each change, wake
//! the pc poll when the state changed and start the launch the checks asked for. The launch and the pipe
//! reads never hold the lock.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value as Json, json};

use super::blobs::DAMAGED;
use super::{Label, Readings};

/// The reader on its pipe, from the player's side (client.rs; a fake in tests).
pub trait Pipe: Send + Sync {
    /// One `{"op":"read"}` and the readings it answers with, or why it couldn't (a timeout, a broken pipe).
    fn read(&self) -> Result<Readings, String>;
    /// `{"op":"quit"}`, so the reader exits; dropping the pipe does the same on EOF.
    fn quit(&self);
}

/// The reader's process, once launched: its id (to check the pipe is really its), and a wait for it to end.
pub trait Launched: Send {
    fn pid(&self) -> u32;
    /// The pipe the reader makes, from this player's id.
    fn pipe_name(&self) -> String;
    /// Its exit code once it has exited, without waiting; None while it runs.
    fn exited(&self) -> Option<i32>;
    /// Blocks until the reader exits, and gives its exit code.
    fn wait(self: Box<Self>) -> i32;
}

/// Why connecting to the reader's pipe failed.
#[derive(Debug, PartialEq, Eq)]
pub enum ConnectError {
    /// The reader exited before it opened its pipe (its checks, or a failed install): its exit code says why.
    Exited(i32),
    /// The sentence to show.
    Failed(String),
}

/// What this process can do about PawnIO's hardware: the §4.2 checks, and the slow steps (launch, connect,
/// wait) kept off the lock. The real one is elevation.rs's `RealEnv`.
pub trait Env: Send + Sync + 'static {
    fn is_elevated(&self) -> bool;
    fn elevation(&self) -> Elevation;
    /// PawnIO's installed version (its uninstall entry's DisplayVersion), or None when it isn't installed.
    fn installed_version(&self) -> Option<String>;
    /// Whether the PawnIO driver's service is running.
    fn service_running(&self) -> bool;
    /// `C:\Program Files` as the registry has it (never the environment), where the reader is installed.
    fn program_files(&self) -> Option<PathBuf>;
    /// `wvsensors.exe` beside the player, the copy the installer runs; None when the player's path is unknown.
    fn build_reader(&self) -> Option<PathBuf>;
    /// A file's SHA-256, or None when it isn't there.
    fn sha256(&self, path: &std::path::Path) -> Option<String>;
    /// Whether `path` can be changed only by an administrator (owner and DACL, §4.2).
    fn admin_only(&self, path: &std::path::Path) -> bool;
    fn now(&self) -> Duration;
    /// Raises the prompt (or, when already elevated, starts it with no prompt) and starts the reader.
    /// `Err(1223)` is the user declining; any other `Err` is its Win32 code.
    fn launch(&self, target: &std::path::Path, install: bool, elevated: bool) -> Result<Box<dyn Launched>, u32>;
    /// Connects to the reader's pipe, checks it's really the launched process, and reads its hello. Gives up
    /// at once when the reader exits first.
    fn connect(&self, launched: &dyn Launched, timeout: Duration) -> Result<Box<dyn Pipe>, ConnectError>;
}

/// This Windows account's rights, from its token (R-E §1.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Elevation {
    /// No linked token: a standard user, the built-in Administrator, or UAC off.
    Default,
    /// An elevated token.
    Full,
    /// A split-token administrator's limited token, which "runas" can elevate with a consent prompt.
    Limited,
}

/// The three not-asked wordings (§1.2): the reader is ready to run, to be set up, or to be updated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NotAsked {
    Ask,
    SetUp,
    Update,
}

/// The one state the player keeps for PawnIO (§1.2).
#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Off,
    NotInstalled,
    NotRunning,
    Unavailable,
    NeedsAdmin,
    NotAsked(NotAsked),
    Asking,
    Declined,
    Failed(String),
    Ok,
}

/// What `read` should do: read the connected reader, show a note, or (off and not even wanted) nothing.
pub enum ReadTarget {
    Reader(Arc<dyn Pipe>),
    Note(String),
}

/// One launch asked for: which copy, whether to install it first, whether this process is already elevated
/// (so no prompt), and the generation that asked, so a stale launch is ignored.
pub struct Plan {
    gen_id: u64,
    target: PathBuf,
    install: bool,
    elevated: bool,
}

/// The launch in flight: its generation, and what it starts (for the log).
struct InFlight {
    gen_id: u64,
    target: PathBuf,
    elevated: bool,
}

/// A finished launch, back from `Env`.
enum Fin {
    Connected(Box<dyn Pipe>, Box<dyn Launched>),
    Declined,
    /// The reader exited before it opened its pipe, with this code.
    Exited(i32),
    Failed(String),
}

/// What picking the reader to run came to (§4.2).
enum Pick {
    /// The installed reader is current: run it.
    Run(PathBuf),
    /// Run the build-folder reader with `--install`; `update` tells set-up from update.
    Install { target: PathBuf, update: bool },
    Failed(String),
}

const NOT_ON_YET: &str = "PawnIO isn't on yet: save to turn it on.";
const DIDNT_ANSWER: &str = "PawnIO's sensor reader didn't answer: choose Ask again, or restart the player.";
const NO_READER: &str = "wvsensors.exe isn't beside the player: rebuild the player (cargo build --release).";

const MSG_NOT_INSTALLED: &str =
    "PawnIO isn't installed: get it from pawnio.eu (or run `winget install namazso.PawnIO`), then choose Check again.";
const MSG_NOT_RUNNING: &str =
    "PawnIO is installed but its driver isn't running: restart Windows (or reinstall PawnIO), then choose Check again.";
const MSG_UNAVAILABLE: &str = "PawnIO sensors are read only by the player while it plays (wvplay play).";
const MSG_NEEDS_ADMIN: &str = "PawnIO needs administrator rights, and this Windows account isn't an administrator.";
const MSG_ASK: &str = "PawnIO is on: choose Ask now, and Windows asks for administrator rights for its sensor reader.";
const MSG_SET_UP: &str =
    "PawnIO is on: choose Ask now to install Wonder Vision's sensor reader in Program Files (Windows asks for administrator rights).";
const MSG_UPDATE: &str =
    "The sensor reader in Program Files is from another build of Wonder Vision: choose Ask now to update it (Windows asks for administrator rights).";
const MSG_ASKING: &str =
    "Waiting for you to allow administrator rights on this PC for PawnIO's sensor reader (look for the Windows prompt, or a flashing shield on the taskbar).";
const MSG_DECLINED: &str = "Administrator rights were declined, so PawnIO sensors are off: choose Ask again, or restart the player.";
const MSG_OK: &str = "Reading PawnIO sensors (Wonder Vision's sensor reader runs as administrator).";

/// How long between prompts, at most (§4.3), and how long the reader has to answer the pipe.
const PROMPT_EVERY: Duration = Duration::from_secs(10);
const CONNECT: Duration = Duration::from_secs(10);
const CONNECT_INSTALL: Duration = Duration::from_secs(30);

/// The state machine. One per process, plus the test ones.
pub struct Control<E: Env> {
    env: Arc<E>,
    /// Whether an enabled pc source has `pawnio` on.
    wanted: bool,
    /// Whether this process is `wvplay play` (the only one that may ask); `allow_elevation` says so.
    started: bool,
    /// The start of `play`, until the first check after the API binds: the prompt waits for that.
    at_play_start: bool,
    /// Whether the API has its port; the start prompt waits for it.
    bound: bool,
    state: State,
    /// The reader, once connected, with the generation of the launch that started it.
    reader: Option<(u64, Arc<dyn Pipe>)>,
    /// Counts launches, so a reader's exit, a read's answer or its failure that isn't the connected
    /// reader's is ignored.
    generation: u64,
    /// The launch being raised or connecting. Only its own finish clears it, so turning PawnIO off and on
    /// while its prompt is up never raises a second.
    launching: Option<InFlight>,
    /// When the last prompt went up, for the one-per-10 s limit; turning off doesn't forget it.
    last_prompt: Option<Duration>,
    /// A name and label for every key read since PawnIO was last turned on, sorted by key.
    labels: BTreeMap<String, Label>,
    /// The launch the last check asked for, for whoever drives it next, off the lock.
    pending: Option<Plan>,
    /// The connected reader's process to wait on, for the watch, off the lock.
    watch: Option<(u64, Box<dyn Launched>)>,
    /// Whether the state changed since last asked, so the pc poll is woken once per change.
    changed: bool,
    /// Lines for the log since last asked (§3.9), written with the lock let go.
    logs: Vec<String>,
    /// The connected reader's notes already logged, so each is logged once.
    noted: BTreeSet<String>,
}

impl<E: Env> Control<E> {
    pub fn new(env: Arc<E>) -> Control<E> {
        Control { env, wanted: false, started: false, at_play_start: false, bound: false, state: State::Off, reader: None,
                  generation: 0, launching: None, last_prompt: None, labels: BTreeMap::new(), pending: None, watch: None,
                  changed: false, logs: Vec::new(), noted: BTreeSet::new() }
    }

    fn env(&self) -> Arc<E> {
        self.env.clone()
    }

    /// Whether an enabled pc source has `pawnio` on. Turning it off stops the reader at once; turning it on
    /// never prompts by itself. The checks run on a change only (§4.2): re-applying the same setting, as any
    /// other save does, leaves the state as it is, so a decline stays declined, a failure keeps its reason
    /// and a connected reader keeps reading.
    pub fn set_wanted(&mut self, on: bool) {
        let edge = on && !self.wanted;
        self.wanted = on;
        if !on {
            self.turn_off();
        } else if edge {
            self.evaluate(false);
        }
    }

    /// This process is `wvplay play`, so it may ask, and this is the start of `play`.
    pub fn allow_elevation(&mut self) {
        self.started = true;
        self.at_play_start = true;
    }

    /// The API has its port (only the first call counts). At the start of `play`, the prompt waits for this.
    pub fn api_bound(&mut self) {
        if self.bound {
            return;
        }
        self.bound = true;
        if self.wanted {
            self.evaluate(false);
        }
        self.at_play_start = false;
    }

    /// The API couldn't have its port (another player has it). It's no longer the start of `play`: if it
    /// gets the port later, when that player has gone, it readies but doesn't ask by itself.
    pub fn api_bind_failed(&mut self) {
        if !self.bound {
            self.at_play_start = false;
        }
    }

    /// The studio's Ask now / Ask again / Check again: re-run the checks, which may raise a prompt. Only in a
    /// state that offers one of them (§3.9).
    pub fn ask(&mut self) {
        if self.started && self.wanted && describe(&self.state).2 {
            self.evaluate(true);
        }
    }

    /// `{"state","message","can_ask"}` for /info and the ask reply.
    pub fn info(&self) -> Json {
        let (state, message, can_ask) = describe(&self.state);
        json!({ "state": state, "message": message, "can_ask": can_ask })
    }

    pub fn is_elevated(&self) -> bool {
        self.env.is_elevated()
    }

    pub fn labels(&self) -> Vec<Label> {
        self.labels.values().cloned().collect()
    }

    /// What `read` should do now: read the connected reader (off the lock), or a note. The note is the
    /// state's sentence, except off, where it means an unsaved test ("save to turn it on").
    pub fn read_target(&self) -> ReadTarget {
        match &self.state {
            State::Ok => match &self.reader {
                Some((_, reader)) => ReadTarget::Reader(reader.clone()),
                None => ReadTarget::Note(DIDNT_ANSWER.to_string()),
            },
            State::Off => ReadTarget::Note(NOT_ON_YET.to_string()),
            other => ReadTarget::Note(describe(other).1),
        }
    }

    /// The connected reader's generation, which `read_done` takes back with the read's answer.
    pub fn reader_gen(&self) -> Option<u64> {
        self.reader.as_ref().map(|(gen_id, _)| *gen_id)
    }

    /// A read of the reader `gen_id` finished, off the lock: its labels join the live set and its new notes
    /// are logged, or, when it failed, the reader is dropped. When that reader isn't the connected one any
    /// more (PawnIO was turned off meanwhile, or another reader started), its answer is dropped and the
    /// state's note given instead, so labels cleared by turning off never come back.
    pub fn read_done(&mut self, gen_id: u64, read: Result<Readings, String>) -> Readings {
        if self.reader_gen() != Some(gen_id) {
            return match self.read_target() {
                ReadTarget::Note(note) => Readings { notes: vec![note], ..Readings::default() },
                ReadTarget::Reader(_) => Readings::default(),
            };
        }
        match read {
            Ok(readings) => {
                self.merge_labels(&readings.labels);
                for note in &readings.notes {
                    if self.noted.insert(note.clone()) {
                        self.logs.push(format!("data: pawnio: {note}"));
                    }
                }
                readings
            }
            Err(_) => Readings { notes: vec![self.reader_failed()], ..Readings::default() },
        }
    }

    /// Adds a read's labels to the live set; a channel that's absent this poll keeps its row.
    pub fn merge_labels(&mut self, labels: &[Label]) {
        for label in labels {
            self.labels.insert(label.key.clone(), label.clone());
        }
    }

    /// The reader stopped answering: the state fails and the pipe is dropped. Gives the note to show.
    pub fn reader_failed(&mut self) -> String {
        self.reader = None;
        if self.wanted {
            self.set_state(State::Failed(DIDNT_ANSWER.to_string()));
        } else {
            self.set_state(State::Off);
        }
        DIDNT_ANSWER.to_string()
    }

    /// Turning off: stop the reader and forget the labels. A launch not started yet is dropped; one whose
    /// prompt is up stays in flight, and when it finishes with PawnIO still off, its reader is told to quit.
    fn turn_off(&mut self) {
        if let Some((_, reader)) = self.reader.take() {
            reader.quit();
        }
        self.labels.clear();
        if self.pending.take().is_some() {
            self.launching = None;
        }
        self.set_state(State::Off);
    }

    /// The §4.2 checks, in order; the first that applies sets the state. `explicit` is an ask (Ask now /
    /// Ask again / Check again), which prompts where a hot reload only readies.
    fn evaluate(&mut self, explicit: bool) {
        if !self.wanted {
            self.turn_off();
            return;
        }
        if self.env.installed_version().is_none() {
            self.set_state(State::NotInstalled);
            return;
        }
        if !self.env.service_running() {
            self.set_state(State::NotRunning);
            return;
        }
        if !self.started {
            self.set_state(State::Unavailable);
            return;
        }
        if !self.env.is_elevated() && self.env.elevation() != Elevation::Limited {
            self.set_state(State::NeedsAdmin);
            return;
        }
        // a reader is already connected or on its way: leave it, and say so (a check above may have said
        // otherwise a moment ago)
        if self.reader.is_some() {
            self.set_state(State::Ok);
            return;
        }
        if self.launching.is_some() {
            self.set_state(State::Asking);
            return;
        }
        let (target, install, update) = match self.pick() {
            Pick::Run(target) => (target, false, false),
            Pick::Install { target, update } => (target, true, update),
            Pick::Failed(sentence) => {
                self.set_state(State::Failed(sentence));
                return;
            }
        };
        // set-up and update happen only on an explicit ask
        if install && !explicit {
            self.set_state(State::NotAsked(if update { NotAsked::Update } else { NotAsked::SetUp }));
            return;
        }
        // a hot reload only readies; the studio then asks
        if !explicit && !self.at_play_start {
            self.set_state(State::NotAsked(NotAsked::Ask));
            return;
        }
        // the start of play waits for the API's port; the ask runs when `api_bound` is called
        if self.at_play_start && !self.bound {
            self.set_state(State::NotAsked(NotAsked::Ask));
            return;
        }
        // at most one prompt per 10 s
        if let Some(last) = self.last_prompt
            && self.env.now().saturating_sub(last) < PROMPT_EVERY
        {
            return;
        }
        self.last_prompt = Some(self.env.now());
        self.generation += 1;
        let elevated = self.env.is_elevated();
        self.launching = Some(InFlight { gen_id: self.generation, target: target.clone(), elevated });
        self.pending = Some(Plan { gen_id: self.generation, target, install, elevated });
        self.set_state(State::Asking);
    }

    /// Which reader to run (§4.2): the installed one if it's current and admin-only, else the build-folder
    /// one with `--install`, or a failure.
    fn pick(&self) -> Pick {
        let Some(program_files) = self.env.program_files() else {
            return Pick::Failed(NO_READER.to_string());
        };
        let folder = program_files.join("Wonder Vision");
        let installed = folder.join("wvsensors.exe");
        let build = self.env.build_reader();
        let build_there = build.as_ref().is_some_and(|b| self.env.sha256(b).is_some());
        if self.env.sha256(&installed).is_some() {
            let pawnio = folder.join("pawnio");
            let admin_only = self.env.admin_only(&folder)
                && self.env.admin_only(&installed)
                && self.env.admin_only(&pawnio)
                && self.env.admin_only(&pawnio.join("AMDFamily17.bin"))
                && self.env.admin_only(&pawnio.join("LpcIO.bin"));
            if !admin_only {
                return Pick::Failed(self.admin_only_sentence());
            }
            let same = build.as_ref().map(|b| self.env.sha256(b)) == Some(self.env.sha256(&installed));
            if !build_there || same {
                Pick::Run(installed)
            } else {
                Pick::Install { target: build.unwrap(), update: true }
            }
        } else if let Some(build) = build.filter(|_| build_there) {
            Pick::Install { target: build, update: false }
        } else {
            Pick::Failed(NO_READER.to_string())
        }
    }

    /// The launch the last check asked for, taken out for whoever drives it off the lock.
    pub fn take_launch(&mut self) -> Option<Plan> {
        self.pending.take()
    }

    /// The connected reader's process to wait on, taken out for the watch off the lock.
    pub fn take_watch(&mut self) -> Option<(u64, Box<dyn Launched>)> {
        self.watch.take()
    }

    /// Whether the state changed since last asked (then it's forgotten), so the pc poll is woken once each.
    pub fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    /// The log lines since last asked (then they're forgotten), to write with the lock let go.
    pub fn take_logs(&mut self) -> Vec<String> {
        std::mem::take(&mut self.logs)
    }

    /// A launch has finished (`run_plan`, off the lock): set the state, keep the reader, and, when it's
    /// kept, hand its process to the watch. Turned off meanwhile, its reader (if it started) is told to quit
    /// (§4.3); turned off and on again, its answer counts. A finish that isn't the launch in flight's can't
    /// happen, and its reader is never kept.
    fn finish_launch(&mut self, plan: &Plan, fin: Fin) {
        if self.launching.as_ref().map(|l| l.gen_id) != Some(plan.gen_id) {
            if let Fin::Connected(pipe, _) = fin {
                pipe.quit();
            }
            return;
        }
        self.launching = None;
        if !self.wanted {
            if let Fin::Connected(pipe, _) = fin {
                pipe.quit();
            }
            self.set_state(State::Off);
            return;
        }
        match fin {
            Fin::Declined => self.set_state(State::Declined),
            Fin::Exited(code) => {
                self.logs.push(format!("data: pawnio: the sensor reader exited before it answered (code {code})"));
                self.set_state(State::Failed(self.exit_sentence(code)));
            }
            Fin::Failed(sentence) => self.set_state(State::Failed(sentence)),
            Fin::Connected(pipe, launched) => {
                self.reader = Some((plan.gen_id, Arc::from(pipe)));
                self.watch = Some((plan.gen_id, launched));
                self.noted.clear();
                self.set_state(State::Ok);
            }
        }
    }

    /// The reader exited. While it was the connected one, the state fails with the exit code's sentence;
    /// it's never re-asked on its own.
    fn reader_exited(&mut self, gen_id: u64, code: i32) {
        if self.reader_gen() != Some(gen_id) {
            return;
        }
        self.reader = None;
        self.logs.push(format!("data: pawnio: the sensor reader exited (code {code})"));
        if self.wanted {
            self.set_state(State::Failed(self.exit_sentence(code)));
        } else {
            self.set_state(State::Off);
        }
    }

    /// The sentence for a reader that exited with `code` (§4.3).
    fn exit_sentence(&self, code: i32) -> String {
        match code {
            8 => self.admin_only_sentence(),
            9 => "Another copy of the sensor reader is running, so it couldn't be updated: stop the other player and choose Ask again."
                .to_string(),
            10 => DAMAGED.to_string(),
            _ => format!("PawnIO's sensor reader stopped (code {code}): choose Ask again, or restart the player."),
        }
    }

    /// The sentence when the reader's folder in Program Files isn't administrator-only.
    fn admin_only_sentence(&self) -> String {
        let folder = self.env.program_files().unwrap_or_else(|| PathBuf::from(r"C:\Program Files")).join("Wonder Vision");
        format!(
            "The sensor reader's folder in Program Files can be changed without administrator rights, so it isn't started: \
             delete {} (as administrator) and choose Ask again.",
            folder.display()
        )
    }

    /// Changes the state, marking the change for the pc poll and logging it once (§1.2, §3.9).
    fn set_state(&mut self, state: State) {
        if self.state != state {
            let line = match (&state, &self.launching) {
                (State::Asking, Some(InFlight { target, elevated: false, .. })) => {
                    format!("data: pawnio: asking for administrator rights ({})", target.display())
                }
                (State::Asking, Some(InFlight { target, elevated: true, .. })) => {
                    format!("data: pawnio: starting the sensor reader, already as administrator ({})", target.display())
                }
                // the state's name, and the sentence where it says more than the name: which wording, or why
                (State::NotAsked(_) | State::Failed(_), _) => {
                    let (name, message, _) = describe(&state);
                    format!("data: pawnio: {name}: {message}")
                }
                _ => format!("data: pawnio: {}", describe(&state).0),
            };
            self.logs.push(line);
            self.state = state;
            self.changed = true;
        }
    }
}

/// Runs a launch and its connect through `Env`, off the lock; the slow part.
fn run_plan<E: Env>(env: &E, plan: &Plan) -> Fin {
    match env.launch(&plan.target, plan.install, plan.elevated) {
        // the user said No
        Err(1223) => Fin::Declined,
        Err(code) => Fin::Failed(format!("Couldn't start PawnIO's sensor reader: {}.", super::elevation::system_message(code))),
        Ok(launched) => {
            let timeout = if plan.install { CONNECT_INSTALL } else { CONNECT };
            match env.connect(&*launched, timeout) {
                Ok(pipe) => Fin::Connected(pipe, launched),
                Err(ConnectError::Exited(code)) => Fin::Exited(code),
                Err(ConnectError::Failed(sentence)) => Fin::Failed(sentence),
            }
        }
    }
}

// --- the process-wide one ---

/// The env the process-wide `Control` uses: the real one, or, in tests, one that never launches.
#[cfg(not(test))]
type ProcessEnv = super::elevation::RealEnv;
#[cfg(test)]
type ProcessEnv = InertEnv;

struct Shared {
    control: Mutex<Control<ProcessEnv>>,
    notify: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

fn shared() -> &'static Shared {
    SHARED.get_or_init(|| Shared { control: Mutex::new(Control::new(Arc::new(ProcessEnv::default()))), notify: Mutex::new(None) })
}

fn fire_notify() {
    if let Some(notify) = shared().notify.lock().unwrap().as_ref() {
        notify();
    }
}

/// With the lock let go: writes the log lines, and wakes the pc poll if the state changed.
fn after(logs: Vec<String>, changed: bool) {
    for line in logs {
        crate::player::log(line);
    }
    if changed {
        fire_notify();
    }
}

/// Runs `f` under the lock, then, with the lock let go, logs, wakes the pc poll if the state changed and
/// starts the launch the checks asked for.
fn act<R>(f: impl FnOnce(&mut Control<ProcessEnv>) -> R) -> R {
    let shared = shared();
    let mut control = shared.control.lock().unwrap();
    let out = f(&mut control);
    let (logs, changed, plan) = (control.take_logs(), control.take_changed(), control.take_launch());
    drop(control);
    after(logs, changed);
    if let Some(plan) = plan {
        spawn_launch(plan);
    }
    out
}

/// Raises a prompt and connects the reader on its own thread (the call blocks while the prompt is up), then
/// watches the reader for exit. Tests never reach it: the process-wide env never asks.
fn spawn_launch(plan: Plan) {
    std::thread::spawn(move || {
        let env = shared().control.lock().unwrap().env();
        let fin = run_plan(&*env, &plan);
        let watch = {
            let mut control = shared().control.lock().unwrap();
            control.finish_launch(&plan, fin);
            let (logs, changed, watch) = (control.take_logs(), control.take_changed(), control.take_watch());
            drop(control);
            after(logs, changed);
            watch
        };
        if let Some((gen_id, launched)) = watch {
            let code = launched.wait();
            let mut control = shared().control.lock().unwrap();
            control.reader_exited(gen_id, code);
            let (logs, changed) = (control.take_logs(), control.take_changed());
            drop(control);
            after(logs, changed);
        }
    });
}

pub fn set_wanted(on: bool) {
    act(|c| c.set_wanted(on));
}

pub fn allow_elevation() {
    act(|c| c.allow_elevation());
}

pub fn api_bound() {
    act(|c| c.api_bound());
}

pub fn api_bind_failed() {
    act(|c| c.api_bind_failed());
}

pub fn ask() -> Json {
    act(|c| {
        c.ask();
        c.info()
    })
}

pub fn info() -> Json {
    shared().control.lock().unwrap().info()
}

pub fn is_elevated() -> bool {
    shared().control.lock().unwrap().is_elevated()
}

pub fn labels() -> Vec<Label> {
    shared().control.lock().unwrap().labels()
}

pub fn set_notify(f: Box<dyn Fn() + Send + Sync>) {
    *shared().notify.lock().unwrap() = Some(f);
}

/// Reads the connected reader, or gives the state's note. Never holds the lock across the pipe read, and
/// never launches.
pub fn read() -> Readings {
    let (target, gen_id) = {
        let control = shared().control.lock().unwrap();
        (control.read_target(), control.reader_gen().unwrap_or(0))
    };
    match target {
        ReadTarget::Reader(reader) => {
            let read = reader.read();
            act(|c| c.read_done(gen_id, read))
        }
        ReadTarget::Note(note) => Readings { notes: vec![note], ..Readings::default() },
    }
}

/// state → (the §1.2 state name, its sentence, whether the studio shows an ask button).
fn describe(state: &State) -> (&'static str, String, bool) {
    match state {
        State::Off => ("off", String::new(), false),
        State::NotInstalled => ("not-installed", MSG_NOT_INSTALLED.to_string(), true),
        State::NotRunning => ("not-running", MSG_NOT_RUNNING.to_string(), true),
        State::Unavailable => ("unavailable", MSG_UNAVAILABLE.to_string(), false),
        State::NeedsAdmin => ("needs-admin", MSG_NEEDS_ADMIN.to_string(), false),
        State::NotAsked(NotAsked::Ask) => ("not-asked", MSG_ASK.to_string(), true),
        State::NotAsked(NotAsked::SetUp) => ("not-asked", MSG_SET_UP.to_string(), true),
        State::NotAsked(NotAsked::Update) => ("not-asked", MSG_UPDATE.to_string(), true),
        State::Asking => ("asking", MSG_ASKING.to_string(), false),
        State::Declined => ("declined", MSG_DECLINED.to_string(), true),
        State::Failed(sentence) => ("failed", sentence.clone(), true),
        State::Ok => ("ok", MSG_OK.to_string(), false),
    }
}

/// The env the process-wide `Control` uses in tests: never elevated, never launches, so no test asks for
/// administrator rights or opens a pipe. Unit tests of the machine build their own `Control<FakeEnv>`.
#[cfg(test)]
#[derive(Default)]
pub struct InertEnv;

#[cfg(test)]
impl Env for InertEnv {
    fn is_elevated(&self) -> bool {
        false
    }
    fn elevation(&self) -> Elevation {
        Elevation::Default
    }
    fn installed_version(&self) -> Option<String> {
        None
    }
    fn service_running(&self) -> bool {
        false
    }
    fn program_files(&self) -> Option<PathBuf> {
        None
    }
    fn build_reader(&self) -> Option<PathBuf> {
        None
    }
    fn sha256(&self, _: &std::path::Path) -> Option<String> {
        None
    }
    fn admin_only(&self, _: &std::path::Path) -> bool {
        false
    }
    fn now(&self) -> Duration {
        Duration::ZERO
    }
    fn launch(&self, _: &std::path::Path, _: bool, _: bool) -> Result<Box<dyn Launched>, u32> {
        panic!("the test env never launches")
    }
    fn connect(&self, _: &dyn Launched, _: Duration) -> Result<Box<dyn Pipe>, ConnectError> {
        panic!("the test env never connects")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// What the fake env is set to answer, and what it was asked.
    #[derive(Default)]
    struct FakeState {
        elevated: bool,
        elevation: Option<Elevation>,
        installed: Option<String>,
        service: bool,
        program_files: Option<PathBuf>,
        build: Option<PathBuf>,
        files: Vec<(PathBuf, String)>,
        admin_only: Vec<PathBuf>,
        now: Duration,
        /// Each launch asked for: (target, install, elevated).
        launches: Vec<(PathBuf, bool, bool)>,
        /// What the next launch does: Err(code), or Ok with a connect result.
        launch_result: Option<Result<(), u32>>,
        connect_result: Option<Result<FakePipe, ConnectError>>,
        /// The exit code the launched reader gives when waited on.
        exit_code: i32,
    }

    #[derive(Clone, Default)]
    struct FakeEnv(Arc<Mutex<FakeState>>);

    impl FakeEnv {
        fn set(&self, f: impl FnOnce(&mut FakeState)) {
            f(&mut self.0.lock().unwrap())
        }
        fn launches(&self) -> Vec<(PathBuf, bool, bool)> {
            self.0.lock().unwrap().launches.clone()
        }
    }

    impl Env for FakeEnv {
        fn is_elevated(&self) -> bool {
            self.0.lock().unwrap().elevated
        }
        fn elevation(&self) -> Elevation {
            let state = self.0.lock().unwrap();
            state.elevation.unwrap_or(if state.elevated { Elevation::Full } else { Elevation::Limited })
        }
        fn installed_version(&self) -> Option<String> {
            self.0.lock().unwrap().installed.clone()
        }
        fn service_running(&self) -> bool {
            self.0.lock().unwrap().service
        }
        fn program_files(&self) -> Option<PathBuf> {
            self.0.lock().unwrap().program_files.clone()
        }
        fn build_reader(&self) -> Option<PathBuf> {
            self.0.lock().unwrap().build.clone()
        }
        fn sha256(&self, path: &std::path::Path) -> Option<String> {
            self.0.lock().unwrap().files.iter().find(|(p, _)| p == path).map(|(_, h)| h.clone())
        }
        fn admin_only(&self, path: &std::path::Path) -> bool {
            self.0.lock().unwrap().admin_only.iter().any(|p| p == path)
        }
        fn now(&self) -> Duration {
            self.0.lock().unwrap().now
        }
        fn launch(&self, target: &std::path::Path, install: bool, elevated: bool) -> Result<Box<dyn Launched>, u32> {
            let mut state = self.0.lock().unwrap();
            state.launches.push((target.to_path_buf(), install, elevated));
            match state.launch_result.take().unwrap_or(Ok(())) {
                Ok(()) => Ok(Box::new(FakeLaunched { code: state.exit_code })),
                Err(code) => Err(code),
            }
        }
        fn connect(&self, _: &dyn Launched, _: Duration) -> Result<Box<dyn Pipe>, ConnectError> {
            match self.0.lock().unwrap().connect_result.take() {
                Some(Ok(pipe)) => Ok(Box::new(pipe)),
                Some(Err(e)) => Err(e),
                None => Ok(Box::new(FakePipe::default())),
            }
        }
    }

    struct FakeLaunched {
        code: i32,
    }
    impl Launched for FakeLaunched {
        fn pid(&self) -> u32 {
            4242
        }
        fn pipe_name(&self) -> String {
            String::new()
        }
        fn exited(&self) -> Option<i32> {
            None
        }
        fn wait(self: Box<Self>) -> i32 {
            self.code
        }
    }

    #[derive(Clone, Default)]
    struct FakePipe {
        reads: Arc<AtomicU32>,
        quits: Arc<AtomicU32>,
        fail: bool,
    }
    impl Pipe for FakePipe {
        fn read(&self) -> Result<Readings, String> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err("broken".to_string())
            } else {
                Ok(Readings { notes: vec!["a reader note".to_string()], ..Readings::default() })
            }
        }
        fn quit(&self) {
            self.quits.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A control on a fake env, that drives each launch the checks ask for, as the process does off the lock.
    struct Rig {
        env: FakeEnv,
        control: Control<FakeEnv>,
    }

    impl Rig {
        fn new() -> Rig {
            let env = FakeEnv::default();
            Rig { control: Control::new(Arc::new(env.clone())), env }
        }

        /// This PC, ready: PawnIO installed and running, a split-token admin, Program Files known.
        fn ready() -> Rig {
            let rig = Rig::new();
            rig.env.set(|s| {
                s.installed = Some("2.1.0.0".into());
                s.service = true;
                s.elevation = Some(Elevation::Limited);
                s.program_files = Some(PathBuf::from(r"C:\Program Files"));
            });
            rig
        }

        /// The installed reader is there and current and admin-only.
        fn installed_current(&self) {
            let folder = PathBuf::from(r"C:\Program Files\Wonder Vision");
            let exe = folder.join("wvsensors.exe");
            self.env.set(|s| {
                s.files.push((exe.clone(), "same".into()));
                s.build = Some(PathBuf::from(r"C:\build\wvsensors.exe"));
                s.files.push((PathBuf::from(r"C:\build\wvsensors.exe"), "same".into()));
                for p in [&folder, &exe, &folder.join("pawnio"), &folder.join("pawnio\\AMDFamily17.bin"),
                          &folder.join("pawnio\\LpcIO.bin")] {
                    s.admin_only.push(p.clone());
                }
            });
        }

        fn state(&self) -> String {
            describe(&self.control.state).0.to_string()
        }

        fn message(&self) -> String {
            describe(&self.control.state).1
        }

        /// Drives the launch the last check asked for: as the process would, off the lock. Returns the
        /// launched reader's watch, so a test can end it.
        fn drive(&mut self) {
            while let Some(plan) = self.control.take_launch() {
                let fin = run_plan(&self.env, &plan);
                self.control.finish_launch(&plan, fin);
            }
        }

        /// Ends the connected reader with `code`, as the watch would.
        fn end_reader(&mut self, code: i32) {
            if let Some((gen_id, _)) = self.control.take_watch() {
                self.control.reader_exited(gen_id, code);
            }
        }
    }

    /// The §4.2 checks, in order: each row's state, one at a time.
    #[test]
    fn check_order() {
        // not wanted
        let mut rig = Rig::ready();
        rig.control.allow_elevation();
        assert_eq!(rig.state(), "off");
        // wanted, but PawnIO isn't installed: not-installed, even for `wvplay data` (not `play`)
        let data = Rig::new();
        let mut data = Rig { control: Control::new(Arc::new(data.env.clone())), env: data.env };
        data.env.set(|s| s.installed = None);
        data.control.set_wanted(true);
        assert_eq!(data.state(), "not-installed");
        // installed but its driver isn't running
        rig.env.set(|s| s.service = false);
        rig.control.set_wanted(true);
        assert_eq!(rig.state(), "not-running");
        // running, but this process isn't `play`
        rig.env.set(|s| s.service = true);
        let mut not_play = Rig::ready();
        not_play.control.set_wanted(true);
        assert_eq!(not_play.state(), "unavailable");
        // a standard account (not a split-token admin, not elevated); the checks run on a change
        rig.env.set(|s| s.elevation = Some(Elevation::Default));
        rig.control.set_wanted(false);
        rig.control.set_wanted(true);
        assert_eq!(rig.state(), "needs-admin");
    }

    /// At the start of `play` with a current installed reader, one prompt for the Program Files path goes
    /// up, but only once the API has its port.
    #[test]
    fn start_of_play_asks_after_api_bound() {
        let mut rig = Rig::ready();
        rig.installed_current();
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        // before the API binds: readied, not asked
        assert_eq!(rig.state(), "not-asked");
        assert!(rig.control.take_launch().is_none());
        rig.control.api_bound();
        assert_eq!(rig.state(), "asking");
        rig.drive();
        assert_eq!(rig.state(), "ok");
        assert_eq!(rig.env.launches(), [(PathBuf::from(r"C:\Program Files\Wonder Vision\wvsensors.exe"), false, false)]);
    }

    /// With no installed reader, the start readies (set-up), then an ask installs the build-folder copy.
    #[test]
    fn set_up_waits_for_an_ask() {
        let mut rig = Rig::ready();
        rig.env.set(|s| {
            s.build = Some(PathBuf::from(r"C:\build\wvsensors.exe"));
            s.files.push((PathBuf::from(r"C:\build\wvsensors.exe"), "built".into()));
        });
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        rig.control.api_bound();
        assert_eq!((rig.state().as_str(), rig.message()), ("not-asked", MSG_SET_UP.to_string()));
        assert!(rig.control.take_launch().is_none());
        rig.control.ask();
        rig.drive();
        assert_eq!(rig.state(), "ok");
        assert_eq!(rig.env.launches(), [(PathBuf::from(r"C:\build\wvsensors.exe"), true, false)]);
    }

    /// A reader in Program Files from another build readies as an update, and an ask installs the new one.
    #[test]
    fn a_different_build_is_an_update() {
        let mut rig = Rig::ready();
        let folder = PathBuf::from(r"C:\Program Files\Wonder Vision");
        let exe = folder.join("wvsensors.exe");
        rig.env.set(|s| {
            s.files.push((exe.clone(), "old".into()));
            s.build = Some(PathBuf::from(r"C:\build\wvsensors.exe"));
            s.files.push((PathBuf::from(r"C:\build\wvsensors.exe"), "new".into()));
            for p in [&folder, &exe, &folder.join("pawnio"), &folder.join("pawnio\\AMDFamily17.bin"),
                      &folder.join("pawnio\\LpcIO.bin")] {
                s.admin_only.push(p.clone());
            }
        });
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        rig.control.api_bound();
        assert_eq!((rig.state().as_str(), rig.message()), ("not-asked", MSG_UPDATE.to_string()));
    }

    /// A hot reload that turns PawnIO on readies but never launches (S-HIGH-4).
    #[test]
    fn a_hot_reload_never_launches() {
        let mut rig = Rig::ready();
        rig.installed_current();
        rig.control.allow_elevation();
        rig.control.api_bound(); // the start passes with PawnIO off
        assert_eq!(rig.state(), "off");
        rig.control.set_wanted(true); // the hot reload
        assert_eq!(rig.state(), "not-asked");
        assert!(rig.control.take_launch().is_none());
        assert!(rig.env.launches().is_empty());
    }

    /// Declining, then turning PawnIO on again five times, never launches; an ask does, and off then on
    /// readies, then an ask launches.
    #[test]
    fn decline_is_sticky() {
        let mut rig = Rig::ready();
        rig.installed_current();
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        rig.control.api_bound();
        rig.env.set(|s| s.launch_result = Some(Err(1223)));
        rig.drive();
        assert_eq!(rig.state(), "declined");
        for _ in 0..5 {
            rig.control.set_wanted(true);
        }
        assert_eq!(rig.state(), "declined");
        assert_eq!(rig.env.launches().len(), 1);
        rig.env.set(|s| s.now += Duration::from_secs(20));
        rig.control.ask();
        rig.drive();
        assert_eq!(rig.state(), "ok");
        assert_eq!(rig.env.launches().len(), 2);
        // off, then on, readies; then an ask launches
        rig.control.set_wanted(false);
        assert_eq!(rig.state(), "off");
        rig.control.set_wanted(true);
        assert_eq!(rig.state(), "not-asked");
        rig.env.set(|s| s.now += Duration::from_secs(20));
        rig.control.ask();
        rig.drive();
        assert_eq!(rig.state(), "ok");
        assert_eq!(rig.env.launches().len(), 3);
    }

    /// Ask from not-installed re-runs the checks (it's Check again, not just Ask now).
    #[test]
    fn ask_rechecks_from_not_installed() {
        let mut rig = Rig::ready();
        rig.installed_current();
        rig.env.set(|s| s.installed = None);
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        rig.control.api_bound();
        assert_eq!(rig.state(), "not-installed");
        rig.env.set(|s| s.installed = Some("2.1.0.0".into()));
        rig.control.ask();
        rig.drive();
        assert_eq!(rig.state(), "ok");
    }

    /// Two asks within 10 s raise one prompt; an ask while asking leaves it asking.
    #[test]
    fn one_prompt_per_ten_seconds() {
        let mut rig = Rig::ready();
        rig.installed_current();
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        rig.control.api_bound();
        assert_eq!(rig.state(), "asking");
        // an ask while asking: unchanged, no second launch
        rig.control.ask();
        assert_eq!(rig.state(), "asking");
        rig.drive();
        assert_eq!(rig.state(), "ok");
        // the reader stops; an ask 3 s later is inside the 10 s, so no new prompt
        rig.end_reader(5);
        assert_eq!(rig.state(), "failed");
        rig.env.set(|s| s.now += Duration::from_secs(3));
        rig.control.ask();
        assert_eq!(rig.env.launches().len(), 1);
        // past 10 s, it asks again
        rig.env.set(|s| s.now += Duration::from_secs(8));
        rig.control.ask();
        rig.drive();
        assert_eq!((rig.state().as_str(), rig.env.launches().len()), ("ok", 2));
    }

    /// An installed folder that isn't administrator-only fails, with no launch.
    #[test]
    fn user_writable_install_fails() {
        let mut rig = Rig::ready();
        let exe = PathBuf::from(r"C:\Program Files\Wonder Vision\wvsensors.exe");
        rig.env.set(|s| s.files.push((exe, "there".into()))); // present, but no admin_only entries
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        rig.control.ask();
        assert_eq!(rig.state(), "failed");
        assert!(rig.message().contains("without administrator rights"));
        assert!(rig.env.launches().is_empty());
    }

    /// Already elevated: the reader is started with no prompt (a CreateProcessW-kind launch), and nothing
    /// is read in-process.
    #[test]
    fn elevated_starts_without_a_prompt() {
        let mut rig = Rig::ready();
        rig.installed_current();
        rig.env.set(|s| {
            s.elevated = true;
            s.elevation = Some(Elevation::Full);
        });
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        rig.control.api_bound();
        rig.drive();
        assert_eq!(rig.state(), "ok");
        assert_eq!(rig.env.launches(), [(PathBuf::from(r"C:\Program Files\Wonder Vision\wvsensors.exe"), false, true)]);
    }

    /// Each reader exit code has its sentence; a reader that exits isn't re-asked.
    #[test]
    fn exit_codes_have_sentences() {
        for (code, mark) in [(5, "code 5"), (8, "without administrator rights"), (9, "Another copy"), (10, "damaged or from another version")]
        {
            let mut rig = Rig::ready();
            rig.installed_current();
            rig.control.allow_elevation();
            rig.control.set_wanted(true);
            rig.control.api_bound();
            rig.drive();
            rig.end_reader(code);
            assert_eq!(rig.state(), "failed", "code {code}");
            assert!(rig.message().contains(mark), "code {code}: {}", rig.message());
            // not re-asked on its own
            assert_eq!(rig.env.launches().len(), 1, "code {code}");
        }
    }

    /// Turned off while the prompt is up: the reader, when it connects, is told to quit, and the state is off.
    #[test]
    fn turned_off_while_asking() {
        let mut rig = Rig::ready();
        rig.installed_current();
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        rig.control.api_bound();
        assert_eq!(rig.state(), "asking");
        let plan = rig.control.take_launch().unwrap();
        // the user turns it off before the prompt is answered
        rig.control.set_wanted(false);
        let fin = run_plan(&rig.env, &plan);
        rig.control.finish_launch(&plan, fin);
        assert_eq!(rig.state(), "off");
        assert!(rig.control.take_watch().is_none());
    }

    /// `read` never launches, and gives the reader's own notes when connected, or the state's note otherwise.
    #[test]
    fn read_reads_or_notes() {
        let mut rig = Rig::ready();
        rig.installed_current();
        // off and not wanted: the unsaved-test note
        assert!(matches!(rig.control.read_target(), ReadTarget::Note(n) if n == NOT_ON_YET));
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        rig.control.api_bound();
        // asking: the state's note
        assert!(matches!(rig.control.read_target(), ReadTarget::Note(n) if n == MSG_ASKING));
        rig.drive();
        // connected: the reader is read
        match rig.control.read_target() {
            ReadTarget::Reader(reader) => assert_eq!(reader.read().unwrap().notes, ["a reader note"]),
            ReadTarget::Note(n) => panic!("{n}"),
        }
        assert_eq!(rig.env.launches().len(), 1);
    }

    /// A reader that stops answering fails the state and drops the reader.
    #[test]
    fn a_broken_reader_fails() {
        let mut rig = Rig::ready();
        rig.installed_current();
        rig.env.set(|s| s.connect_result = Some(Ok(FakePipe { fail: true, ..FakePipe::default() })));
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        rig.control.api_bound();
        rig.drive();
        assert_eq!(rig.state(), "ok");
        let reader = match rig.control.read_target() {
            ReadTarget::Reader(reader) => reader,
            ReadTarget::Note(n) => panic!("{n}"),
        };
        assert!(reader.read().is_err());
        let note = rig.control.reader_failed();
        assert_eq!(note, DIDNT_ANSWER);
        assert_eq!(rig.state(), "failed");
        assert!(matches!(rig.control.read_target(), ReadTarget::Note(_)));
    }

    /// Every state change marks the poll to be woken, once each.
    #[test]
    fn each_change_is_marked_once() {
        let mut rig = Rig::ready();
        rig.installed_current();
        rig.control.allow_elevation();
        rig.control.set_wanted(true); // off -> not-asked (start, before bind)
        assert!(rig.control.take_changed());
        assert!(!rig.control.take_changed());
        rig.control.api_bound(); // -> asking
        assert!(rig.control.take_changed());
        rig.drive(); // -> ok
        assert!(rig.control.take_changed());
        assert!(!rig.control.take_changed());
    }

    /// Labels build up across reads, and clear when PawnIO is turned off.
    #[test]
    fn labels_build_then_clear() {
        let mut rig = Rig::ready();
        rig.control.merge_labels(&[Label { key: "pawnio.b".into(), label: "B".into(), unit: "".into() }]);
        rig.control.merge_labels(&[Label { key: "pawnio.a".into(), label: "A".into(), unit: "°C".into() }]);
        let labels = rig.control.labels();
        assert_eq!(labels.iter().map(|l| l.key.as_str()).collect::<Vec<_>>(), ["pawnio.a", "pawnio.b"]);
        rig.control.set_wanted(false);
        assert!(rig.control.labels().is_empty());
    }

    /// Turned off and on while the prompt is up, then asked: no second prompt while the first is up (§4.3,
    /// S-MEDIUM-7). The first prompt's Yes still counts, as PawnIO is on again by then.
    #[test]
    fn off_and_on_while_asking_raises_no_second_prompt() {
        let mut rig = Rig::ready();
        rig.installed_current();
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        rig.control.api_bound();
        let first = rig.control.take_launch().expect("the first prompt");
        // the prompt is up; PawnIO is turned off and on, and the studio asks, twice over
        for _ in 0..2 {
            rig.control.set_wanted(false);
            rig.control.set_wanted(true);
            rig.control.ask();
            assert!(rig.control.take_launch().is_none(), "a second prompt went up while the first was up");
            assert_eq!(rig.state(), "asking");
        }
        let fin = run_plan(&rig.env, &first);
        rig.control.finish_launch(&first, fin);
        assert_eq!(rig.state(), "ok");
        assert_eq!(rig.env.launches().len(), 1);
    }

    /// Declined, then turned off and on and asked within 10 s of that prompt: no new prompt until the 10 s
    /// are up (§4.3).
    #[test]
    fn off_and_on_keeps_the_ten_seconds() {
        let mut rig = Rig::ready();
        rig.installed_current();
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        rig.control.api_bound();
        rig.env.set(|s| s.launch_result = Some(Err(1223)));
        rig.drive();
        assert_eq!(rig.state(), "declined");
        rig.control.set_wanted(false);
        rig.control.set_wanted(true);
        rig.env.set(|s| s.now += Duration::from_secs(3));
        rig.control.ask();
        assert!(rig.control.take_launch().is_none(), "a prompt 3 s after the last");
        assert_eq!(rig.state(), "not-asked");
        rig.env.set(|s| s.now += Duration::from_secs(8));
        rig.control.ask();
        rig.drive();
        assert_eq!((rig.state().as_str(), rig.env.launches().len()), ("ok", 2));
    }

    /// A check that fails for a moment while the prompt is up doesn't stick: Check again finds the prompt
    /// still up, and raises no second one.
    #[test]
    fn check_again_finds_the_prompt_still_up() {
        let mut rig = Rig::ready();
        rig.installed_current();
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        rig.control.api_bound();
        let first = rig.control.take_launch().expect("the first prompt");
        rig.control.set_wanted(false);
        rig.env.set(|s| s.service = false);
        rig.control.set_wanted(true);
        assert_eq!(rig.state(), "not-running");
        rig.env.set(|s| s.service = true);
        rig.control.ask();
        assert!(rig.control.take_launch().is_none(), "a second prompt went up while the first was up");
        assert_eq!(rig.state(), "asking");
        let fin = run_plan(&rig.env, &first);
        rig.control.finish_launch(&first, fin);
        assert_eq!(rig.state(), "ok");
    }

    /// Re-applying the same setting (saving anything else) is no change, so the checks don't run again
    /// (§4.2): a reader keeps reading through a moment when PawnIO's driver restarts, and a failure keeps
    /// its reason.
    #[test]
    fn a_reapply_changes_nothing() {
        let mut rig = Rig::ready();
        rig.installed_current();
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        rig.control.api_bound();
        rig.drive();
        rig.env.set(|s| s.service = false);
        rig.control.set_wanted(true);
        assert_eq!(rig.state(), "ok");
        assert!(matches!(rig.control.read_target(), ReadTarget::Reader(_)));
        rig.env.set(|s| s.service = true);
        rig.end_reader(9);
        rig.control.set_wanted(true);
        assert_eq!(rig.state(), "failed");
        assert!(rig.message().starts_with("Another copy"), "{}", rig.message());
    }

    /// A second player whose first bind failed, and which gets the port only later (the first player gone),
    /// readies but doesn't ask by itself; an ask still asks.
    #[test]
    fn a_late_bind_never_asks() {
        let mut rig = Rig::ready();
        rig.installed_current();
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        rig.control.api_bind_failed();
        rig.control.api_bind_failed();
        rig.control.api_bound();
        assert_eq!((rig.state().as_str(), rig.message()), ("not-asked", MSG_ASK.to_string()));
        assert!(rig.control.take_launch().is_none());
        rig.control.ask();
        rig.drive();
        assert_eq!((rig.state().as_str(), rig.env.launches().len()), ("ok", 1));
        // a bind that fails after the first one worked changes nothing
        rig.control.api_bind_failed();
        assert_eq!(rig.state(), "ok");
    }

    /// A reader that exits before it opens its pipe (a failed install, its checks) fails with its exit
    /// code's sentence at once, not "didn't answer" (§4.3).
    #[test]
    fn an_early_exit_says_why() {
        for (code, mark) in [(9, "Another copy"), (10, "damaged or from another version"), (8, "without administrator rights"),
                             (3, "stopped (code 3)"), (5, "stopped (code 5)")] {
            let mut rig = Rig::ready();
            rig.env.set(|s| {
                s.build = Some(PathBuf::from(r"C:\build\wvsensors.exe"));
                s.files.push((PathBuf::from(r"C:\build\wvsensors.exe"), "built".into()));
                s.connect_result = Some(Err(ConnectError::Exited(code)));
            });
            rig.control.allow_elevation();
            rig.control.set_wanted(true);
            rig.control.api_bound();
            rig.control.ask();
            rig.drive();
            assert_eq!(rig.state(), "failed", "code {code}");
            assert!(rig.message().contains(mark), "code {code}: {}", rig.message());
            assert!(rig.control.take_watch().is_none(), "code {code}: a reader that's gone is watched");
            let logs = rig.control.take_logs();
            assert!(logs.iter().any(|l| l.ends_with(&format!("before it answered (code {code})"))), "code {code}: {logs:?}");
        }
    }

    /// A launch Windows refuses says Windows' own reason (§4.3), not a bare code.
    #[test]
    fn a_failed_start_says_windows_reason() {
        let mut rig = Rig::ready();
        rig.installed_current();
        rig.env.set(|s| s.launch_result = Some(Err(2)));
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        rig.control.api_bound();
        rig.drive();
        assert_eq!(rig.state(), "failed");
        let reason = crate::pawnio::elevation::system_message(2);
        assert_ne!(reason, "error 2");
        assert_eq!(rig.message(), format!("Couldn't start PawnIO's sensor reader: {reason}."));
    }

    /// One log line per change (§1.2, §3.9): the path a prompt is for, a decline, an exit with its code, and
    /// each of the reader's notes once.
    #[test]
    fn each_change_is_logged() {
        let mut rig = Rig::ready();
        rig.installed_current();
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        assert_eq!(rig.control.take_logs(), [format!("data: pawnio: not-asked: {MSG_ASK}")]);
        rig.control.api_bound();
        assert_eq!(rig.control.take_logs(),
                   [r"data: pawnio: asking for administrator rights (C:\Program Files\Wonder Vision\wvsensors.exe)"]);
        rig.drive();
        assert_eq!(rig.control.take_logs(), ["data: pawnio: ok"]);
        let gen_id = rig.control.reader_gen().unwrap();
        let read = || Ok(Readings { notes: vec!["a reader note".to_string()], ..Readings::default() });
        rig.control.read_done(gen_id, read());
        rig.control.read_done(gen_id, read());
        assert_eq!(rig.control.take_logs(), ["data: pawnio: a reader note"]);
        rig.end_reader(5);
        let stopped = describe(&rig.control.state).1;
        assert_eq!(rig.control.take_logs(), ["data: pawnio: the sensor reader exited (code 5)".to_string(),
                                             format!("data: pawnio: failed: {stopped}")]);
        rig.env.set(|s| {
            s.now += Duration::from_secs(20);
            s.launch_result = Some(Err(1223));
        });
        rig.control.ask();
        rig.drive();
        assert_eq!(rig.control.take_logs().last().map(String::as_str), Some("data: pawnio: declined"));
        rig.control.set_wanted(false);
        assert_eq!(rig.control.take_logs(), ["data: pawnio: off"]);
        rig.control.set_wanted(false);
        assert!(rig.control.take_logs().is_empty(), "no change, no line");
    }

    /// A read that finishes after PawnIO was turned off (or another reader started) changes nothing: its
    /// labels don't come back into the catalog, and its failure fails nothing.
    #[test]
    fn a_stale_read_changes_nothing() {
        let mut rig = Rig::ready();
        rig.installed_current();
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        rig.control.api_bound();
        rig.drive();
        let old = rig.control.reader_gen().unwrap();
        let label = Label { key: "pawnio.cpu.temperature.tctl".into(), label: "CPU Tctl".into(), unit: "°C".into() };
        let readings = Readings { labels: vec![label.clone()], ..Readings::default() };
        // turned off while the read was in flight
        rig.control.set_wanted(false);
        let late = rig.control.read_done(old, Ok(readings.clone()));
        assert!(rig.control.labels().is_empty(), "the labels came back after turning off");
        assert_eq!(late.notes, [NOT_ON_YET]);
        // on again with a new reader: the old reader's failure doesn't fail it
        rig.control.set_wanted(true);
        rig.env.set(|s| s.now += Duration::from_secs(20));
        rig.control.ask();
        rig.drive();
        assert_eq!(rig.state(), "ok");
        rig.control.read_done(old, Err("broken".into()));
        assert_eq!(rig.state(), "ok");
        assert!(matches!(rig.control.read_target(), ReadTarget::Reader(_)));
        // the current reader's read counts
        let now = rig.control.reader_gen().unwrap();
        assert_eq!(rig.control.read_done(now, Ok(readings)).labels, [label.clone()]);
        assert_eq!(rig.control.labels(), [label]);
        assert_eq!(rig.control.read_done(now, Err("broken".into())).notes, [DIDNT_ANSWER]);
        assert_eq!(rig.state(), "failed");
    }

    /// The second player, which never gets the API's port, never asks: the start prompt waits for the bind.
    #[test]
    fn a_second_player_never_asks() {
        let mut rig = Rig::ready();
        rig.installed_current();
        rig.control.allow_elevation();
        rig.control.set_wanted(true);
        // the API never binds (another player has the port)
        assert_eq!(rig.state(), "not-asked");
        assert!(rig.control.take_launch().is_none());
        assert!(rig.env.launches().is_empty());
    }
}
