//! The reader: opens the modules, reads the CPU and then the chips, and makes every failure a note, so the
//! rest still comes through. Every hardware sequence holds one lock in the process, so whoever ends the
//! process lets the sequence finish, and put back what it changed, first.
//!
//! It runs only where PawnIO can be opened: in the elevated sensor reader, and in `wvplay pawnio` (the probe).
//! Everything it touches is handed to it (the module opener, the shared locks, the CPU, the board, the
//! clock), so tests run it on fakes (fake.rs).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::amd::{Amd, Cpuid};
use super::ite::SuperIo;
use super::keys::{Cpu, Sensor};
use super::labels::{Board, Names};
use super::{IoError, Label, Lock, ModuleId, OpenError, OpenModule, Readings};

/// A gap between the clock that counts sleep and the one that doesn't, longer than this, means the PC slept.
const SLEPT: Duration = Duration::from_secs(5);

/// The two clocks a reader goes by: time since Windows started, with and without the time asleep, so it can
/// tell when the PC slept. And waiting, which detection does for a millisecond.
pub trait Clock: Send {
    /// Time since Windows started, counting time asleep (QueryInterruptTime).
    fn since_boot(&self) -> Duration;
    /// Time since Windows started, not counting time asleep (QueryUnbiasedInterruptTime).
    fn awake(&self) -> Duration;
    fn sleep(&self, time: Duration);
}

/// Windows' clocks.
pub struct SystemClock;

impl Clock for SystemClock {
    fn since_boot(&self) -> Duration {
        let mut ticks = 0u64;
        unsafe { windows_sys::Win32::System::WindowsProgramming::QueryInterruptTime(&mut ticks) };
        Duration::from_nanos(ticks.saturating_mul(100))
    }

    fn awake(&self) -> Duration {
        let mut ticks = 0u64;
        unsafe { windows_sys::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTime(&mut ticks) };
        Duration::from_nanos(ticks.saturating_mul(100))
    }

    fn sleep(&self, time: Duration) {
        std::thread::sleep(time);
    }
}

/// The lock in the process around every hardware sequence (the CPU's SMN reads, one chip detection, one
/// chip's poll), and the flag that asks them to stop. Whoever ends the process calls `stop` and exits while
/// holding what it returns: the sequence running then has finished by that time, and has put the chip back
/// as it found it (ite.rs). A process has one, shared by its reader and whatever ends the process.
#[derive(Default)]
pub struct HwLock {
    stopping: AtomicBool,
    lock: Mutex<()>,
}

impl HwLock {
    /// Held around one hardware sequence. Inside the shared lock (`Lock`) for the same hardware, never
    /// around it.
    pub fn hold(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the process is ending: a sequence checks between its steps, and returns once it has put
    /// back what it changed.
    pub fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Asks the sequences to stop, without waiting.
    pub fn ask_to_stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    /// Asks the sequences to stop, and waits until the one running has finished: exit while holding this.
    pub fn stop(&self) -> MutexGuard<'_, ()> {
        self.ask_to_stop();
        self.hold()
    }
}

/// The locks shared with other programs that read the same hardware (mutex.rs): `pci` around the CPU's SMN
/// reads, `isa` around the Super I/O chips. None when it could be neither made nor opened: that part is
/// then not read at all, rather than read without it.
pub struct Locks {
    pub pci: Option<Box<dyn Lock>>,
    pub isa: Option<Box<dyn Lock>>,
}

/// What every part of one read goes by: the process's hardware lock, the clock, when the read began, and
/// when the PC was last found to have woken from sleep (None if never, since the reader started).
pub struct Poll<'a> {
    pub hw: &'a HwLock,
    pub clock: &'a dyn Clock,
    pub now: Duration,
    pub resumed_at: Option<Duration>,
}

/// Notes the CPU and the chips share.
pub(crate) const DAMAGED: &str =
    "The PawnIO module files beside the player are damaged or from another version: rebuild or reinstall the player.";
