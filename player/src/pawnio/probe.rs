//! `wvplay pawnio`: every raw and decoded reading as JSON, beside Libre Hardware Monitor's, for checking
//! what a board gives. Run in an administrator terminal; it never asks for the rights itself, and without
//! them it prints `{"error":"needs-admin"}` and exits with 1.
//!
//! It reads through the sensor reader's own `Reader` (reader.rs), so `keys` and `labels` are what the pc
//! source would publish, and after each read it dumps, read-only, what the reader decodes from: Tctl and
//! every CCD slot, the energy counter, and each chip's registers. Beyond the reader's writes (spec, Appendix
//! A), it writes only the index of each register it dumps, and the configuration index for the `after`
//! check, which reads each configuration port without a key, to show the chips were left as found. It's the
//! only program that reads PawnIO in its own process.
//!
//! `--stage cpu` (the default) reads the CPU only, and never touches the motherboard's chips or their lock.
//! Ctrl+C lets the hardware sequence running finish, putting back what it changed; the probe then writes
//! what it has and exits with 130.
//!
//! In the JSON, registers and IDs are hex text ("0x8696"); a chip register whose index didn't read back
//! (another program's index got in between) is null.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Map, Value as Json, json};

use super::amd::{self, Cpuid};
use super::driver::Device;
use super::elevation;
use super::ite::{self, Attempt, CHIPS, Chip, ChipSpec, Counts, PORTS};
use super::keys::hardware;
use super::labels::{Board, Names, win};
use super::mutex::{self, Bus};
use super::reader::{self, Clock, HwLock, Locks, Reader, SystemClock, io_text};
use super::{Exec, Held, IoError, Lock, ModuleId, OpenError, OpenModule, Readings, blobs, sha256};
use crate::sources::{lhm_name, lhm_sensors};

/// How it's run.
const USAGE: &str = "wvplay pawnio [--stage cpu|all] [--samples N] [--every MS] [--compare-lhm URL] [--out FILE]";

/// Samples by default: a few for the CPU; a minute's worth with the chips, to count how often another
/// program gets in the way.
const CPU_SAMPLES: usize = 3;
const ALL_SAMPLES: usize = 60;
const EVERY: Duration = Duration::from_millis(1000);
/// The most samples, and the least time between them, for the CPU and with the chips: each sample with the
/// chips is a few hundred port reads and writes, which other programs on the same bus wait for.
const MOST_SAMPLES: u64 = 600;
const LEAST_CPU: Duration = Duration::from_millis(500);
const LEAST_ALL: Duration = Duration::from_millis(1000);
/// How long a wait between samples goes before it looks for Ctrl+C again.
const STEP: Duration = Duration::from_millis(50);

/// The exit code after Ctrl+C, and how long Ctrl+C waits for the probe to write what it has before it ends
/// the process itself.
const STOPPED: i32 = 130;
const WRITE_WAIT: Duration = Duration::from_secs(5);

/// The registers dumped beside the reader's (spec §6.4): the voltages, and each PWM channel's control and
/// duty registers, of which a chip has its own count (`ChipSpec::pwms`).
const VOLTAGES: [u8; 10] = [0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x2F];
const PWM_CONTROL: [u8; 6] = [0x15, 0x16, 0x17, 0x7F, 0xA7, 0xAF];
const PWM_DUTY: [u8; 6] = [0x63, 0x6B, 0x73, 0x7B, 0xA3, 0xAB];

/// Where Windows keeps the board's and the BIOS's names (anyone may read them), and the ones shown.
const BIOS_KEY: &str = r"HARDWARE\DESCRIPTION\System\BIOS";
const BIOS: [&str; 9] = ["BaseBoardManufacturer", "BaseBoardProduct", "BaseBoardVersion", "BIOSVendor", "BIOSVersion",
                         "BIOSReleaseDate", "SystemManufacturer", "SystemProductName", "SystemFamily"];

/// Libre Hardware Monitor: how long it's waited for, and the most of its reply read.
const LHM_TIMEOUT: Duration = Duration::from_secs(5);
const LHM_LIMIT: u64 = 4 << 20;

/// What the probe is doing, for the log of lock waits: the reader's read, its own dumps, the `after` check.
const READ: &str = "read";
const DUMP: &str = "dump";
const AFTER: &str = "after";

/// `wvplay pawnio <args>`, writing to `out` (the console): its exit code.
pub fn main(args: &[String], out: &mut dyn Write) -> i32 {
    let code = main_as(args, out, elevation::is_elevated());
    let _ = out.flush();
    code
}

/// `main`, run as administrator or not.
fn main_as(args: &[String], out: &mut dyn Write, elevated: bool) -> i32 {
    let options = match Options::parse(args) {
        Ok(options) => options,
        Err(e) => return error(out, &e),
    };
    if let Some(machine) = fake(&options) {
        return probe(&options, machine, Arc::default(), out);
    }
    if !elevated {
        return error(out, "needs-admin");
    }
    let hw = Arc::new(HwLock::default());
    on_ctrl_c(hw.clone());
    probe(&options, Machine::this_pc(), hw, out)
}

/// `{"error": "<what>"}` on one line, and the exit code for it.
fn error(out: &mut dyn Write, what: &str) -> i32 {
    let _ = writeln!(out, "{}", json!({ "error": what }));
    1
}

/// What `wvplay pawnio` is asked for.
#[derive(Debug, PartialEq)]
struct Options {
    /// Also find and read the motherboard's sensor chips (`--stage all`), not just the CPU.
    all: bool,
    /// How many samples, how far apart: brought within `MOST_SAMPLES` and `LEAST_CPU`/`LEAST_ALL`.
    samples: usize,
    every: Duration,
    /// Libre Hardware Monitor's address, to compare with.
    lhm: Option<String>,
    /// Where the JSON goes, instead of the console.
    out: Option<PathBuf>,
    /// Tests: the hardware is fake.rs's, so no rights are needed.
    #[cfg(test)]
    fake: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Options, String> {
        let (mut all, mut samples, mut every, mut lhm, mut out) = (false, None, EVERY, None, None);
        #[cfg(test)]
        let mut fake = false;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value: {USAGE}"));
            let number = |text: &String| text.parse::<u64>().map_err(|_| format!("{arg} needs a number, not {text}"));
            match arg.as_str() {
                "--stage" => {
                    all = match value()?.as_str() {
                        "cpu" => false,
                        "all" => true,
                        other => return Err(format!("--stage is cpu or all, not {other}")),
                    }
                }
                "--samples" => samples = Some(number(value()?)?.clamp(1, MOST_SAMPLES) as usize),
                "--every" => every = Duration::from_millis(number(value()?)?),
                "--compare-lhm" => lhm = Some(value()?.clone()),
                "--out" => out = Some(PathBuf::from(value()?)),
                #[cfg(test)]
                "--fake" => fake = true,
                other => return Err(format!("{other} isn't an option: {USAGE}")),
            }
        }
        let samples = samples.unwrap_or(if all { ALL_SAMPLES } else { CPU_SAMPLES });
        let every = every.max(if all { LEAST_ALL } else { LEAST_CPU });
        Ok(Options {
            all,
            samples,
            every,
            lhm,
            out,
            #[cfg(test)]
            fake,
        })
    }
}

/// Tests: with `--fake`, this PC on fakes (fake.rs).
#[cfg(test)]
fn fake(options: &Options) -> Option<Machine> {
    options.fake.then(tests::fake_machine)
}

#[cfg(not(test))]
fn fake(_: &Options) -> Option<Machine> {
    None
}

/// The process's hardware lock, for Ctrl+C.
static HW: OnceLock<Arc<HwLock>> = OnceLock::new();

/// From now on, Ctrl+C (or Ctrl+Break, or closing the console) stops the probe as `ctrl_c` does.
fn on_ctrl_c(hw: Arc<HwLock>) {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    if HW.set(hw).is_ok() {
        unsafe { SetConsoleCtrlHandler(Some(ctrl_c), 1) };
    }
}

/// Asks the hardware sequences to stop, and waits for the one running to finish, putting back what it
/// changed. It then lets go of the lock (where the spec, §3.8, has it held until the process ends), so the
/// probe can still write what it has: no sequence starts after this, as `HwLock::hold` gives nothing once
/// stopping, so none can be under way when the process ends. If the probe hasn't ended within `WRITE_WAIT`
/// (say it's waiting for LHM), this ends it.
unsafe extern "system" fn ctrl_c(_event: u32) -> windows_sys::core::BOOL {
    if let Some(hw) = HW.get() {
        drop(hw.stop());
    }
    std::thread::sleep(WRITE_WAIT);
    std::process::exit(STOPPED)
}

/// What the probe reads: this PC (`Machine::this_pc`), or fakes in tests.
struct Machine {
    /// The program, beside which the module folder is (blobs.rs).
    exe: PathBuf,
    /// The board's and the BIOS's names, as Windows keeps them.
    bios: Vec<(&'static str, Option<String>)>,
    cpu: Cpuid,
    /// PawnIO as installed, its driver's service, and the device (`driver_state`).
    pawnio: Json,
    /// Makes a module opener: the reader's, then the `after` check's.
    modules: Box<dyn Fn() -> OpenModule>,
    /// The lock shared with other programs around a bus, made here (true) or opened as another program
    /// made it; or the note saying why there's none. Each call is a handle of its own.
    lock: Box<dyn Fn(Bus) -> Result<(Box<dyn Lock>, bool), String>>,
    clock: Arc<dyn Clock + Sync>,
    /// Libre Hardware Monitor's data.json at an address.
    lhm: Box<dyn Fn(&str) -> Result<Json, String>>,
}

impl Machine {
    /// This PC: its program, registry, CPU, PawnIO, module files, shared mutexes, clocks, and LHM.
    fn this_pc() -> Machine {
        let exe = std::env::current_exe().unwrap_or_default();
        let modules = {
            let exe = exe.clone();
            Box::new(move || reader::modules(&exe))
        };
        let lock = |bus| {
            mutex::shared(bus).map(|lock| {
                let created = lock.created;
                (Box::new(lock) as Box<dyn Lock>, created)
            })
        };
        Machine {
            bios: BIOS.iter().map(|&name| (name, win::machine_string(BIOS_KEY, name))).collect(),
            cpu: Cpuid::current(),
            pawnio: driver_state(),
            modules,
            lock: Box::new(lock),
            clock: Arc::new(SystemClock),
            lhm: Box::new(fetch_lhm),
            exe,
        }
    }
}

/// PawnIO as installed (its uninstall entry's version), its driver's service, whether the device opens, and
/// the driver's version, which it gives with no module loaded.
fn driver_state() -> Json {
    let installed = elevation::installed_version();
    let (open, version) = match Device::open() {
        Ok(device) => {
            let version = device.version().map(|v| format!("{}.{}.{}", v >> 16, (v >> 8) & 0xFF, v & 0xFF));
            ("ok".to_string(), Some(version.unwrap_or_else(io_text)))
        }
        Err(e) => (format!("{e:?}"), None),
    };
    json!({ "installed": installed, "service": elevation::service_state("PawnIO"), "open": open, "driver_version": version })
}

/// Libre Hardware Monitor's data.json at `url` (its Remote Web Server's address), straight from it, never
/// through a proxy.
fn fetch_lhm(url: &str) -> Result<Json, String> {
    let base = url.trim().trim_end_matches('/');
    let base = base.strip_suffix("/data.json").unwrap_or(base);
    let base = if base.contains("://") { base.to_string() } else { format!("http://{base}") };
    let config = ureq::Agent::config_builder().timeout_global(Some(LHM_TIMEOUT)).http_status_as_error(false).proxy(None);
    let agent: ureq::Agent = config.max_redirects(0).build().into();
    let mut reply = agent.get(&format!("{base}/data.json")).call().map_err(|e| e.to_string())?;
    match reply.status().as_u16() {
        200..=299 => {}
        401 => {
            return Err("HTTP 401: it asks for a user name and password, so turn its Remote Web Server's Authentication off while \
                        the probe runs".into());
        }
        code => return Err(format!("HTTP {code}")),
    }
    let text = reply.body_mut().with_config().limit(LHM_LIMIT).read_to_string().map_err(|e| e.to_string())?;
    let tree = serde_json::from_str::<Json>(&text).ok().filter(|tree| tree["Children"].is_array());
    tree.ok_or_else(|| "its reply isn't sensor data".to_string())
}

/// What the probe saw the reader do, through the modules and locks it handed it: each module opened, the time
/// each module function took, and each wait for a shared lock.
#[derive(Default)]
struct Log(Mutex<Seen>);

#[derive(Default)]
struct Seen {
    /// What the probe is doing (`READ`, `DUMP`, `AFTER`).
    phase: &'static str,
    loads: Vec<(ModuleId, Result<(), OpenError>)>,
    calls: BTreeMap<String, Vec<Duration>>,
    waits: Vec<Wait>,
}

/// One wait for a shared lock: how long it took, whether it was had, and how long it was held.
struct Wait {
    bus: Bus,
    phase: &'static str,
    timeout_ms: u32,
    waited: Duration,
    got: bool,
    held: Option<Duration>,
}

impl Log {
    fn seen(&self) -> MutexGuard<'_, Seen> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A module, its calls timed.
struct Timed {
    exec: Box<dyn Exec>,
    log: Arc<Log>,
}

impl Exec for Timed {
    fn execute(&self, function: &str, input: &[u64], out: &mut [u64]) -> Result<(), IoError> {
        let started = Instant::now();
        let done = self.exec.execute(function, input, out);
        let took = started.elapsed();
        let mut seen = self.log.seen();
        match seen.calls.get_mut(function) {
            Some(times) => times.push(took),
            None => {
                seen.calls.insert(function.to_string(), vec![took]);
            }
        }
        done
    }
}

/// `open`, with each module it opens logged, and its calls timed.
fn timed(open: OpenModule, log: Arc<Log>) -> OpenModule {
    Box::new(move |module| {
        let opened = open(module);
        log.seen().loads.push((module, opened.as_ref().map(|_| ()).map_err(|e| *e)));
        opened.map(|exec| Box::new(Timed { exec, log: log.clone() }) as Box<dyn Exec>)
    })
}

/// A shared lock, each wait for it logged.
struct Watched {
    lock: Box<dyn Lock>,
    bus: Bus,
    log: Arc<Log>,
}

impl Lock for Watched {
    fn acquire(&self, timeout_ms: u32) -> Option<Held<'_>> {
        let started = Instant::now();
        let held = self.lock.acquire(timeout_ms);
        let mut seen = self.log.seen();
        let at = seen.waits.len();
        let phase = seen.phase;
        seen.waits.push(Wait { bus: self.bus, phase, timeout_ms, waited: started.elapsed(), got: held.is_some(), held: None });
        drop(seen);
        let got = Instant::now();
        held.map(|held| {
            Held::new(move || {
                drop(held);
                self.log.seen().waits[at].held = Some(got.elapsed());
            })
        })
    }
}

/// The machine's clock, for the reader: the probe keeps a handle on it too.
struct Shared(Arc<dyn Clock + Sync>);

impl Clock for Shared {
    fn since_boot(&self) -> Duration {
        self.0.since_boot()
    }

