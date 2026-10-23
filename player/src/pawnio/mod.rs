//! PawnIO sensors for the pc source (docs/data.md): CPU temperatures and power, fan and pump speeds and
//! motherboard temperatures, read straight from the hardware through the PawnIO driver (pawnio.eu),
//! which needs administrator rights. Only `wvplay play` ever asks for them.
//!
//! This file is all the rest of the player sees of it; the files below do the work:
//! - driver.rs, blobs.rs, sha256.rs, mutex.rs: the driver, the pinned module files beside the player, and
//!   the locks shared with other monitoring programs;
//! - amd.rs, ite.rs, keys.rs, labels.rs, reader.rs (fake.rs in tests): reading the CPU and the
//!   motherboard's sensor chips, and naming what was read;
//! - probe.rs: `wvplay pawnio`, which prints every reading, to check against Libre Hardware Monitor.
//!
//! Not finished in this build: `read` only says so, and the state stays off.

use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

pub mod amd;
pub mod blobs;
pub mod driver;
#[cfg(test)]
pub mod fake;
pub mod ite;
pub mod keys;
pub mod labels;
pub mod mutex;
pub mod probe;
pub mod reader;
pub mod sha256;

/// What `read` says while this build can't read PawnIO.
const UNFINISHED: &str = "PawnIO support isn't finished in this build.";

/// A key's name for people, which the studio shows beside it, and its unit: °C, RPM or W.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Label {
    pub key: String,
    pub label: String,
    pub unit: String,
}

/// One read: numbers by key, labels for those keys, the CPU temperature for `pc.cpu.temp`, and notes
/// (one sentence each, naming the fix). It serializes as it is, so it can cross from one process to another.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Readings {
    pub values: Vec<(String, f64)>,
    pub labels: Vec<Label>,
    pub cpu_temp: Option<f64>,
    pub notes: Vec<String>,
}

/// What the driver answered one EXECUTE with, by the Win32 code it gave at that stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoError {
    /// 5: the module's allow-list refused that MSR or port.
    AccessDenied,
    /// 21: LpcIO was used before `ioctl_select_slot`.
    NotReady,
    /// 2: the module has no such `ioctl_` function (another version).
    NoSuchFunction,
    /// 87: the wrong number of cells, a bad name, or no module loaded.
    BadArguments,
    /// 31: the module faulted.
    Fault,
    Other(u32),
}

/// One loaded module on a device handle of its own (a handle holds one module). `input` and `out` have
/// exactly the cells the function documents (the module sources, player/pawnio/src): the driver refuses
/// other counts.
pub trait Exec: Send {
    fn execute(&self, function: &str, input: &[u64], out: &mut [u64]) -> Result<(), IoError>;
}

/// Why a module couldn't be opened: at the device's open, at its LOAD, or before either.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenError {
    /// 2 at the open: no PawnIO device (not installed, or its driver isn't running).
    NotInstalled,
    /// 5 at the open: this process isn't running as administrator.
    AccessDenied,
    /// 0x80090006 at the LOAD: the file isn't signed by PawnIO's key.
    BadSignature,
    /// 50 at the LOAD: the module refused this machine (another CPU vendor or family).
    NotSupported,
    /// 31 at the LOAD: the module needs something this driver lacks (the driver is too old), or it faulted.
    TooOld,
    /// 87 at the LOAD: the file is too short to be a module.
    Malformed,
    /// Before the LOAD: the module's file is missing, or isn't the one pinned (its SHA-256 differs).
    Damaged,
    Other(u32),
}

/// The modules this player loads, each from its own pinned `.bin` file beside the player.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModuleId {
    /// AMD family 17h to 1Ah: temperatures and package power.
    AmdFamily17,
    /// The Super I/O chips' ports.
    LpcIo,
}

impl ModuleId {
    /// Its name, for notes.
    pub fn name(self) -> &'static str {
        match self {
            ModuleId::AmdFamily17 => "AMDFamily17",
            ModuleId::LpcIo => "LpcIO",
        }
    }

    /// Its file in the module folder.
    pub fn file(self) -> &'static str {
        match self {
            ModuleId::AmdFamily17 => "AMDFamily17.bin",
            ModuleId::LpcIo => "LpcIO.bin",
        }
    }
}

/// How a reader opens a module, on a device handle of its own each time it's called: from the pinned file
/// in the module folder for real (blobs.rs, driver.rs), a fake in tests (fake.rs).
pub type OpenModule = Box<dyn Fn(ModuleId) -> Result<Box<dyn Exec>, OpenError> + Send>;

/// A `Lock` while it's held: dropping it lets go. It isn't Send, as a Windows mutex is let go by the thread
/// that took it.
pub struct Held<'a> {
    release: Option<Box<dyn FnOnce() + 'a>>,
}