pub(crate) const TOO_OLD: &str = "PawnIO is too old for its modules: update it from pawnio.eu.";
pub(crate) const INTEL: &str = "PawnIO CPU readings aren't supported on Intel CPUs yet.";
pub(crate) const OTHER_CPU: &str = "PawnIO CPU readings are only for AMD Ryzen CPUs (family 17h and later) so far.";

/// What a reading part is called in a note: `what` is "CPU readings" or "fan or board readings".
pub(crate) fn lock_missing(what: &str) -> String {
    format!("Couldn't share the motherboard sensor lock with other monitoring programs, so {what} are off.")
}

/// The note for a module that couldn't be opened.
pub(crate) fn open_note(module: ModuleId, error: OpenError) -> String {
    let (what, machine) = match module {
        ModuleId::AmdFamily17 => ("CPU readings", "CPU"),
        ModuleId::LpcIo => ("fan or board readings", "PC"),
    };
    match error {
        OpenError::NotInstalled => {
            "PawnIO's driver isn't running: restart Windows (or reinstall PawnIO from pawnio.eu).".to_string()
        }
        OpenError::AccessDenied => "PawnIO needs administrator rights to read sensors.".to_string(),
        OpenError::BadSignature | OpenError::Malformed | OpenError::Damaged => DAMAGED.to_string(),
        OpenError::NotSupported => format!("PawnIO's {} module doesn't support this {machine}, so there are no {what}.", module.name()),
        OpenError::TooOld => TOO_OLD.to_string(),
        OpenError::Other(code) => format!("PawnIO couldn't load its {} module (error {code}), so there are no {what}.", module.name()),
    }
}

/// An I/O error in a note.
pub(crate) fn io_text(error: IoError) -> String {
    match error {
        IoError::AccessDenied => "access denied".to_string(),
        IoError::NotReady => "not ready".to_string(),
        IoError::NoSuchFunction => "no such function".to_string(),
        IoError::BadArguments => "bad arguments".to_string(),
        IoError::Fault => "the module faulted".to_string(),
        IoError::Other(code) => format!("error {code}"),
    }
}

/// Reads the CPU and the motherboard's sensor chips through PawnIO.
pub struct Reader {
    open: OpenModule,
    locks: Locks,
    names: Names,
    clock: Box<dyn Clock>,
    hw: Arc<HwLock>,
    /// The CPU, when it can be read; else why not.
    amd: Result<Amd, String>,
    superio: SuperIo,
    /// Both clocks at the last read, and when a sleep was last noticed.
    last: Option<(Duration, Duration)>,
    resumed_at: Option<Duration>,
}

impl Reader {
    /// Opens the CPU's module, if the CPU is one it reads. The Super I/O chips are found on the first read.
    pub fn open(open: OpenModule, locks: Locks, cpu: Cpuid, board: &Board, clock: Box<dyn Clock>, hw: Arc<HwLock>) -> Reader {
        let amd = if cpu.amd_zen() {
            open(ModuleId::AmdFamily17).map(|exec| Amd::new(exec, cpu)).map_err(|e| open_note(ModuleId::AmdFamily17, e))
        } else if cpu.vendor == "GenuineIntel" {
            Err(INTEL.to_string())
        } else {
            Err(OTHER_CPU.to_string())
        };
        Reader { open, locks, names: Names::for_board(board), clock, hw, amd, superio: SuperIo::default(), last: None,
                 resumed_at: None }
    }