    fn awake(&self) -> Duration {
        self.0.awake()
    }

    fn sleep(&self, time: Duration) {
        self.0.sleep(time)
    }
}

/// One sample's read, as the pc source would publish it: when, on the probe's clock, and how long it took.
struct Sample {
    at: Duration,
    took: Duration,
    readings: Readings,
}

/// One chip's register dumps.
struct ChipDump {
    slot: usize,
    key: String,
    spec: &'static ChipSpec,
    base: u16,
    fans: [bool; 6],
    samples: Vec<Json>,
    counts: Counts,
    /// Registers the dumps read, and those whose index didn't read back.
    reads: u32,
    invalid: u32,
}

impl ChipDump {
    fn new(chip: &Chip, key: String) -> ChipDump {
        ChipDump { slot: chip.slot, key, spec: chip.spec, base: chip.base, fans: chip.fans, samples: Vec::new(), counts: chip.counts,
                   reads: 0, invalid: 0 }
    }

    /// One sample of the chip's registers (`registers`), read as the reader reads them, then its
    /// temperatures and every fan's count decoded as the reader decodes them, with whether the reader reads
    /// that fan at all (regs 0x13 and 0x0C). The bank is read first and again last; a chip with banks is
    /// read no further than the first unless it's in bank 0, as the reader reads it.
    fn dump(&mut self, chip: &mut Chip, hw: &HwLock, at: Duration) -> Json {
        let list = registers(chip.spec);
        let (mut regs, mut values, mut end_bank, mut error, mut skipped) = (Map::new(), HashMap::new(), None, None, None);
        for (n, &reg) in list.iter().chain(&[ite::BANK]).enumerate() {
            if hw.stopping() {
                break;
            }
            match chip.read_reg(reg) {
                Ok(value) => {
                    self.reads += 1;
                    self.invalid += u32::from(value.is_none());
                    if n == list.len() {
                        end_bank = value.map(bank);
                    } else {
                        regs.insert(hex(reg, 2), json!(value.map(|v| hex(v, 2))));
                        values.extend(value.map(|v| (reg, v)));
                    }
                    // in another bank, its registers 0x10-0xAF are something else; and a bank that didn't
                    // read back can't be told
                    if n == 0 && chip.spec.bank_guard && value.map(bank) != Some(0) {
                        skipped = Some(if value.is_some() { "bank" } else { "read_back" });
                        break;
                    }
                }
                Err(e) => {
                    error = Some(io_text(e));
                    break;
                }
            }
        }
        let get = |reg: u8| values.get(&reg).copied();
        if let Some(skipped) = skipped {
            return json!({ "t_ms": ms(at), "bank": [get(ite::BANK).map(bank), Json::Null], "regs": regs, "skipped": skipped });
        }
        let temperatures: Vec<Option<f64>> =
            (0..chip.spec.temperatures).map(|i| get(ite::TEMPERATURE + i as u8).and_then(ite::temperature)).collect();
        let fans: Vec<Json> = (0..chip.spec.fans)
            .map(|i| {
                let count = get(ite::FAN_LOW[i]).zip(get(ite::FAN_HIGH[i])).map(|(low, high)| low as u16 | (high as u16) << 8);
                json!({ "fan": i + 1, "read": chip.fans[i], "count": count.map(|c| hex(c, 4)), "rpm": count.and_then(ite::fan_rpm) })
            })
            .collect();
        let mut sample = json!({ "t_ms": ms(at), "bank": [get(ite::BANK).map(bank), end_bank], "regs": regs,
                                 "temperatures": temperatures, "fans": fans });
        if let Some(error) = error {
            sample["error"] = json!(error);
        }
        sample
    }
}

/// The registers dumped for a chip (spec §6.4), in the order read: the bank, configuration, fans on (4-6,
/// then 1-3) and vendor registers, the temperatures, each fan's count (low byte first, as the reader reads
/// it), the voltages, then the control and duty registers of the chip's own PWM channels.
fn registers(spec: &ChipSpec) -> Vec<u8> {
    let mut regs = vec![ite::BANK, ite::CONFIGURATION, ite::FANS_4_TO_6, ite::FANS_1_TO_3, ite::VENDOR];
    regs.extend((0..spec.temperatures as u8).map(|i| ite::TEMPERATURE + i));
    for i in 0..spec.fans {
        regs.extend([ite::FAN_LOW[i], ite::FAN_HIGH[i]]);
    }
    regs.extend(VOLTAGES);
    regs.extend(&PWM_CONTROL[..spec.pwms]);
    regs.extend(&PWM_DUTY[..spec.pwms]);
    regs
}

/// The register bank, from reg 0x06.
fn bank(value: u8) -> u8 {
    (value >> 5) & 3
}

/// The ID at a slot's configuration port, and, when a chip answers there (so it's in configuration mode),
/// its logical device: reads, which write only the configuration index.
fn read_port(h: &dyn Exec, slot: usize) -> Result<(u16, Option<u8>), IoError> {
    let mut out = [0u64];
    h.execute("ioctl_select_slot", &[slot as u64], &mut [])?;
    h.execute("ioctl_superio_inw", &[ite::CHIP_ID as u64], &mut out)?;
    let id = out[0] as u16;
    if id == 0xFFFF || id == 0 {
        return Ok((id, None));
    }
    h.execute("ioctl_superio_inb", &[ite::LDN as u64], &mut out)?;
    Ok((id, Some(out[0] as u8)))
}

/// A probe run: the reader, the probe's own handles on the shared locks for its dumps, and what it found.
struct Probe<'a> {
    options: &'a Options,
    machine: Machine,
    board: Board,
    hw: Arc<HwLock>,
    log: Arc<Log>,
    reader: Reader,
    /// How the reader's shared locks were had: made here (true), opened, or the note saying why not.
    made: Vec<(Bus, Result<bool, String>)>,
    pci: Option<Watched>,
    isa: Option<Watched>,
    /// The clock when the probe started.
    start: Duration,
    samples: Vec<Sample>,
    cpu_raw: Vec<Json>,
    /// The energy counter's unit register, and the counter at the last dump.
    unit: Option<u64>,
    energy: Option<(u32, Duration)>,
    chips: Vec<ChipDump>,
    after: Option<Vec<Json>>,
    rows: BTreeMap<String, Row>,
    notes: Vec<String>,
}