impl<'a> Held<'a> {
    /// Held until dropped, which runs `release`.
    pub fn new(release: impl FnOnce() + 'a) -> Held<'a> {
        Held { release: Some(Box::new(release)) }
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// A lock shared with other programs that read the same hardware (the named mutexes in mutex.rs; a fake in
/// tests): None when it's still taken after `timeout_ms`.
pub trait Lock: Send {
    fn acquire(&self, timeout_ms: u32) -> Option<Held<'_>>;
}

/// The sensors, for the pc poll and /test. Never asks for administrator rights. When PawnIO can't be
/// read, a note says why.
pub fn read() -> Readings {
    called("read");
    Readings { notes: vec![UNFINISHED.to_string()], ..Readings::default() }
}

/// Whether an enabled pc source has `pawnio` on. data.rs says so first in every `apply`, before any
/// source starts, so the pc source's first poll already sees it. Turning it on this way never asks for
/// administrator rights by itself; turning it off stops reading at once.
pub fn set_wanted(on: bool) {
    called(&format!("set_wanted({on})"));
}

/// This process is `wvplay play`, the one command that may ask for administrator rights: player.rs says
/// so before `data::start`.
pub fn allow_elevation() {
    called("allow_elevation");
}

/// The API has its port (api.rs, after each bind; only the first counts). A player that never gets it,
/// like a second one, never asks for administrator rights.
pub fn api_bound() {
    called("api_bound");
}

/// What to call after every change of state, with no lock held: data.rs has the pc source poll at once
/// (`Hub::poll_now`), so the readings and the note follow.
pub fn set_notify(f: Box<dyn Fn() + Send + Sync>) {
    called("set_notify");
    drop(f);
}

/// The studio's Ask now, Ask again or Check again (POST /pawnio/ask): asks or checks again when the state
/// allows it, and gives the state after, as `info` does.
pub fn ask() -> Json {
    called("ask");
    off()
}

/// The state for /info's `pawnio`: `{"state", "message", "can_ask"}` (docs/data.md).
pub fn info() -> Json {
    called("info");
    off()
}

/// Whether this process runs as administrator, for /info's `elevated`.
pub fn is_elevated() -> bool {
    called("is_elevated");
    false
}

/// A label for every key read since PawnIO was last turned on, sorted by key: the pc manifest's live rows
/// (`Hub::catalog`).
pub fn labels() -> Vec<Label> {
    called("labels");
    Vec::new()
}

fn off() -> Json {
    json!({ "state": "off", "message": "", "can_ask": false })
}

/// Notes a call, for tests (`calls`); nothing outside them.
fn called(what: &str) {
    #[cfg(test)]
    calls::record(what);
    #[cfg(not(test))]
    let _ = what;
}

/// Tests: the calls made into this module on the test's own thread, in order, so a test can check when
/// the player makes them. `watch` adds what was true at each call.
#[cfg(test)]
pub mod calls {
    use std::cell::RefCell;

    thread_local! {
        static SEEN: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
        static WATCH: RefCell<Option<Box<dyn Fn() -> String>>> = const { RefCell::new(None) };
    }

    pub(super) fn record(call: &str) {
        let noted = WATCH.with_borrow(|watch| watch.as_ref().map(|w| w()));
        let line = noted.map_or_else(|| call.to_string(), |noted| format!("{call}: {noted}"));
        SEEN.with_borrow_mut(|seen| seen.push(line));
    }

    /// From now on, each call on this thread is noted with what `f` says.
    pub fn watch(f: impl Fn() -> String + 'static) {
        WATCH.set(Some(Box::new(f)));
    }

    /// The calls on this thread so far, which are then forgotten.
    pub fn take() -> Vec<String> {
        SEEN.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Until the reader is in, PawnIO is off, and reading only says so.
    #[test]
    fn unfinished() {
        let read = read();
        assert_eq!(read.notes, [UNFINISHED]);
        assert!(read.notes[0].starts_with("PawnIO"));
        assert!(read.values.is_empty() && read.labels.is_empty() && read.cpu_temp.is_none());
        for state in [info(), ask()] {
            assert_eq!(state, json!({ "state": "off", "message": "", "can_ask": false }));
        }
        assert!(!is_elevated() && labels().is_empty());
        set_wanted(true);
        allow_elevation();
        set_notify(Box::new(|| {}));
        set_wanted(false);
        assert_eq!(calls::take(), ["read", "info", "ask", "is_elevated", "labels", "set_wanted(true)", "allow_elevation",
                                   "set_notify", "set_wanted(false)"]);
        assert!(calls::take().is_empty());
    }

    /// Readings cross processes as JSON and come back the same.
    #[test]
    fn readings_round_trip() {
        let r = Readings { values: vec![("pawnio.cpu.temperature.tctl".into(), 70.375)],
                           labels: vec![Label { key: "pawnio.cpu.temperature.tctl".into(), label: "CPU Tctl".into(), unit: "°C".into() }],
                           cpu_temp: Some(70.375), notes: vec!["a note".into()] };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Readings>(&json).unwrap(), r);
    }

    /// A held lock lets go once, when it's dropped.
    #[test]
    fn held_lets_go_on_drop() {
        let let_go = std::cell::Cell::new(0);
        let held = Held::new(|| let_go.set(let_go.get() + 1));
        assert_eq!(let_go.get(), 0);
        drop(held);
        assert_eq!(let_go.get(), 1);
    }

    /// Each module is its own file in the module folder.
    #[test]
    fn module_files() {
        assert_eq!((ModuleId::AmdFamily17.name(), ModuleId::AmdFamily17.file()), ("AMDFamily17", "AMDFamily17.bin"));
        assert_eq!((ModuleId::LpcIo.name(), ModuleId::LpcIo.file()), ("LpcIO", "LpcIO.bin"));
    }

    /// The two clocks the reader tells a sleep by (reader.rs) are in windows-sys's
    /// Win32_System_WindowsProgramming: time asleep counts in QueryInterruptTime, not in
    /// QueryUnbiasedInterruptTime.
    #[test]
    fn interrupt_times() {
        use windows_sys::Win32::System::WindowsProgramming::{QueryInterruptTime, QueryUnbiasedInterruptTime};
        let (mut awake, mut since_boot) = (0u64, 0u64);
        unsafe {
            QueryUnbiasedInterruptTime(&mut awake);
            QueryInterruptTime(&mut since_boot);
        }
        assert!(awake > 0 && since_boot >= awake, "{since_boot} {awake}");
    }
}