    /// One read of everything: the CPU, then the chips. A part that can't be read is a note.
    pub fn read(&mut self) -> Readings {
        let (now, awake) = (self.clock.since_boot(), self.clock.awake());
        if let Some((boot0, awake0)) = self.last
            && now.saturating_sub(boot0).saturating_sub(awake.saturating_sub(awake0)) > SLEPT
        {
            self.resumed_at = Some(now);
        }
        self.last = Some((now, awake));
        let mut readings = Readings::default();
        if self.hw.stopping() {
            return readings;
        }
        let poll = Poll { hw: &self.hw, clock: &*self.clock, now, resumed_at: self.resumed_at };
        let mut read: Vec<(Sensor, f64)> = Vec::new();
        let mut notes = Vec::new();

        match &mut self.amd {
            Ok(amd) => {
                let mut cpu = Vec::new();
                amd.read(&poll, self.locks.pci.as_deref(), &mut cpu, &mut notes);
                read.extend(cpu.into_iter().map(|(c, v)| (Sensor::Cpu(c), v)));
            }
            Err(note) => notes.push(note.clone()),
        }
        self.superio.read(&poll, &self.open, self.locks.isa.as_deref(), &mut read, &mut notes);

        let value = |cpu: Cpu| read.iter().find(|(s, _)| *s == Sensor::Cpu(cpu)).map(|(_, v)| *v);
        readings.cpu_temp = value(Cpu::Tdie).or(value(Cpu::Tctl));
        for (sensor, v) in &read {
            let key = sensor.key();
            readings.labels.push(Label { key: key.clone(), label: self.names.label(sensor), unit: sensor.unit().to_string() });
            readings.values.push((key, *v));
        }
        for note in notes {
            if !readings.notes.contains(&note) {
                readings.notes.push(note);
            }
        }
        readings
    }

    /// The Super I/O chips, as found so far, for the probe.
    pub fn superio(&self) -> &SuperIo {
        &self.superio
    }