impl<'a> Probe<'a> {
    /// Opens the reader (and with it the CPU's module) on the machine's parts, each logged.
    fn new(options: &'a Options, machine: Machine, hw: Arc<HwLock>) -> Probe<'a> {
        let log = Arc::new(Log::default());
        log.seen().phase = READ;
        let watched = |bus| (machine.lock)(bus).map(|(lock, made)| (Watched { lock, bus, log: log.clone() }, made));
        // the reader's handles on the shared locks; for `--stage cpu`, none for the chips, which it then leaves alone
        let mut made = Vec::new();
        let mut give = |bus| match watched(bus) {
            Ok((lock, created)) => {
                made.push((bus, Ok(created)));
                Some(Box::new(lock) as Box<dyn Lock>)
            }
            Err(note) => {
                made.push((bus, Err(note)));
                None
            }
        };
        let locks = Locks { pci: give(Bus::Pci), isa: if options.all { give(Bus::Isa) } else { None } };
        // and the probe's own, for its dumps
        let pci = watched(Bus::Pci).ok().map(|(lock, _)| lock);
        let isa = if options.all { watched(Bus::Isa).ok().map(|(lock, _)| lock) } else { None };
        let value = |name| machine.bios.iter().find(|(n, _)| *n == name).and_then(|(_, v)| v.clone()).unwrap_or_default();
        let board = Board { manufacturer: value("BaseBoardManufacturer"), product: value("BaseBoardProduct") };
        let reader = Reader::open(timed((machine.modules)(), log.clone()), locks, machine.cpu.clone(), &board,
                                  Box::new(Shared(machine.clock.clone())), hw.clone());
        let start = machine.clock.since_boot();
        Probe { options, machine, board, hw, log, reader, made, pci, isa, start, samples: Vec::new(), cpu_raw: Vec::new(), unit: None,
                energy: None, chips: Vec::new(), after: None, rows: BTreeMap::new(), notes: Vec::new() }
    }

    /// The samples, then, with the chips, the check that they were left as found.
    fn run(&mut self) {
        self.sample_all();
        if self.options.all {
            self.check_after();
        }
    }

    /// `options.samples` samples, `options.every` apart, until Ctrl+C. One that ends late (say LHM took long
    /// to answer) isn't caught up on: the next is a whole `every` after it ended, never straight after.
    fn sample_all(&mut self) {
        let every = self.options.every;
        let mut due = self.start;
        for _ in 0..self.options.samples {
            if !self.wait_until(due) {
                break;
            }
            self.sample();
            let (next, now) = (due.saturating_add(every), self.machine.clock.since_boot());
            due = if now >= next { now.saturating_add(every) } else { next };
        }
    }

    /// Waits until `due` on the clock, looking for Ctrl+C every `STEP`: false once it's pressed.
    fn wait_until(&self, due: Duration) -> bool {
        loop {
            if self.hw.stopping() {
                return false;
            }
            let now = self.machine.clock.since_boot();
            if now >= due {
                return true;
            }
            self.machine.clock.sleep((due - now).min(STEP));
        }
    }

    /// One sample: the reader's read, then the dumps, then LHM's readings beside the reader's.
    fn sample(&mut self) {
        let at = self.machine.clock.since_boot().saturating_sub(self.start);
        self.log.seen().phase = READ;
        let started = Instant::now();
        let mut readings = self.reader.read();
        let took = started.elapsed();
        self.log.seen().phase = DUMP;
        if !self.options.all {
            // the reader was given no lock for the chips, so as to leave them alone: not a problem to note
            readings.notes.retain(|note| *note != Bus::Isa.unshared());
        }
        for note in &readings.notes {
            self.note(note);
        }
        if !self.hw.stopping() {
            self.dump_cpu(at);
            if self.options.all {
                self.dump_chips(at);
            }
        }
        let options = self.options;
        if let Some(url) = &options.lhm
            && !self.hw.stopping()
        {
            self.compare(url, &readings);
        }
        self.samples.push(Sample { at, took, readings });
    }

    /// The CPU's registers, raw, beside what the reader made of them: Tctl and every CCD slot this CPU has
    /// (under the PCI lock, as the reader reads them), then the energy counter.
    fn dump_cpu(&mut self, at: Duration) {
        let Some(cpu) = self.reader.amd() else { return };
        let mut sample = json!({ "t_ms": ms(at) });
        if self.unit.is_none() {
            self.unit = cpu.msr(amd::PWR_UNIT).ok();
        }
        match self.pci.as_ref().map(|pci| pci.acquire(amd::LOCK_MS)) {
            // without the lock the reader reads no temperatures either, and says so
            None => {}
            Some(None) => sample["busy"] = json!(true),
            Some(Some(_shared)) => {
                let Some(_hw) = self.hw.hold() else { return };
                sample["tctl"] = match cpu.smn(amd::TCTL) {
                    Ok(raw) => json!({ "raw": hex(raw, 8), "c": amd::tctl(raw) }),
                    Err(e) => json!({ "error": io_text(e) }),
                };
                let mut ccds = Vec::new();
                let (offset, slots) = amd::ccd_slots(cpu.cpu().family, cpu.cpu().model).unwrap_or_default();
                for slot in 0..slots as u32 {
                    if self.hw.stopping() {
                        break;
                    }
                    ccds.push(match cpu.smn(amd::TCTL + offset + 4 * slot) {
                        Ok(raw) => json!({ "slot": slot, "raw": hex(raw, 8), "valid": amd::ccd_valid(raw), "c": amd::ccd(raw) }),
                        Err(e) => json!({ "slot": slot, "error": io_text(e) }),
                    });
                }
                sample["ccds"] = json!(ccds);
            }
        }
        sample["energy"] = match cpu.msr(amd::PKG_ENERGY) {
            Ok(raw) => {
                // over the time since the last dump, as the reader works it out but with none of its limits
                let energy = raw as u32;
                let watts = match (self.energy, self.unit) {
                    (Some((last, then)), Some(unit)) if at > then => {
                        Some(energy.wrapping_sub(last) as f64 * amd::energy_unit(unit) / (at - then).as_secs_f64())
                    }
                    _ => None,
                };
                self.energy = Some((energy, at));
                json!({ "raw": hex(raw, 8), "w": watts })
            }
            Err(e) => json!({ "error": io_text(e) }),
        };
        self.cpu_raw.push(sample);
    }

    /// Each chip's registers, under the ISA lock as the reader reads them; not those of a chip the reader
    /// found missing, which it leaves alone but for a try now and then (ite.rs).
    fn dump_chips(&mut self, at: Duration) {
        let names: Vec<&str> = self.reader.superio().chips().map(|chip| chip.spec.name).collect();
        for (chip, key) in self.reader.superio_mut().chips_mut().zip(hardware(&names)) {
            let dump = match self.chips.iter().position(|d| d.slot == chip.slot && d.base == chip.base) {
                Some(i) => &mut self.chips[i],
                None => {
                    self.chips.push(ChipDump::new(chip, key));
                    self.chips.last_mut().unwrap()
                }
            };
            let sample = if chip.is_missing() {
                json!({ "t_ms": ms(at), "missing": true })
            } else {
                match self.isa.as_ref().map(|isa| isa.acquire(ite::POLL_MS)) {
                    // without the lock the reader reads no chip either, and says so
                    None => continue,
                    Some(None) => json!({ "t_ms": ms(at), "busy": true }),
                    Some(Some(_shared)) => {
                        let Some(_hw) = self.hw.hold() else { return };
                        dump.dump(chip, &self.hw, at)
                    }
                }
            };
            dump.counts = chip.counts;
            dump.samples.push(sample);
        }
    }

    /// Reads each configuration port as the probe leaves it, without a key, and the logical device where a
    /// chip answers: a chip that left configuration mode reads 0xFFFF, as one that was never in it; one that
    /// was in it reads its ID, with the logical device it had. Beside each, what the first detection attempt
    /// there found before it changed anything, and whether what's read now is what's expected: as found, or,
    /// for a chip we put in configuration mode that mustn't be sent the exit (spec D13), still in it with the
    /// logical device it had.
    fn check_after(&mut self) {
        if self.hw.stopping() {
            return;
        }
        self.log.seen().phase = AFTER;
        let open = timed((self.machine.modules)(), self.log.clone());
        let mut after = Vec::new();
        // 0x4E first, as detection goes
        for slot in [1, 0] {
            let port = PORTS[slot];
            let attempts: Vec<&Attempt> = self.reader.superio().attempts.iter().filter(|a| a.port == port).collect();
            let found = attempts.iter().find(|a| a.id_before.is_some());
            let (id_before, ldn_before) = (found.and_then(|a| a.id_before), found.and_then(|a| a.ldn_before));
            let left_in = attempts.iter().find(|a| a.key_sent && !a.exit_sent && a.id_after.is_some_and(ite::noconf_family));
            let read = match (&self.isa, open(ModuleId::LpcIo)) {
                (_, Err(e)) => Err(format!("{e:?}")),
                (None, _) => Err("no lock".to_string()),
                (Some(isa), Ok(h)) => match isa.acquire(ite::DETECT_MS) {
                    None => Err("busy".to_string()),
                    Some(_shared) => {
                        let Some(_hw) = self.hw.hold() else { break };
                        read_port(&*h, slot).map_err(io_text)
                    }
                },
            };
            let found = json!({ "id": id_before.map(|id| hex(id, 4)), "ldn": ldn_before.map(|l| hex(l, 2)) });
            let mut row = json!({ "port": hex(port, 2), "found": found });
            match read {
                Ok((id, ldn)) => {
                    row["id"] = json!(hex(id, 4));
                    row["ldn"] = json!(ldn.map(|l| hex(l, 2)));
                    let as_found = id_before.map(|before| id == before && (before == 0xFFFF || ldn_before.is_none() || ldn == ldn_before));
                    row["as_found"] = json!(as_found);
                    let by_design = left_in.is_some_and(|a| a.id_after == Some(id) && (a.ldn_before.is_none() || ldn == a.ldn_before));
                    row["expected"] = json!(if by_design { Some(true) } else { as_found });
                    if by_design {
                        row["by_design"] = json!("left in configuration mode: the chip mustn't be sent the exit (NOCONF)");
                    }
                }
                Err(e) => row["error"] = json!(e),
            }
            after.push(row);
        }
        self.after = Some(after);
    }

    /// Libre Hardware Monitor's readings now, beside this sample's.
    fn compare(&mut self, url: &str, readings: &Readings) {
        match (self.machine.lhm)(url) {
            Ok(tree) => add_rows(&mut self.rows, &readings.values, &lhm_readings(&tree), self.options.all),
            Err(why) => self.note(&format!("Libre Hardware Monitor couldn't be read at {url} ({why}), so nothing was compared with it.")),
        }
    }

    fn note(&mut self, note: &str) {
        if !self.notes.iter().any(|n| n == note) {
            self.notes.push(note.to_string());
        }
    }

    /// Everything, as the JSON the probe prints (spec §6.4).
    fn report(&self) -> Json {
        let seen = self.log.seen();
        let cpu = &self.machine.cpu;
        let mut board: Map<String, Json> = self.machine.bios.iter().map(|(name, value)| (name.to_string(), json!(value))).collect();
        board.insert("mapped".into(), json!(Names::for_board(&self.board).mapped()));
        let ccd_slots = amd::ccd_slots(cpu.family, cpu.model).map(|(offset, count)| json!({ "offset": hex(offset, 3), "count": count }));

        let modules: Vec<Json> = [ModuleId::AmdFamily17, ModuleId::LpcIo]
            .iter()
            .map(|&module| {
                let dir = blobs::find_dir(&self.machine.exe).or_else(|| self.machine.exe.parent().map(|dir| dir.join("pawnio")));
                let path = dir.unwrap_or_default().join(module.file());
                let sha = std::fs::read(&path).ok().map(|file| sha256::hex(&file));
                let loads: Vec<String> = seen.loads.iter().filter(|(m, _)| *m == module).map(|(_, done)| match done {
                    Ok(()) => "ok".to_string(),
                    Err(e) => format!("{e:?}"),
                }).collect();
                json!({ "name": module.name(), "path": path.display().to_string(), "sha256": sha,
                        "pinned_ok": sha.as_deref() == Some(blobs::pin(module)), "loads": loads })
            })
            .collect();

        let mutexes: Vec<Json> = self
            .made
            .iter()
            .map(|(bus, made)| {
                let waits: Vec<Json> = seen.waits.iter().filter(|w| w.bus == *bus).map(|w| {
                    json!({ "phase": w.phase, "timeout_ms": w.timeout_ms, "wait_ms": ms(w.waited), "got": w.got, "held_ms": w.held.map(ms) })
                }).collect();
                let state = match made {
                    Ok(true) => "created",
                    Ok(false) => "opened",
                    Err(_) => "none",
                };
                json!({ "name": bus.name(), "state": state, "note": made.as_ref().err(), "acquires": waits })
            })
            .collect();

        let start_ms = self.start.as_millis() as u64;
        let superio = self.options.all.then(|| {
            let attempts = &self.reader.superio().attempts;
            let ports: Vec<Json> = [PORTS[1], PORTS[0]].iter().map(|&port| {
                let attempts: Vec<Json> = attempts.iter().filter(|a| a.port == port).map(|a| attempt(a, start_ms)).collect();
                json!({ "port": hex(port, 2), "attempts": attempts })
            }).collect();
            ports
        });

        let chips: Vec<Json> = self.chips.iter().map(|dump| {
            json!({ "chip": dump.spec.name, "key": dump.key, "port": hex(PORTS[dump.slot], 2), "base": hex(dump.base, 4),
                    "fans_read": &dump.fans[..dump.spec.fans], "regs": dump.samples })
        }).collect();
        let counts: Map<String, Json> = self.chips.iter().map(|dump| {
            (dump.key.clone(), json!({ "read_back_failures": dump.counts.read_backs, "bank_guard_trips": dump.counts.bank_trips,
                                       "vendor_misses": dump.counts.misses, "dump_reads": dump.reads,
                                       "dump_read_back_failures": dump.invalid }))
        }).collect();

        let last = self.samples.last().map(|s| &s.readings);
        let keys: Map<String, Json> = last.iter().flat_map(|r| &r.values).map(|(k, v)| (k.clone(), json!(v))).collect();
        let readings: Vec<Json> = self.samples.iter().map(|s| {
            let values: Map<String, Json> = s.readings.values.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
            json!({ "t_ms": ms(s.at), "poll_ms": ms(s.took), "values": values, "cpu_temp": s.readings.cpu_temp, "notes": s.readings.notes })
        }).collect();

        json!({
            "probe": 1,
            "stage": if self.options.all { "all" } else { "cpu" },
            "exe": self.machine.exe.display().to_string(),
            "samples": { "asked": self.options.samples, "done": self.samples.len(), "every_ms": self.options.every.as_millis() as u64 },
            "stopped": self.hw.stopping(),
            "board": board,
            "cpu": { "vendor": cpu.vendor, "family": hex(cpu.family, 2), "model": hex(cpu.model, 2), "brand": cpu.brand,
                     "ccd_slots": ccd_slots, "read": self.reader.amd().is_some() },
            "pawnio": self.machine.pawnio,
            "modules": modules,
            "mutexes": mutexes,
            "cpu_raw": { "pwr_unit": self.unit.map(|raw| json!({ "raw": hex(raw, 8), "joules": amd::energy_unit(raw) })),
                         "kept_ccds": self.reader.amd().map(|read| read.ccds().to_vec()), "samples": self.cpu_raw },
            "superio": superio,
            "chips": chips,
            "counts": counts,
            "after": self.after,
            "keys": keys,
            "labels": last.map(|r| &r.labels),
            "cpu_temp": last.and_then(|r| r.cpu_temp),
            "readings": readings,
            "compare": self.options.lhm.as_ref().map(|_| self.rows.iter().map(|(key, row)| row.json(key)).collect::<Vec<_>>()),
            "tctl_vs_chips": self.tctl_vs_chips(),
            "timing": timing(&seen, &self.samples),
            "notes": self.notes,
        })
    }

    /// How far each chip temperature is from Tctl over the samples (the median of ours minus Tctl), nearest
    /// first: the one that follows Tctl is the CPU's own sensor on the board (IT8696E temperature 3 on this
    /// PC's, spec §2.4).
    fn tctl_vs_chips(&self) -> Vec<Json> {
        let mut diffs: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
        for sample in &self.samples {
            let values = &sample.readings.values;
            let Some((_, tctl)) = values.iter().find(|(k, _)| k == "pawnio.cpu.temperature.tctl") else { continue };
            for (key, value) in values.iter().filter(|(k, _)| k.starts_with("pawnio.ite_") && k.contains(".temperature.")) {
                diffs.entry(key.as_str()).or_default().push(value - tctl);
            }
        }
        let mut rows: Vec<(f64, Json)> = diffs
            .into_iter()
            .map(|(key, diffs)| {
                let median = median(diffs.clone());
                (median.abs(), json!({ "key": key, "median_diff": median, "samples": diffs.len() }))
            })
            .collect();
        rows.sort_by(|a, b| a.0.total_cmp(&b.0));
        rows.into_iter().map(|(_, row)| row).collect()
    }
}

/// One detection attempt, its time from the probe's start.
fn attempt(a: &Attempt, start_ms: u64) -> Json {
    json!({ "t_ms": a.at_ms.saturating_sub(start_ms), "id_before": a.id_before.map(|id| hex(id, 4)), "key_sent": a.key_sent,
            "id_after": a.id_after.map(|id| hex(id, 4)), "chip": a.chip, "ldn_before": a.ldn_before.map(|l| hex(l, 2)),
            "find_bars": a.find_bars, "base": a.base.iter().map(|&b| hex(b, 4)).collect::<Vec<_>>(), "ldn_restored": a.ldn_restored,
            "exit_sent": a.exit_sent, "left_keyed": a.left_keyed, "outcome": a.outcome })
}

/// The time each module function took (µs), each detection attempt held the ISA lock (ms), each chip's poll
/// and the CPU's temperatures held their lock (ms), and each whole read took (ms).
fn timing(seen: &Seen, samples: &[Sample]) -> Json {
    let micros = |times: &[Duration]| stats(times.iter().map(|t| (t.as_nanos() as f64 / 100.0).round() / 10.0).collect());
    let mut ioctl: Map<String, Json> = seen.calls.iter().map(|(function, times)| (function.clone(), micros(times))).collect();
    ioctl.insert("all".into(), micros(&seen.calls.values().flatten().copied().collect::<Vec<_>>()));
    let held = |bus: Bus, timeout_ms: u32| -> Vec<f64> {
        seen.waits.iter().filter(|w| w.bus == bus && w.phase == READ && w.timeout_ms == timeout_ms).filter_map(|w| w.held.map(ms)).collect()
    };
    let reads: Vec<f64> = samples.iter().map(|s| ms(s.took)).collect();
    json!({
        "ioctl_us": ioctl,
        "detection_ms": held(Bus::Isa, ite::DETECT_MS),
        "chip_poll_ms": stats(held(Bus::Isa, ite::POLL_MS)),
        "cpu_temperatures_ms": stats(held(Bus::Pci, amd::LOCK_MS)),
        "read_ms": { "first": reads.first(), "rest": stats(reads.iter().skip(1).copied().collect()) },
    })
}

/// How many, the median, the 99th percentile (nearest rank) and the most.
fn stats(mut values: Vec<f64>) -> Json {
    values.sort_by(f64::total_cmp);
    let p99 = values.get((values.len() * 99).div_ceil(100).saturating_sub(1)).copied();
    json!({ "count": values.len(), "median": (!values.is_empty()).then(|| median(values.clone())), "p99": p99, "max": values.last() })
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    match values.len() {
        0 => f64::NAN,
        n if n % 2 == 1 => values[n / 2],
        n => (values[n / 2 - 1] + values[n / 2]) / 2.0,
    }
}

/// One Libre Hardware Monitor sensor with a reading: its id (`/lpc/it87952e/0/fan/1`: the chip, which one of
/// that name, the type, and the channel from 0), its name, its key as the pc source has it, and its value.
#[derive(Clone, Debug, PartialEq)]
struct LhmReading {
    id: String,
    name: String,
    key: String,
    value: f64,
}

/// Every sensor with an id and a reading in LHM's data.json, each read as the pc source reads it (sources.rs).
fn lhm_readings(tree: &Json) -> Vec<LhmReading> {
    fn walk(node: &Json, hardware: &str, named: &mut HashMap<String, usize>, out: &mut Vec<LhmReading>) {
        if let Some(id) = node["SensorId"].as_str() {
            // the pc source's reading of this one sensor: its type, its name as a key part, its value
            if let Some(sensor) = lhm_sensors(node).pop()
                && let Some(value) = sensor.value
            {
                let key = format!("lhm.{hardware}.{}.{}", sensor.kind, sensor.key);
                out.push(LhmReading { id: id.to_string(), name: sensor.name, key, value });
            }
            return;
        }
        // hardware is named as the pc source names it, a second one of a name with `_2`
        let hardware = match node["HardwareId"].as_str() {
            Some(id) => {
                let key = lhm_name(node["Text"].as_str().unwrap_or_default(), Some(id), "hardware");
                let count = named.entry(key.clone()).or_default();
                *count += 1;
                if *count > 1 { format!("{key}_{count}") } else { key }
            }
            None => hardware.to_string(),
        };
        for child in node["Children"].as_array().into_iter().flatten() {
            walk(child, &hardware, named, out);
        }
    }
    let mut out = Vec::new();
    walk(tree, "", &mut HashMap::new(), &mut out);
    out
}

/// LHM's id for one of our chip keys: `pawnio.ite_it87952e.fan.fan_2` is `/lpc/it87952e/0/fan/1`, and
/// `pawnio.ite_it8696e_2.…` the second IT8696E's, `/lpc/it8696e/1/…`. Chips are matched by channel, never by
/// name, as LHM names channels from a board's map where it has one (spec I-L3).
fn lhm_id(key: &str) -> Option<String> {
    let (hardware, rest) = key.strip_prefix("pawnio.ite_")?.split_once('.')?;
    let (kind, sensor) = rest.split_once('.')?;
    let channel: usize = sensor.strip_prefix(kind)?.strip_prefix('_')?.parse().ok()?;
    let (chip, nth) = match hardware.rsplit_once('_') {
        Some((chip, n)) => (chip, n.parse::<usize>().ok()?),
        None => (hardware, 1),
    };
    Some(format!("/lpc/{chip}/{}/{kind}/{}", nth.checked_sub(1)?, channel.checked_sub(1)?))
}

/// Our key for one of LHM's readings: a chip channel by its id (`lhm_id` the other way), for chips in ite.rs's
/// table; a CPU reading by the name LHM gives it (Amd17Cpu.cs). None for anything we don't read.
fn our_key(reading: &LhmReading) -> Option<String> {
    let parts: Vec<&str> = reading.id.trim_start_matches('/').split('/').collect();
    match parts.as_slice() {
        ["lpc", chip, nth, kind @ ("temperature" | "fan"), channel] => {
            CHIPS.iter().find(|c| c.name.eq_ignore_ascii_case(chip))?;
            let (nth, channel): (usize, usize) = (nth.parse().ok()?, channel.parse().ok()?);
            let hardware = if nth == 0 { format!("ite_{chip}") } else { format!("ite_{chip}_{}", nth + 1) };
            Some(format!("pawnio.{hardware}.{kind}.{kind}_{}", channel + 1))
        }
        ["amdcpu", "0", "power", _] if reading.name == "Package" => Some("pawnio.cpu.power.package".to_string()),
        ["amdcpu", "0", "temperature", _] => {
            let sensor = match reading.name.as_str() {
                "Core (Tctl/Tdie)" | "Core (Tctl)" => "tctl".to_string(),
                "Core (Tdie)" => "tdie".to_string(),
                "CCDs Max (Tdie)" => "ccd_max".to_string(),
                "CCDs Average (Tdie)" => "ccd_average".to_string(),
                name => format!("ccd{}", name.strip_prefix("CCD")?.strip_suffix(" (Tdie)")?.parse::<usize>().ok()?),
            };
            Some(format!("pawnio.cpu.temperature.{sensor}"))
        }
        _ => None,
    }
}

/// Whether two readings of the same thing agree (spec §6.4): temperatures within 2 °C, fans within 5 % and
/// 50 RPM, power within 10 %.
fn agree(key: &str, ours: f64, theirs: f64) -> bool {
    let diff = (ours - theirs).abs();
    if key.contains(".fan.") {
        diff <= 0.05 * theirs.abs() + 50.0
    } else if key.contains(".power.") {
        diff <= 0.10 * theirs.abs()
    } else {
        diff <= 2.0
    }
}

/// One of our readings beside LHM's of the same channel, over the samples.
#[derive(Debug, Default)]
struct Row {
    lhm: Option<LhmReading>,
    ours: Option<f64>,
    /// The last sample's pair, when both had one, how many samples had both, and in how many they agreed.
    pair: Option<(f64, f64)>,
    samples: usize,
    agreed: usize,
}

impl Row {
    fn json(&self, key: &str) -> Json {
        let (ours, theirs) = self.pair.map_or((self.ours, self.lhm.as_ref().map(|l| l.value)), |(o, t)| (Some(o), Some(t)));
        let (chip, channel) = match key.strip_prefix("pawnio.ite_") {
            Some(rest) => {
                let id = lhm_id(key);
                let channel = id.as_deref().and_then(|id| id.rsplit('/').next()).and_then(|c| c.parse::<usize>().ok());
                let hardware = rest.split('.').next().unwrap_or_default();
                let chip = match hardware.rsplit_once('_') {
                    Some((chip, nth)) => format!("{} #{nth}", chip.to_uppercase()),
                    None => hardware.to_uppercase(),
                };
                (chip, channel)
            }
            // the CCDs by slot, from 0
            None => {
                let ccd = key.strip_prefix("pawnio.cpu.temperature.ccd").and_then(|n| n.parse::<usize>().ok());
                ("CPU".to_string(), ccd.map(|n| n - 1))
            }
        };
        json!({ "chip": chip, "channel": channel, "pawnio_key": key, "lhm_key": self.lhm.as_ref().map(|l| &l.key),
                "lhm_id": self.lhm.as_ref().map(|l| &l.id), "lhm_name": self.lhm.as_ref().map(|l| &l.name),
                "pawnio": ours, "lhm": theirs, "diff": ours.zip(theirs).map(|(o, t)| o - t),
                "ok": self.samples > 0 && self.agreed == self.samples, "samples": self.samples, "agreed": self.agreed })
    }
}

/// One sample of our readings and LHM's, paired by our key, into `rows`: each of ours, and each of LHM's
/// we'd read too (the CPU's, and, with `chips`, the chips'), with or without the other.
fn add_rows(rows: &mut BTreeMap<String, Row>, ours: &[(String, f64)], lhm: &[LhmReading], chips: bool) {
    let mut pairs: BTreeMap<String, (Option<f64>, Option<&LhmReading>)> = BTreeMap::new();
    for (key, value) in ours {
        pairs.entry(key.clone()).or_default().0 = Some(*value);
    }
    for reading in lhm {
        if let Some(key) = our_key(reading).filter(|key| chips || key.starts_with("pawnio.cpu."))
            && pairs.get(&key).is_none_or(|pair| pair.1.is_none())
        {
            pairs.entry(key).or_default().1 = Some(reading);
        }
    }
    for (key, (ours, theirs)) in pairs {
        let row = rows.entry(key.clone()).or_default();
        row.ours = ours.or(row.ours);
        if let Some(theirs) = theirs {
            row.lhm = Some(theirs.clone());
        }
        if let (Some(ours), Some(theirs)) = (ours, theirs) {
            row.pair = Some((ours, theirs.value));
            row.samples += 1;
            row.agreed += usize::from(agree(&key, ours, theirs.value));
        }
    }
}

/// Runs the probe on `machine` and writes the JSON: to `options.out`, with a summary on `out`, else to `out`.
/// The exit code: 0, or `STOPPED` after Ctrl+C (what was read is still written), or 1 when the file can't be
/// written (the JSON then goes to `out`).
fn probe(options: &Options, machine: Machine, hw: Arc<HwLock>, out: &mut dyn Write) -> i32 {
    let mut probe = Probe::new(options, machine, hw.clone());
    probe.run();
    let stopped = hw.stopping();
    if stopped {
        let note = format!("Stopped by Ctrl+C after {} of {} samples; the hardware sequence under way was finished first.",
                           probe.samples.len(), options.samples);
        probe.note(&note);
    }
    let report = probe.report();
    // the modules are unloaded now, before the JSON is written
    drop(probe);
    let text = serde_json::to_string_pretty(&report).unwrap_or_default();
    let code = if stopped { STOPPED } else { 0 };
    let Some(file) = &options.out else {
        let _ = writeln!(out, "{text}");
        return code;
    };
    if let Err(e) = std::fs::write(file, format!("{text}\n")) {
        let _ = writeln!(out, "{text}");
        return error(out, &format!("couldn't write {}: {e}", file.display()));
    }
    let rows = report["compare"].as_array().into_iter().flatten();
    let disagree: Vec<&Json> = rows.filter(|row| row["ok"] != true).map(|row| &row["pawnio_key"]).collect();
    let summary = json!({ "out": file.display().to_string(), "samples": report["samples"], "keys": report["keys"],
                          "compare_not_ok": report["compare"].is_array().then_some(disagree), "notes": report["notes"] });
    let _ = writeln!(out, "{}", serde_json::to_string_pretty(&summary).unwrap_or_default());
    code
}

/// A register or ID as hex text, with at least `digits` digits.
fn hex(value: impl Into<u64>, digits: usize) -> String {
    let value: u64 = value.into();
    format!("0x{value:0digits$X}")
}

/// A time in milliseconds, to the microsecond.
fn ms(time: Duration) -> f64 {
    (time.as_secs_f64() * 1e6).round() / 1e3
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::Path;

    use super::*;
    use crate::pawnio::fake::{self, FakeAmd, FakeClock, FakeLock, FakeLpc};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn options(list: &[&str]) -> Options {
        Options::parse(&args(list)).unwrap()
    }

    /// This PC's sensors as LHM shows them, at the hardware probe's first sample: the chips' channels under
    /// LHM's generic names (it has no map for the board), and the CPU's. Its pump reads a tach count off ours
    /// (6308 RPM for 6250), as it did in the probe.
    fn lhm_tree() -> Json {
        let sensor = |id: String, name: String, kind: &str, value: String| {
            json!({ "Text": name, "SensorId": id, "Type": kind, "Value": value, "Children": [] })
        };
        let temperatures = |chip: &str, values: &[(usize, u32)]| -> Json {
            let sensors: Vec<Json> = values.iter().map(|&(i, t)| {
                sensor(format!("/lpc/{chip}/0/temperature/{i}"), format!("Temperature #{}", i + 1), "Temperature", format!("{t}.0 °C"))
            }).collect();
            json!({ "Text": "Temperatures", "Children": sensors })
        };
        let fans = |chip: &str, values: &[u32]| -> Json {
            let sensors: Vec<Json> = values.iter().enumerate().map(|(i, rpm)| {
                sensor(format!("/lpc/{chip}/0/fan/{i}"), format!("Fan #{}", i + 1), "Fan", format!("{rpm} RPM"))
            }).collect();
            json!({ "Text": "Fans", "Children": sensors })
        };
        let cpu = |i: usize, name: &str, value: &str| sensor(format!("/amdcpu/0/temperature/{i}"), name.into(), "Temperature", value.into());
        json!({ "Text": "Sensor", "Children": [{ "Text": "DESKTOP", "Children": [
            { "Text": "X870E AORUS ELITE X3D ICE", "HardwareId": "/motherboard", "Children": [
                { "Text": "ITE IT8696E", "HardwareId": "/lpc/it8696e/0", "Children": [
                    temperatures("it8696e", &[(0, 37), (1, 47), (2, 72), (3, 44), (4, 51)]),
                    fans("it8696e", &[2368, 1221, 1184, 1170, 0]) ]},
                { "Text": "ITE IT87952E", "HardwareId": "/lpc/it87952e/0", "Children": [
                    temperatures("it87952e", &[(0, 40), (2, 41)]),
                    fans("it87952e", &[0, 6308, 0]),
                    { "Text": "Controls", "Children": [
                        sensor("/lpc/it87952e/0/control/1".into(), "Fan #2".into(), "Control", "100.0 %".into())] } ]}]},
            { "Text": "AMD Ryzen 9 9950X3D", "HardwareId": "/amdcpu/0", "Children": [
                { "Text": "Temperatures", "Children": [cpu(2, "Core (Tctl/Tdie)", "72.1 °C"), cpu(3, "CCD1 (Tdie)", "39.0 °C"),
                                                       cpu(4, "CCD2 (Tdie)", "63.0 °C"), cpu(5, "CCDs Max (Tdie)", "63.0 °C"),
                                                       cpu(6, "CCDs Average (Tdie)", "51.0 °C")] },
                { "Text": "Powers", "Children": [sensor("/amdcpu/0/power/0".into(), "Package".into(), "Power", "101.0 W".into())] } ]}]}]})
    }

    /// This PC on fakes, as the hardware probe found it and as the reader's tests have it: a Ryzen 9 9950X3D at
    /// 72.125 °C with CCDs at 39 and 63 °C, and its two ITE chips; the test keeps the parts to look at.
    struct Rig {
        amd: FakeAmd,
        lpc: FakeLpc,
        pci: FakeLock,
        isa: FakeLock,
        clock: FakeClock,
        hw: Arc<HwLock>,
    }

    impl Rig {
        fn new() -> Rig {
            Rig { amd: FakeAmd::this_pc(), lpc: FakeLpc::this_pc(), pci: FakeLock::default(), isa: FakeLock::default(),
                  clock: FakeClock::default(), hw: Arc::default() }
        }

        /// The machine on these parts. Its LHM answers with `lhm_tree`, and moves the energy counter on by
        /// 100 J each time it's asked, so a sample a second after one that asked reads 100 W.
        fn machine(&self) -> Machine {
            let (amd, lpc, pci, isa) = (self.amd.clone(), self.lpc.clone(), self.pci.clone(), self.isa.clone());
            let (energy, asked, start) = (self.amd.clone(), Cell::new(0u64), self.amd.msr(amd::PKG_ENERGY));
            let bios = [("BaseBoardManufacturer", "Gigabyte Technology Co., Ltd."), ("BaseBoardProduct", "X870E AORUS ELITE X3D ICE"),
                        ("BIOSVersion", "F6")];
            Machine {
                exe: Path::new(env!("CARGO_MANIFEST_DIR")).join(r"target-agent\release\wvplay.exe"),
                bios: bios.into_iter().map(|(name, value)| (name, Some(value.to_string()))).collect(),
                cpu: Cpuid { vendor: "AuthenticAMD".into(), family: 0x1A, model: 0x44, brand: "AMD Ryzen 9 9950X3D 16-Core Processor".into() },
                pawnio: json!({ "installed": "2.1.0.0", "service": "running", "open": "ok", "driver_version": "2.1.0" }),
                modules: Box::new(move || fake::opener(Ok(amd.clone()), Ok(lpc.clone()))),
                lock: Box::new(move |bus| Ok((Box::new(if bus == Bus::Pci { pci.clone() } else { isa.clone() }) as Box<dyn Lock>, true))),
                clock: Arc::new(self.clock.clone()),
                lhm: Box::new(move |_| {
                    asked.set(asked.get() + 1);
                    energy.set_msr(amd::PKG_ENERGY, start + asked.get() * 100 * 65_536);
                    Ok(lhm_tree())
                }),
            }
        }

        fn probe<'a>(&self, options: &'a Options) -> Probe<'a> {
            Probe::new(options, self.machine(), self.hw.clone())
        }
    }

    /// `--fake`'s machine: this PC on fakes.
    pub(super) fn fake_machine() -> Machine {
        Rig::new().machine()
    }

    /// The options, and their defaults: 3 samples for the CPU, 60 with the chips, a second apart. At most
    /// 600 samples, at least half a second apart for the CPU, and a second with the chips.
    #[test]
    fn options_parse() {
        let cpu = options(&[]);
        assert_eq!((cpu.all, cpu.samples, cpu.every, cpu.lhm, cpu.out, cpu.fake), (false, 3, Duration::from_secs(1), None, None, false));
        let all = options(&["--stage", "all"]);
        assert_eq!((all.all, all.samples), (true, 60));
        let set = options(&["--stage", "all", "--samples", "5", "--every", "2500", "--compare-lhm", "http://127.0.0.1:8085", "--out",
                            r"C:\x\probe.json", "--fake"]);
        assert_eq!(set, Options { all: true, samples: 5, every: Duration::from_millis(2500), lhm: Some("http://127.0.0.1:8085".into()),
                                  out: Some(PathBuf::from(r"C:\x\probe.json")), fake: true });
        assert_eq!(options(&["--stage", "cpu", "--samples", "0"]).samples, 1);
        assert_eq!(options(&["--samples", "100000"]).samples, 600);
        let every = |list: &[&str]| options(list).every.as_millis();
        assert_eq!((every(&["--every", "0"]), every(&["--every", "750"])), (500, 750));
        assert_eq!((every(&["--every", "0", "--stage", "all"]), every(&["--stage", "all", "--every", "250"])), (1000, 1000));
        assert_eq!(every(&["--every", "18446744073709551615"]), u64::MAX as u128);
        let wrong: [(&[&str], &str); 4] = [(&["--stage", "gpu"], "--stage is cpu or all, not gpu"),
                                           (&["--samples"], "--samples needs a value: wvplay pawnio ["),
                                           (&["--every", "soon"], "--every needs a number, not soon"),
                                           (&["--verbose"], "--verbose isn't an option: ")];
        for (wrong, error) in wrong {
            let e = Options::parse(&args(wrong)).unwrap_err();
            assert!(e.starts_with(error), "{e}");
        }
    }

    /// Without administrator rights it says so, as one line of JSON, and exits with 1, before it makes or
    /// opens anything; options it doesn't know are an error the same way.
    #[test]
    fn needs_admin() {
        let lists: [&[&str]; 2] = [&[], &["--stage", "all", "--compare-lhm", "http://127.0.0.1:8085", "--out", "probe.json"]];
        for list in lists {
            let mut printed = Vec::new();
            assert_eq!(main_as(&args(list), &mut printed, false), 1);
            assert_eq!(String::from_utf8(printed).unwrap(), "{\"error\":\"needs-admin\"}\n");
        }
        assert!(!Path::new("probe.json").exists());
        let mut printed = Vec::new();
        assert_eq!(main_as(&args(&["--stage", "gpu"]), &mut printed, false), 1);
        assert_eq!(serde_json::from_slice::<Json>(&printed).unwrap(), json!({ "error": "--stage is cpu or all, not gpu" }));
    }

    /// `--fake`: the probe on this PC's fakes, with every field the spec asks for (§6.4) holding what the
    /// fakes give, written to the file asked for, with a summary on the console.
    #[test]
    fn fake_mode() {
        let file = std::env::temp_dir().join(format!("wvplay-probe-{}.json", std::process::id()));
        let list = ["--fake", "--stage", "all", "--samples", "3", "--compare-lhm", "http://lhm.invalid", "--out", file.to_str().unwrap()];
        let mut printed = Vec::new();
        assert_eq!(main_as(&args(&list), &mut printed, false), 0);
        let report: Json = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        std::fs::remove_file(&file).unwrap();
        let summary: Json = serde_json::from_slice(&printed).unwrap();
        assert_eq!((&summary["out"], &summary["compare_not_ok"]), (&json!(file.display().to_string()), &json!([])));
        assert_eq!((&summary["keys"], &summary["notes"]), (&report["keys"], &json!([])));

        for field in ["board", "cpu", "pawnio", "modules", "mutexes", "cpu_raw", "superio", "chips", "counts", "after", "keys", "labels",
                      "compare", "timing", "notes", "readings", "tctl_vs_chips"] {
            assert!(!report[field].is_null(), "{field}");
        }
        assert_eq!(report["samples"], json!({ "asked": 3, "done": 3, "every_ms": 1000 }));
        assert_eq!((&report["stage"], &report["stopped"], &report["notes"]), (&json!("all"), &json!(false), &json!([])));
        assert_eq!((&report["board"]["BaseBoardProduct"], &report["board"]["mapped"]), (&json!("X870E AORUS ELITE X3D ICE"), &json!(true)));
        assert_eq!((&report["cpu"]["family"], &report["cpu"]["model"], &report["cpu"]["read"]), (&json!("0x1A"), &json!("0x44"), &json!(true)));
        assert_eq!(report["cpu"]["ccd_slots"], json!({ "offset": "0x308", "count": 8 }));
        assert_eq!(report["pawnio"]["driver_version"], "2.1.0");

        // the repo's module files, as pinned; LpcIO opened for each slot's detection, and again for each `after` read
        let modules = report["modules"].as_array().unwrap();
        assert!(modules.iter().all(|m| m["pinned_ok"] == true && m["sha256"].as_str().is_some_and(|s| s.len() == 64)));
        assert_eq!((&modules[0]["name"], &modules[0]["loads"]), (&json!("AMDFamily17"), &json!(["ok"])));
        assert_eq!((&modules[1]["name"], &modules[1]["loads"]), (&json!("LpcIO"), &json!(["ok", "ok", "ok", "ok"])));

        // each wait for a shared lock, by what waited: the reader, the dumps, the `after` check
        let mutexes = report["mutexes"].as_array().unwrap();
        let waits = |m: &Json| -> Vec<(String, u64)> {
            m["acquires"].as_array().unwrap().iter().map(|a| (a["phase"].as_str().unwrap().into(), a["timeout_ms"].as_u64().unwrap())).collect()
        };
        let each = |list: &[(&str, u64)]| -> Vec<(String, u64)> { list.iter().map(|&(p, t)| (p.to_string(), t)).collect() };
        assert_eq!((&mutexes[0]["name"], &mutexes[0]["state"]), (&json!(Bus::Pci.name()), &json!("created")));
        assert_eq!(waits(&mutexes[0]), each(&[("read", 10), ("dump", 10)].repeat(3)));
        assert_eq!(mutexes[1]["name"], Bus::Isa.name());
        let polls = each(&[("read", 10), ("read", 10), ("dump", 10), ("dump", 10)]);
        let (detection, after) = (each(&[("read", 100), ("read", 100)]), each(&[("after", 100), ("after", 100)]));
        assert_eq!(waits(&mutexes[1]), [detection, polls.clone(), polls.clone(), polls, after].concat());
        assert!(mutexes.iter().flat_map(|m| m["acquires"].as_array().unwrap()).all(|a| a["got"] == true && a["held_ms"].is_number()));

        // the CPU's registers: Tctl and all 8 slots in each sample, of which slots 0 and 1 are CCDs
        let cpu_raw = &report["cpu_raw"];
        assert_eq!((&cpu_raw["pwr_unit"], &cpu_raw["kept_ccds"]), (&json!({ "raw": "0x000A1000", "joules": 1.0 / 65_536.0 }), &json!([0, 1])));
        let samples = cpu_raw["samples"].as_array().unwrap();
        assert_eq!(samples.len(), 3);
        for sample in samples {
            assert_eq!(sample["tctl"], json!({ "raw": "0x793B0000", "c": 72.125 }));
            let ccds = sample["ccds"].as_array().unwrap();
            assert_eq!(ccds.len(), 8);
            assert_eq!(ccds[0], json!({ "slot": 0, "raw": "0x00000AC0", "valid": true, "c": 39.0 }));
            assert_eq!(ccds[1]["c"], 63.0);
            assert_eq!(ccds[2], json!({ "slot": 2, "raw": "0x00000000", "valid": false, "c": null }));
        }
        // LHM's answers moved the energy counter on by 100 J a second
        assert_eq!(samples.iter().map(|s| s["energy"]["w"].clone()).collect::<Vec<_>>(), [json!(null), json!(100.0), json!(100.0)]);

        // each detection attempt, 0x4E first: the IT87952E found in configuration mode, the IT8696E keyed and let go
        let superio = report["superio"].as_array().unwrap();
        assert_eq!(superio.iter().map(|s| s["port"].as_str().unwrap()).collect::<Vec<_>>(), ["0x4E", "0x2E"]);
        let (at_4e, at_2e) = (&superio[0]["attempts"][0], &superio[1]["attempts"][0]);
        assert_eq!(*at_4e, json!({ "t_ms": 0, "id_before": "0x8695", "key_sent": false, "id_after": null, "chip": "IT87952E",
                                   "ldn_before": "0x0F", "find_bars": "ok", "base": ["0x0A60", "0x0A60"], "ldn_restored": true,
                                   "exit_sent": false, "left_keyed": false, "outcome": "found IT87952E at 0xA60" }));
        assert_eq!((&at_2e["id_before"], &at_2e["key_sent"], &at_2e["id_after"], &at_2e["exit_sent"], &at_2e["outcome"]),
                   (&json!("0xFFFF"), &json!(true), &json!("0x8696"), &json!(true), &json!("found IT8696E at 0xA40")));

        // each chip's registers in each sample, decoded as the reader decodes them
        let chips = report["chips"].as_array().unwrap();
        let found: Vec<(&str, &str, &str, &str)> = chips.iter().map(|c| {
            (c["chip"].as_str().unwrap(), c["key"].as_str().unwrap(), c["port"].as_str().unwrap(), c["base"].as_str().unwrap())
        }).collect();
        assert_eq!(found, [("IT8696E", "ite_it8696e", "0x2E", "0x0A40"), ("IT87952E", "ite_it87952e", "0x4E", "0x0A60")]);
        assert_eq!(chips[0]["fans_read"], json!([true, true, true, true, true, false]));
        assert_eq!(chips[1]["fans_read"], json!([true, true, true]));
        let first = &chips[0]["regs"][0];
        assert_eq!(chips[0]["regs"].as_array().unwrap().len(), 3);
        assert_eq!(first["regs"].as_object().unwrap().len(), 45);
        assert_eq!((&first["regs"]["0x58"], &first["regs"]["0x13"], &first["regs"]["0x2B"]), (&json!("0x90"), &json!("0x77"), &json!("0x48")));
        assert_eq!((&first["bank"], &first["temperatures"]), (&json!([0, 0]), &json!([37.0, 47.0, 72.0, 44.0, 51.0, null])));
        assert_eq!(first["fans"][1], json!({ "fan": 2, "read": true, "count": "0x0229", "rpm": 1.35e6 / (2.0 * 553.0) }));
        assert_eq!(first["fans"][5], json!({ "fan": 6, "read": false, "count": "0x0000", "rpm": null }));
        // the IT87952E's PWM registers are its own 3 channels'
        let regs = chips[1]["regs"][0]["regs"].as_object().unwrap();
        assert!(regs.len() == 30 && regs.contains_key("0x63") && regs.contains_key("0x17"));
        assert!(!regs.contains_key("0x7F") && !regs.contains_key("0x7B"));
        let counts = |dump_reads: u32| {
            json!({ "read_back_failures": 0, "bank_guard_trips": 0, "vendor_misses": 0, "dump_reads": dump_reads, "dump_read_back_failures": 0 })
        };
        // each sample reads every register in the chip's list, and the bank again at the end
        assert_eq!(report["counts"], json!({ "ite_it8696e": counts(3 * 46), "ite_it87952e": counts(3 * 31) }));

        // both chips left as found
        assert_eq!(report["after"], json!([
            { "port": "0x4E", "found": { "id": "0x8695", "ldn": "0x0F" }, "id": "0x8695", "ldn": "0x0F", "as_found": true, "expected": true },
            { "port": "0x2E", "found": { "id": "0xFFFF", "ldn": "0x07" }, "id": "0xFFFF", "ldn": null, "as_found": true, "expected": true },
        ]));

        // what the pc source would publish, power from the second read on
        let keys = report["keys"].as_object().unwrap();
        assert_eq!((keys.len(), report["labels"].as_array().unwrap().len()), (21, 21));
        assert_eq!((&keys["pawnio.cpu.power.package"], &report["cpu_temp"]), (&json!(100.0), &json!(72.125)));
        assert_eq!(keys["pawnio.ite_it87952e.fan.fan_2"], 6250.0);
        // named from the board's map
        let label = |key: &str| report["labels"].as_array().unwrap().iter().find(|l| l["key"] == key).unwrap()["label"].clone();
        assert_eq!((label("pawnio.ite_it87952e.fan.fan_2"), label("pawnio.ite_it8696e.temperature.temperature_3")),
                   (json!("System Fan #6 / Pump"), json!("CPU")));
        assert!(!report["readings"][0]["values"].as_object().unwrap().contains_key("pawnio.cpu.power.package"));

        // beside LHM's, by chip and channel: all agree
        let compare = report["compare"].as_array().unwrap();
        assert_eq!(compare.len(), 21);
        assert!(compare.iter().all(|row| row["ok"] == true), "{compare:#?}");
        let row = |key: &str| compare.iter().find(|row| row["pawnio_key"] == key).unwrap();
        let pump = row("pawnio.ite_it87952e.fan.fan_2");
        assert_eq!((&pump["chip"], &pump["channel"], &pump["lhm_key"], &pump["lhm_id"]),
                   (&json!("IT87952E"), &json!(1), &json!("lhm.ite_it87952e.fan.fan_2"), &json!("/lpc/it87952e/0/fan/1")));
        assert_eq!((&pump["lhm"], &pump["samples"]), (&json!(6308.0), &json!(3)));
        assert_eq!(row("pawnio.cpu.temperature.tctl")["lhm_key"], "lhm.amd_ryzen_9_9950x3d.temperature.core_tctl_tdie");
        assert_eq!(row("pawnio.cpu.power.package")["samples"], 2);
        let nearest = json!({ "key": "pawnio.ite_it8696e.temperature.temperature_3", "median_diff": -0.125, "samples": 3 });
        assert_eq!(report["tctl_vs_chips"][0], nearest);

        let timing = &report["timing"];
        // the reader's Tctl and 8 slots at first, then Tctl and the 2 CCDs; the dump's Tctl and 8 slots each time
        assert_eq!(timing["ioctl_us"]["ioctl_read_smn"]["count"], 9 + 3 + 3 + 3 * 9);
        assert!(timing["ioctl_us"]["all"]["p99"].is_number());
        assert_eq!(timing["detection_ms"].as_array().unwrap().len(), 2);
        assert_eq!((&timing["chip_poll_ms"]["count"], &timing["cpu_temperatures_ms"]["count"], &timing["read_ms"]["rest"]["count"]),
                   (&json!(6), &json!(3), &json!(2)));
    }

    /// `--stage cpu` reads the CPU only: the chips' lock is neither made nor waited for, their module isn't
    /// opened, nothing is written to a port, and the reader's note that they're off isn't one here.
    #[test]
    fn stage_cpu_leaves_the_chips_alone() {
        let rig = Rig::new();
        let options = options(&["--samples", "2", "--compare-lhm", "http://lhm.invalid"]);
        let mut probe = rig.probe(&options);
        probe.run();
        let report = probe.report();
        assert!(rig.lpc.writes().is_empty() && rig.lpc.bus().opens == 0 && rig.isa.waits().is_empty());
        assert_eq!(report["notes"], json!([]));
        let keys = report["keys"].as_object().unwrap();
        assert!(keys.len() == 6 && keys.keys().all(|k| k.starts_with("pawnio.cpu.")), "{keys:?}");
        assert_eq!((&report["superio"], &report["after"]), (&Json::Null, &Json::Null));
        assert_eq!((&report["chips"], &report["counts"]), (&json!([]), &json!({})));
        assert_eq!((report["mutexes"].as_array().unwrap().len(), &report["modules"][1]["loads"]), (1, &json!([])));
        // compared on the CPU's readings only
        let compare = report["compare"].as_array().unwrap();
        assert!(compare.len() == 6 && compare.iter().all(|row| row["chip"] == "CPU" && row["ok"] == true), "{compare:#?}");
    }

    /// Beyond the reader's writes, the dumps write only a found chip's index (base+5), and only registers in
    /// that chip's list (its own PWM channels); the `after` check only the configuration index, for the chip
    /// ID and the logical device. Configuration data writes are the reader's (reg 0x07, and 0x02 to leave).
    #[test]
    fn dumps_write_only_indexes() {
        let rig = Rig::new();
        let options = options(&["--stage", "all", "--samples", "2"]);
        let mut probe = rig.probe(&options);
        probe.sample_all();
        let sampled = rig.lpc.writes();
        probe.check_after();
        let mut index = HashMap::new();
        for (n, &(port, value)) in sampled.iter().enumerate() {
            match port {
                0x2E | 0x4E => {
                    index.insert(port, value);
                }
                0x2F | 0x4F => assert!(matches!(index.get(&(port - 1)), Some(0x07 | 0x02)), "write {n}: {port:X} <- {value:X}"),
                0xA45 => assert!(registers(ite::chip_spec(0x8696).unwrap()).contains(&value), "write {n}: {value:X}"),
                0xA65 => assert!(registers(ite::chip_spec(0x8695).unwrap()).contains(&value), "write {n}: {value:X}"),
                _ => panic!("write {n}: {port:X} <- {value:X}"),
            }
        }
        // the dumps did read beyond the reader: the voltages, and the PWM registers of each chip's own count
        assert!(sampled.contains(&(0xA45, 0x7F)) && sampled.contains(&(0xA65, 0x20)) && !sampled.contains(&(0xA65, 0x7F)));
        assert_eq!(rig.lpc.writes()[sampled.len()..], [(0x4E, 0x20), (0x4E, 0x21), (0x4E, 0x07), (0x2E, 0x20), (0x2E, 0x21)]);
    }

    /// The registers dumped, in order: the bank first, a fan's low byte before its high byte, and the PWM
    /// registers of the chip's own channels.
    #[test]
    fn dumped_registers() {
        let it8696 = registers(ite::chip_spec(0x8696).unwrap());
        assert_eq!(it8696.len(), 5 + 6 + 2 * 6 + 10 + 2 * 6);
        assert_eq!(it8696[..13], [0x06, 0x00, 0x0C, 0x13, 0x58, 0x29, 0x2A, 0x2B, 0x2C, 0x2D, 0x2E, 0x0D, 0x18]);
        assert_eq!(it8696[it8696.len() - 12..], [0x15, 0x16, 0x17, 0x7F, 0xA7, 0xAF, 0x63, 0x6B, 0x73, 0x7B, 0xA3, 0xAB]);
        let it87952 = registers(ite::chip_spec(0x8695).unwrap());
        assert_eq!(it87952[it87952.len() - 6..], [0x15, 0x16, 0x17, 0x63, 0x6B, 0x73]);
        assert!(!it87952.contains(&0x80) && it87952.contains(&0x2F));
    }

    /// A chip not left as found shows: here the one at 0x4E has another logical device selected, and the one
    /// at 0x2E is still in configuration mode.
    #[test]
    fn after_shows_what_changed() {
        let rig = Rig::new();
        let options = options(&["--stage", "all", "--samples", "1"]);
        let mut probe = rig.probe(&options);
        probe.sample_all();
        {
            let mut bus = rig.lpc.bus();
            bus.chip(0x2E).configured = true;
            bus.chip(0x4E).ldn = 0x07;
        }
        probe.check_after();
        assert_eq!(probe.after, Some(vec![
            json!({ "port": "0x4E", "found": { "id": "0x8695", "ldn": "0x0F" }, "id": "0x8695", "ldn": "0x07", "as_found": false,
                    "expected": false }),
            json!({ "port": "0x2E", "found": { "id": "0xFFFF", "ldn": "0x07" }, "id": "0x8696", "ldn": "0x04", "as_found": false,
                    "expected": false }),
        ]));
    }

    /// A chip we put in configuration mode that mustn't be sent the exit (here the IT87952E at 0x4E, when
    /// the BIOS had left it out of that mode) isn't as found, by design: still in it, with the logical device
    /// it had, is what's expected. Another logical device isn't.
    #[test]
    fn after_noconf_is_by_design() {
        let rig = Rig::new();
        rig.lpc.bus().chip(0x4E).configured = false;
        let options = options(&["--stage", "all", "--samples", "1"]);
        let mut probe = rig.probe(&options);
        probe.sample_all();
        let attempt = &probe.reader.superio().attempts[0];
        assert!(attempt.key_sent && !attempt.exit_sent && attempt.id_after == Some(0x8695));
        probe.check_after();
        let by_design = "left in configuration mode: the chip mustn't be sent the exit (NOCONF)";
        assert_eq!(probe.after.as_ref().unwrap()[0], json!({ "port": "0x4E", "found": { "id": "0xFFFF", "ldn": "0x0F" }, "id": "0x8695",
                                                             "ldn": "0x0F", "as_found": false, "expected": true, "by_design": by_design }));
        rig.lpc.bus().chip(0x4E).ldn = 0x07;
        probe.check_after();
        let row = &probe.after.as_ref().unwrap()[0];
        assert_eq!((&row["as_found"], &row["expected"], &row["by_design"]), (&json!(false), &json!(false), &Json::Null));
    }

    /// Ctrl+C in the middle of a chip's dump (the process's `HwLock` asked to stop): the register being read
    /// is finished, nothing more is written, no later sample, LHM or `after` check, and what was read is still
    /// written, with a note, and the exit code 130.
    #[test]
    fn ctrl_c_stops_between_registers() {
        // the write that selects the first voltage register in the second sample's dump of the IT8696E
        let n = {
            let rig = Rig::new();
            let options = options(&["--stage", "all", "--samples", "2"]);
            rig.probe(&options).sample_all();
            rig.lpc.writes().iter().enumerate().filter(|(_, w)| **w == (0xA45, 0x20)).nth(1).unwrap().0 + 1
        };
        let rig = Rig::new();
        let hw = rig.hw.clone();
        rig.lpc.bus().after_write = Some((n, Box::new(move || hw.ask_to_stop())));
        let options = options(&["--stage", "all", "--samples", "5", "--compare-lhm", "http://lhm.invalid"]);
        let mut printed = Vec::new();
        assert_eq!(probe(&options, rig.machine(), rig.hw.clone(), &mut printed), STOPPED);
        assert_eq!(rig.lpc.writes().len(), n);
        let report: Json = serde_json::from_slice(&printed).unwrap();
        assert_eq!((&report["stopped"], &report["samples"]["done"], &report["after"]), (&json!(true), &json!(2), &Json::Null));
        assert_eq!(report["notes"], json!(["Stopped by Ctrl+C after 2 of 5 samples; the hardware sequence under way was finished first."]));
        let regs = report["chips"][0]["regs"][1]["regs"].as_object().unwrap();
        assert!(regs.contains_key("0x20") && !regs.contains_key("0x21"));
        assert_eq!(report["chips"][1]["regs"].as_array().unwrap().len(), 1);
        // LHM was asked after the first sample only
        let tctl = report["compare"].as_array().unwrap().iter().find(|row| row["pawnio_key"] == "pawnio.cpu.temperature.tctl").unwrap();
        assert_eq!(tctl["samples"], 1);
    }

    /// Ctrl+C's handler lets go of the hardware lock as soon as the sequence under way has finished, so the
    /// probe can still write what it has (`ctrl_c`): nothing touches the hardware after that, not even a
    /// sequence started past the probe's own checks, as `HwLock::hold` gives nothing once stopping.
    #[test]
    fn nothing_after_ctrl_c_lets_go() {
        let rig = Rig::new();
        let options = options(&["--stage", "all", "--samples", "3"]);
        let mut probe = rig.probe(&options);
        probe.sample();
        let calls = || (rig.amd.calls().len(), rig.lpc.calls().len());
        let before = calls();
        drop(rig.hw.stop());
        probe.sample();
        probe.dump_cpu(Duration::ZERO);
        probe.dump_chips(Duration::ZERO);
        probe.check_after();
        assert_eq!(calls(), before);
        assert_eq!((probe.samples.len(), probe.cpu_raw.len(), probe.chips[0].samples.len(), &probe.after), (2, 1, 1, &None));
    }

    /// Samples keep to their times; one that ends late (here LHM takes 5 s to answer) isn't caught up on:
    /// the next comes a whole `every` after it ended, never straight after.
    #[test]
    fn late_samples_are_not_caught_up() {
        let rig = Rig::new();
        let options = options(&["--samples", "4", "--compare-lhm", "http://lhm.invalid"]);
        let mut machine = rig.machine();
        let (clock, asked) = (rig.clock.clone(), Cell::new(0));
        machine.lhm = Box::new(move |_| {
            asked.set(asked.get() + 1);
            if asked.get() == 2 {
                clock.advance(Duration::from_secs(5));
            }
            Err("timed out".into())
        });
        let mut probe = Probe::new(&options, machine, rig.hw.clone());
        probe.run();
        let times: Vec<u128> = probe.samples.iter().map(|s| s.at.as_millis()).collect();
        assert_eq!(times, [0, 1000, 7000, 8000]);
    }

    /// A chip the reader found missing (its registers stopped answering) isn't dumped, but says so: its
    /// registers are left to the reader's tries, now and then (ite.rs).
    #[test]
    fn missing_chips_are_not_dumped() {
        let rig = Rig::new();
        let options = options(&["--stage", "all", "--samples", "5"]);
        let mut probe = rig.probe(&options);
        probe.sample();
        rig.lpc.bus().hwm(0xA40).regs[0x58] = 0;
        // three misses, the last of which makes it missing
        for _ in 0..3 {
            probe.sample();
        }
        // not tried by the reader this time, and not dumped
        let writes = rig.lpc.writes().len();
        probe.sample();
        assert!(rig.lpc.writes()[writes..].iter().all(|&(port, _)| port == 0xA65));
        let samples = &probe.chips[0].samples;
        assert_eq!((probe.chips[0].spec.name, samples.len()), ("IT8696E", 5));
        assert!(samples[..3].iter().all(|s| s["regs"].as_object().unwrap().len() == 45));
        assert!(samples[3..].iter().all(|s| s["missing"] == true && s.as_object().unwrap().len() == 2), "{samples:#?}");
        assert_eq!(probe.chips[1].samples.len(), 5);
    }

    /// A chip with banks is dumped only in bank 0, as the reader reads it: in another (another program
    /// switched it), the dump reads the bank and no further, and says so; also when the bank doesn't read
    /// back, which the reader then counts as a miss, not a switched bank.
    #[test]
    fn dumps_keep_to_bank_zero() {
        let rig = Rig::new();
        let options = options(&["--stage", "all", "--samples", "3"]);
        let mut probe = rig.probe(&options);
        probe.sample();
        rig.lpc.bus().hwm(0xA40).regs[0x06] = 0x20;
        let writes = rig.lpc.writes().len();
        probe.sample();
        // the reader's bank read, and the dump's
        let indexes: Vec<u8> = rig.lpc.writes()[writes..].iter().filter(|(port, _)| *port == 0xA45).map(|(_, reg)| *reg).collect();
        assert_eq!(indexes, [0x06, 0x06]);
        let skipped = &probe.chips[0].samples[1];
        assert_eq!((&skipped["bank"], &skipped["regs"], &skipped["skipped"]), (&json!([1, null]), &json!({ "0x06": "0x20" }), &json!("bank")));
        assert!(skipped["temperatures"].is_null() && skipped["fans"].is_null());
        // the IT87952E has no banks
        assert_eq!(probe.chips[1].samples[1]["regs"].as_object().unwrap().len(), 30);

        {
            let mut bus = rig.lpc.bus();
            bus.hwm(0xA40).regs[0x06] = 0;
            bus.hwm(0xA40).bad_readback = [0x06].into();
        }
        probe.sample();
        let skipped = &probe.chips[0].samples[2];
        assert_eq!((&skipped["bank"], &skipped["regs"], &skipped["skipped"]), (&json!([null, null]), &json!({ "0x06": null }), &json!("read_back")));
        // (the dump's read-back failures count among the chip's too)
        assert_eq!(probe.chips[0].counts, Counts { read_backs: 2, bank_trips: 1, misses: 1 });
    }

    /// Rows pair ours and LHM's by chip and channel, though LHM names channels from a board's map; each kind
    /// of reading has its tolerance; a channel only one side has isn't ok; a chip we don't read (a Nuvoton's),
    /// and LHM's other types, are left out.
    #[test]
    fn compare_by_channel() {
        let lhm = |id: &str, name: &str, value: f64| LhmReading { id: id.into(), name: name.into(), key: format!("lhm.{id}"), value };
        let theirs = [
            lhm("/lpc/it87952e/0/fan/1", "System Fan #6 / Pump", 6000.0),
            lhm("/lpc/it8696e/0/fan/0", "CPU Fan", 2200.0),
            lhm("/lpc/it8696e/0/fan/5", "Fan #6", 900.0),
            lhm("/lpc/it8696e/1/temperature/2", "CPU", 71.0),
            lhm("/lpc/it8696e/0/temperature/2", "CPU", 72.5),
            lhm("/lpc/nct6799d/0/fan/0", "Fan #1", 1000.0),
            lhm("/lpc/it87952e/0/control/1", "System Fan #6 / Pump", 100.0),
            lhm("/amdcpu/0/temperature/2", "Core (Tctl/Tdie)", 71.0),
            lhm("/amdcpu/0/temperature/3", "CCD1 (Tdie)", 52.0),
            lhm("/amdcpu/0/power/0", "Package", 120.0),
            lhm("/amdcpu/0/power/1", "Core #1 (SMU)", 10.0),
        ];
        let ours: Vec<(String, f64)> = [
            ("pawnio.ite_it87952e.fan.fan_2", 6308.4), ("pawnio.ite_it8696e.fan.fan_1", 2368.4),
            ("pawnio.ite_it8696e_2.temperature.temperature_3", 70.0), ("pawnio.ite_it8696e.temperature.temperature_3", 70.0),
            ("pawnio.cpu.temperature.tctl", 70.375), ("pawnio.cpu.temperature.ccd1", 49.625), ("pawnio.cpu.temperature.ccd2", 60.0),
            ("pawnio.cpu.power.package", 130.0),
        ].map(|(k, v)| (k.to_string(), v)).into();
        let mut rows = BTreeMap::new();
        add_rows(&mut rows, &ours, &theirs, true);
        let summed: Vec<(&str, bool, Option<f64>, Option<f64>)> = rows.iter().map(|(key, row)| {
            let row = row.json(key);
            (key.as_str(), row["ok"] == true, row["pawnio"].as_f64(), row["lhm"].as_f64())
        }).collect();
        assert_eq!(summed, [
            ("pawnio.cpu.power.package", true, Some(130.0), Some(120.0)),             // within 10 %
            ("pawnio.cpu.temperature.ccd1", false, Some(49.625), Some(52.0)),         // 2.375 °C apart
            ("pawnio.cpu.temperature.ccd2", false, Some(60.0), None),                 // ours only
            ("pawnio.cpu.temperature.tctl", true, Some(70.375), Some(71.0)),
            ("pawnio.ite_it8696e.fan.fan_1", false, Some(2368.4), Some(2200.0)),     // more than 5 % + 50 RPM apart
            ("pawnio.ite_it8696e.fan.fan_6", false, None, Some(900.0)),              // a fan we don't read
            ("pawnio.ite_it8696e.temperature.temperature_3", false, Some(70.0), Some(72.5)),
            ("pawnio.ite_it8696e_2.temperature.temperature_3", true, Some(70.0), Some(71.0)),
            ("pawnio.ite_it87952e.fan.fan_2", true, Some(6308.4), Some(6000.0)),     // within 5 % + 50 RPM
        ]);
        let second = rows["pawnio.ite_it8696e_2.temperature.temperature_3"].json("pawnio.ite_it8696e_2.temperature.temperature_3");
        assert_eq!((&second["chip"], &second["channel"], &second["lhm_id"], &second["diff"]),
                   (&json!("IT8696E #2"), &json!(2), &json!("/lpc/it8696e/1/temperature/2"), &json!(-1.0)));
        assert_eq!(rows["pawnio.cpu.temperature.ccd1"].json("pawnio.cpu.temperature.ccd1")["channel"], 0);
        // a second sample where they agree: the row counts both
        add_rows(&mut rows, &[("pawnio.cpu.temperature.ccd1".into(), 51.0)], &theirs, true);
        let ccd1 = rows["pawnio.cpu.temperature.ccd1"].json("pawnio.cpu.temperature.ccd1");
        assert_eq!((&ccd1["samples"], &ccd1["agreed"], &ccd1["ok"], &ccd1["pawnio"]), (&json!(2), &json!(1), &json!(false), &json!(51.0)));
        // without the chips, only the CPU's
        let mut cpu = BTreeMap::new();
        add_rows(&mut cpu, &ours[4..], &theirs, false);
        assert!(cpu.keys().all(|k| k.starts_with("pawnio.cpu.")) && cpu.len() == 4);
    }

    /// Our chip keys and LHM's ids, both ways; LHM's sensors read as the pc source reads them, with its keys.
    #[test]
    fn lhm_ids() {
        assert_eq!(lhm_id("pawnio.ite_it87952e.fan.fan_2").as_deref(), Some("/lpc/it87952e/0/fan/1"));
        assert_eq!(lhm_id("pawnio.ite_it8696e_2.temperature.temperature_6").as_deref(), Some("/lpc/it8696e/1/temperature/5"));
        assert_eq!(lhm_id("pawnio.cpu.temperature.tctl"), None);
        let tree = lhm_tree();
        let readings = lhm_readings(&tree);
        assert_eq!(readings.len(), 15 + 1 + 6);
        let pc: Vec<String> = crate::sources::lhm_values(&lhm_sensors(&tree)).into_iter().map(|(key, _)| key).collect();
        assert!(readings.iter().all(|r| pc.contains(&r.key)), "{readings:#?}");
        for reading in readings.iter().filter(|r| r.id.starts_with("/lpc/") && !r.id.contains("/control/")) {
            let key = our_key(reading).unwrap();
            assert_eq!(lhm_id(&key).as_deref(), Some(reading.id.as_str()));
            // on a board LHM has no map for, its key is ours with lhm. for pawnio. (keys.rs)
            assert_eq!(reading.key.replacen("lhm.", "pawnio.", 1), key);
        }
        let pump = readings.iter().find(|r| r.id == "/lpc/it87952e/0/fan/1").unwrap();
        assert_eq!((pump.key.as_str(), pump.name.as_str(), pump.value), ("lhm.ite_it87952e.fan.fan_2", "Fan #2", 6308.0));
        // a second chip of a name: `_2` in LHM's keys, the second of that name in its ids
        let fan = |id: &str, rpm: &str| json!({ "Text": "Fan #1", "SensorId": id, "Type": "Fan", "Value": rpm });
        let two = json!({ "Text": "Sensor", "Children": [
            { "Text": "ITE IT8696E", "HardwareId": "/lpc/it8696e/0", "Children": [fan("/lpc/it8696e/0/fan/0", "900 RPM")] },
            { "Text": "ITE IT8696E", "HardwareId": "/lpc/it8696e/1", "Children": [fan("/lpc/it8696e/1/fan/0", "800 RPM")] }]});
        let keys: Vec<(String, Option<String>)> = lhm_readings(&two).iter().map(|r| (r.key.clone(), our_key(r))).collect();
        assert_eq!(keys, [("lhm.ite_it8696e.fan.fan_1".to_string(), Some("pawnio.ite_it8696e.fan.fan_1".to_string())),
                          ("lhm.ite_it8696e_2.fan.fan_1".to_string(), Some("pawnio.ite_it8696e_2.fan.fan_1".to_string()))]);
    }

    /// LHM's data.json, from the address as given (as the studio has it, or with /data.json); what isn't
    /// sensor data, or isn't there, is an error the probe notes. A server of the test's own, never LHM's.
    #[test]
    fn lhm_over_http() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let replies = [(200, lhm_tree().to_string()), (200, lhm_tree().to_string()), (200, "<html>".to_string()), (404, String::new()),
                       (401, String::new())];
        let thread = std::thread::spawn(move || {
            let mut paths = Vec::new();
            for (status, body) in replies {
                let request = server.recv().unwrap();
                paths.push(request.url().to_string());
                request.respond(tiny_http::Response::from_string(body).with_status_code(status)).unwrap();
            }
            paths
        });
        let base = format!("http://127.0.0.1:{port}");
        assert_eq!(fetch_lhm(&base).unwrap(), lhm_tree());
        assert_eq!(fetch_lhm(&format!("127.0.0.1:{port}/data.json/")).unwrap(), lhm_tree());
        assert_eq!(fetch_lhm(&base).unwrap_err(), "its reply isn't sensor data");
        assert_eq!(fetch_lhm(&base).unwrap_err(), "HTTP 404");
        assert!(fetch_lhm(&base).unwrap_err().starts_with("HTTP 401: it asks for a user name and password"));
        assert_eq!(thread.join().unwrap(), ["/data.json"; 5]);
    }

    /// PawnIO's state, read without the device (which no test can open: driver.rs); and this process isn't
    /// an administrator (tests never are, spec §6.1).
    #[test]
    fn driver_state_and_rights() {
        let state = driver_state();
        assert_eq!((&state["open"], &state["driver_version"]), (&json!("Other(0)"), &Json::Null));
        assert!(state["service"].is_string() && (state["installed"].is_string() || state["installed"].is_null()), "{state}");
        assert_eq!(elevation::service_state("WvNoSuchServiceHere"), "not installed");
        assert!(!elevation::is_elevated(), "tests run without administrator rights (spec §6.1)");
    }

    /// The JSON's numbers: hex text with at least the digits asked for, times to the microsecond, and stats.
    #[test]
    fn numbers() {
        assert_eq!((hex(0x8696u16, 4), hex(0x0Au8, 2), hex(0xB15u32, 8), hex(0x1_0000_0000u64, 8)),
                   ("0x8696".into(), "0x0A".into(), "0x00000B15".into(), "0x100000000".into()));
        assert_eq!(ms(Duration::from_nanos(1_234_567)), 1.235);
        assert_eq!(stats(Vec::new()), json!({ "count": 0, "median": null, "p99": null, "max": null }));
        assert_eq!(stats((1..=100).rev().map(f64::from).collect()), json!({ "count": 100, "median": 50.5, "p99": 99.0, "max": 100.0 }));
        assert_eq!(stats(vec![3.0]), json!({ "count": 1, "median": 3.0, "p99": 3.0, "max": 3.0 }));
    }
}