    /// The CPU's reader, when the CPU is read, for the probe.
    pub fn amd(&self) -> Option<&Amd> {
        self.amd.as_ref().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pawnio::fake::{self, FakeAmd, FakeClock, FakeLock, FakeLpc};

    const ELITE: (&str, &str) = ("Gigabyte Technology Co., Ltd.", "X870E AORUS ELITE X3D ICE");

    /// The parts a test reader is made of, kept so the test can change them.
    struct Rig {
        amd: FakeAmd,
        lpc: FakeLpc,
        pci: FakeLock,
        isa: FakeLock,
        clock: FakeClock,
        hw: Arc<HwLock>,
    }

    impl Rig {
        /// This PC: a Ryzen 9 9950X3D at 70.375 °C with CCDs at 49.625 and 60 °C, and its two ITE chips.
        fn new() -> Rig {
            let amd = FakeAmd::default();
            amd.set_smn(0x59800, 0x4660_0000);
            amd.set_smn(0x59B08, 0xB15);
            amd.set_smn(0x59B0C, 0x800 | 872);
            amd.set_msr(0xC001_0299, 0x0000_1000);
            Rig { amd, lpc: FakeLpc::this_pc(), pci: FakeLock::default(), isa: FakeLock::default(), clock: FakeClock::default(),
                  hw: Arc::default() }
        }

        fn reader(&self, cpu: Cpuid, board: (&str, &str)) -> Reader {
            let locks = Locks { pci: Some(Box::new(self.pci.clone())), isa: Some(Box::new(self.isa.clone())) };
            let board = Board { manufacturer: board.0.to_string(), product: board.1.to_string() };
            Reader::open(fake::opener(Ok(self.amd.clone()), Ok(self.lpc.clone())), locks, cpu, &board,
                         Box::new(self.clock.clone()), self.hw.clone())
        }
    }

    fn ryzen() -> Cpuid {
        Cpuid { vendor: "AuthenticAMD".into(), family: 0x1A, model: 0x44, brand: "AMD Ryzen 9 9950X3D 16-Core Processor".into() }
    }

    fn value(readings: &Readings, key: &str) -> Option<f64> {
        readings.values.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
    }

    fn keys(readings: &Readings) -> Vec<&str> {
        readings.values.iter().map(|(k, _)| k.as_str()).collect()
    }

    /// This PC as the spec expects it: the CPU's keys, then each chip's, with generic labels, as LHM has no
    /// map for this board yet. Power comes from the second read on.
    #[test]
    fn this_pc_reads() {
        let rig = Rig::new();
        let mut reader = rig.reader(ryzen(), ELITE);
        let first = reader.read();
        assert_eq!(first.notes, Vec::<String>::new());
        assert_eq!(keys(&first), [
            "pawnio.cpu.temperature.tctl", "pawnio.cpu.temperature.ccd1", "pawnio.cpu.temperature.ccd2",
            "pawnio.cpu.temperature.ccd_max", "pawnio.cpu.temperature.ccd_average",
            "pawnio.ite_it8696e.temperature.temperature_1", "pawnio.ite_it8696e.temperature.temperature_2",
            "pawnio.ite_it8696e.temperature.temperature_3", "pawnio.ite_it8696e.temperature.temperature_4",
            "pawnio.ite_it8696e.temperature.temperature_5", "pawnio.ite_it8696e.fan.fan_1", "pawnio.ite_it8696e.fan.fan_2",
            "pawnio.ite_it8696e.fan.fan_3", "pawnio.ite_it8696e.fan.fan_4", "pawnio.ite_it8696e.fan.fan_5",
            "pawnio.ite_it87952e.temperature.temperature_1", "pawnio.ite_it87952e.temperature.temperature_3",
            "pawnio.ite_it87952e.fan.fan_1", "pawnio.ite_it87952e.fan.fan_2", "pawnio.ite_it87952e.fan.fan_3",
        ]);
        assert_eq!(first.cpu_temp, Some(70.375));
        assert_eq!(value(&first, "pawnio.cpu.temperature.ccd2"), Some(60.0));
        assert_eq!(value(&first, "pawnio.cpu.temperature.ccd_average"), Some((49.625 + 60.0) / 2.0));
        assert_eq!(value(&first, "pawnio.ite_it8696e.temperature.temperature_3"), Some(70.0));
        assert_eq!(value(&first, "pawnio.ite_it8696e.fan.fan_5"), Some(0.0));
        assert!((value(&first, "pawnio.ite_it87952e.fan.fan_2").unwrap() - 6308.41).abs() < 0.01);
        assert_eq!(first.labels.len(), first.values.len());
        let label = |key: &str| first.labels.iter().find(|l| l.key == key).map(|l| (l.label.as_str(), l.unit.as_str()));
        assert_eq!(label("pawnio.ite_it87952e.fan.fan_2"), Some(("ITE IT87952E fan 2", "RPM")));
        assert_eq!(label("pawnio.cpu.temperature.tctl"), Some(("CPU Tctl", "°C")));

        // 1 s after the first read (which took 2 ms, waiting while it found the chips): 8 519 680 counts
        rig.clock.advance(Duration::from_secs(1) - rig.clock.since_boot());
        rig.amd.set_msr(0xC001_029B, 8_519_680);
        let second = reader.read();
        assert_eq!(value(&second, "pawnio.cpu.power.package"), Some(130.0));
        let label = second.labels.iter().find(|l| l.key == "pawnio.cpu.power.package").unwrap();
        assert_eq!((label.label.as_str(), label.unit.as_str()), ("CPU package power", "W"));
    }

    /// A board with a map gets its names.
    #[test]
    fn mapped_board_names() {
        let rig = Rig::new();
        let read = rig.reader(ryzen(), ("Gigabyte Technology Co., Ltd.", "X870E AORUS PRO")).read();
        let label = |key: &str| read.labels.iter().find(|l| l.key == key).unwrap().label.clone();
        assert_eq!(label("pawnio.ite_it87952e.fan.fan_2"), "System Fan #6 / Pump");
        assert_eq!(label("pawnio.ite_it8696e.temperature.temperature_3"), "CPU");
        assert_eq!(label("pawnio.cpu.temperature.ccd2"), "CPU CCD2");
    }

    /// An Intel CPU gets a note, and the chips still come through, with no CPU module opened.
    #[test]
    fn intel_still_reads_chips() {
        let rig = Rig::new();
        let intel = Cpuid { vendor: "GenuineIntel".into(), family: 6, model: 0xB7, brand: "Intel Core i9".into() };
        let read = rig.reader(intel, ELITE).read();
        assert_eq!(read.notes, [INTEL]);
        assert_eq!(read.cpu_temp, None);
        assert!(read.values.iter().all(|(k, _)| k.starts_with("pawnio.ite_")) && read.values.len() == 15);
        assert!(rig.amd.calls().is_empty());
        let other = Cpuid { vendor: "AuthenticAMD".into(), family: 0x15, model: 0x02, brand: "AMD FX-8350".into() };
        assert_eq!(Rig::new().reader(other, ELITE).read().notes, [OTHER_CPU]);
    }

    /// A CPU module that can't be opened is a note, and the chips still come through.
    #[test]
    fn module_errors_are_notes() {
        for (error, note) in [
            (OpenError::NotSupported, "PawnIO's AMDFamily17 module doesn't support this CPU, so there are no CPU readings."),
            (OpenError::TooOld, TOO_OLD),
            (OpenError::BadSignature, DAMAGED),
            (OpenError::Damaged, DAMAGED),
        ] {
            let rig = Rig::new();
            let locks = Locks { pci: Some(Box::new(rig.pci.clone())), isa: Some(Box::new(rig.isa.clone())) };
            let mut reader = Reader::open(fake::opener(Err(error), Ok(rig.lpc.clone())), locks, ryzen(), &Board::default(),
                                          Box::new(rig.clock.clone()), rig.hw.clone());
            let read = reader.read();
            assert_eq!(read.notes, [note]);
            assert_eq!(read.values.len(), 15);
            // and again at every read, as long as it lasts
            assert_eq!(reader.read().notes, [note]);
        }
        // the same for the chips' module, with the CPU still read
        let rig = Rig::new();
        let locks = Locks { pci: Some(Box::new(rig.pci.clone())), isa: Some(Box::new(rig.isa.clone())) };
        let mut reader = Reader::open(fake::opener(Ok(rig.amd.clone()), Err(OpenError::NotSupported)), locks, ryzen(),
                                      &Board::default(), Box::new(rig.clock.clone()), rig.hw.clone());
        let read = reader.read();
        assert_eq!(read.notes, ["PawnIO's LpcIO module doesn't support this PC, so there are no fan or board readings."]);
        assert_eq!(read.cpu_temp, Some(70.375));
    }

    /// A lock that can't be shared with other programs turns off what it guards, with a note; the energy
    /// counter, which takes no lock, is still read.
    #[test]
    fn missing_locks_turn_parts_off() {
        let rig = Rig::new();
        let locks = Locks { pci: None, isa: None };
        let mut reader = Reader::open(fake::opener(Ok(rig.amd.clone()), Ok(rig.lpc.clone())), locks, ryzen(),
                                      &Board::default(), Box::new(rig.clock.clone()), rig.hw.clone());
        let read = reader.read();
        assert_eq!(read.notes, [lock_missing("CPU temperatures"), lock_missing("fan and board readings")]);
        assert!(read.values.is_empty());
        rig.clock.advance(Duration::from_secs(1));
        rig.amd.set_msr(0xC001_029B, 65_536);
        assert_eq!(value(&reader.read(), "pawnio.cpu.power.package"), Some(1.0));
        assert!(rig.lpc.calls().is_empty() && rig.amd.calls().iter().all(|(f, _)| f == "ioctl_read_msr"));
    }

    /// A sleep shows as the clock that counts it running ahead of the one that doesn't: the energy counter
    /// is then read afresh, not over the sleep.
    #[test]
    fn sleep_is_noticed() {
        let rig = Rig::new();
        let mut reader = rig.reader(ryzen(), ELITE);
        rig.amd.set_msr(0xC001_029B, 0);
        reader.read();
        assert_eq!(reader.resumed_at, None);
        // 4 s awake since the first read (which took 2 ms, finding the chips), then 4 s asleep: no sleep yet
        rig.clock.advance(Duration::from_secs(4) - rig.clock.since_boot());
        rig.clock.suspend(Duration::from_secs(4));
        rig.amd.set_msr(0xC001_029B, 65_536);
        assert_eq!(value(&reader.read(), "pawnio.cpu.power.package"), Some(65_536.0 / 65_536.0 / 8.0));
        assert_eq!(reader.resumed_at, None);
        rig.clock.advance(Duration::from_secs(1));
        rig.clock.suspend(Duration::from_secs(3600));
        rig.amd.set_msr(0xC001_029B, 2 * 65_536);
        let read = reader.read();
        assert_eq!(value(&read, "pawnio.cpu.power.package"), None);
        assert_eq!(read.notes, Vec::<String>::new());
        assert_eq!(reader.resumed_at, Some(rig.clock.since_boot()));
        rig.clock.advance(Duration::from_secs(2));
        rig.amd.set_msr(0xC001_029B, 4 * 65_536);
        assert_eq!(value(&reader.read(), "pawnio.cpu.power.package"), Some(1.0));
    }

    /// The notes come once each, though both parts give the same one.
    #[test]
    fn notes_come_once() {
        let rig = Rig::new();
        let locks = Locks { pci: Some(Box::new(rig.pci.clone())), isa: Some(Box::new(rig.isa.clone())) };
        let mut reader = Reader::open(fake::opener(Err(OpenError::Damaged), Err(OpenError::Damaged)), locks, ryzen(),
                                      &Board::default(), Box::new(rig.clock.clone()), rig.hw.clone());
        assert_eq!(reader.read().notes, [DAMAGED]);
    }

    /// Once the process is ending, a read reads nothing.
    #[test]
    fn nothing_once_stopping() {
        let rig = Rig::new();
        let mut reader = rig.reader(ryzen(), ELITE);
        rig.hw.ask_to_stop();
        assert_eq!(reader.read(), Readings::default());
        assert!(rig.lpc.calls().is_empty() && rig.amd.calls().is_empty());
    }

    /// `stop` waits for the sequence that holds the lock, and every later one sees it's stopping.
    #[test]
    fn stop_waits_for_the_sequence() {
        let hw = Arc::new(HwLock::default());
        let held = hw.hold();
        let (sent, got) = std::sync::mpsc::channel();
        let stopper = hw.clone();
        let thread = std::thread::spawn(move || {
            let _held = stopper.stop();
            sent.send(()).unwrap();
        });
        while !hw.stopping() {
            std::thread::yield_now();
        }
        assert!(got.recv_timeout(Duration::from_millis(100)).is_err());
        drop(held);
        got.recv_timeout(Duration::from_secs(5)).unwrap();
        thread.join().unwrap();
    }

    /// A reader can be handed to the thread that serves it, and its hardware lock shared with the one that
    /// ends the process.
    #[test]
    fn reader_moves_between_threads() {
        fn send<T: Send>() {}
        fn shared<T: Send + Sync>() {}
        send::<Reader>();
        shared::<HwLock>();
    }

    /// The system clocks: asleep counts in one, not the other.
    #[test]
    fn system_clock() {
        let clock = SystemClock;
        let (boot, awake) = (clock.since_boot(), clock.awake());
        assert!(boot >= awake && awake > Duration::ZERO);
        clock.sleep(Duration::from_millis(2));
        assert!(clock.since_boot() >= boot + Duration::from_millis(1));
    }

    /// Every note names what's off and, where there's one, the fix.
    #[test]
    fn open_notes() {
        assert_eq!(open_note(ModuleId::LpcIo, OpenError::Other(1450)),
                   "PawnIO couldn't load its LpcIO module (error 1450), so there are no fan or board readings.");
        assert_eq!(open_note(ModuleId::AmdFamily17, OpenError::Malformed), DAMAGED);
        assert!(open_note(ModuleId::AmdFamily17, OpenError::NotInstalled).contains("pawnio.eu"));
        assert_eq!(io_text(IoError::Other(1168)), "error 1168");
    }
}
