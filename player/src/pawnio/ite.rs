//! ITE Super I/O chips, through the LpcIO module: finding them, and leaving them as they were found, then
//! reading their temperatures and fan speeds at each poll.
//!
//! A chip is found through its configuration ports (0x2E/0x2F, 0x4E/0x4F), where it gives its ID and the
//! base of its environment controller; the readings are then that controller's registers (index at base+5,
//! data at base+6), read with no configuration access at all. The only writes (spec, Appendix A):
//! - ITE's key, `87 01 55 55` to 0x2E or `87 01 55 AA` to 0x4E, only when the ID reads 0xFFFF;
//! - the configuration index (the register to read), and the logical device (reg 0x07), which LpcIO's
//!   find_bars also sweeps through every number;
//! - leaving configuration mode (`02` to reg 0x02), only if we entered it and the chip isn't one that
//!   mustn't be (`noconf_family`); else the logical device is put back as found;
//! - the environment controller's index (base+5), once per register read.
//!
//! Never its data port, never a bank, fan control or PWM register, never another vendor's key.

use std::cell::RefCell;
use std::time::Duration;

use serde::Serialize;

use super::keys::{Channel, Sensor, hardware};
use super::mutex::Bus;
use super::reader::{HwLock, Poll, io_text, open_note};
use super::{Exec, IoError, Lock, ModuleId, OpenModule};

/// A supported chip.
#[derive(Debug, PartialEq, Eq)]
pub struct ChipSpec {
    pub id: u16,
    pub name: &'static str,
    pub temperatures: usize,
    pub fans: usize,
    pub pwms: usize,
    /// Its registers 0x10-0xAF are banked, and other programs switch the bank (Linux's FEAT_BANK_SEL): it's
    /// read only in bank 0, and never switched. The hardware probe never found this PC's switched, but
    /// Gigabyte's software may switch it at other times, and the check is one register read.
    pub bank_guard: bool,
    /// Doesn't read its index back: every read is taken as valid (LHM).
    pub readback_exempt: bool,
}

/// The chips read, with LHM's channel counts and Linux's flags. Only the IT8696E and IT87952E are tested on
/// hardware; the rest are LHM's same register layout.
pub const CHIPS: [ChipSpec; 6] = [
    ChipSpec { id: 0x8696, name: "IT8696E", temperatures: 6, fans: 6, pwms: 6, bank_guard: true, readback_exempt: false },
    ChipSpec { id: 0x8695, name: "IT87952E", temperatures: 3, fans: 3, pwms: 3, bank_guard: false, readback_exempt: false },
    ChipSpec { id: 0x8689, name: "IT8689E", temperatures: 6, fans: 6, pwms: 6, bank_guard: true, readback_exempt: false },
    ChipSpec { id: 0x8688, name: "IT8688E", temperatures: 6, fans: 6, pwms: 6, bank_guard: true, readback_exempt: true },
    // LHM reads a 7th temperature, which is VIN9 (Linux)
    ChipSpec { id: 0x8686, name: "IT8686E", temperatures: 6, fans: 6, pwms: 6, bank_guard: true, readback_exempt: false },
    ChipSpec { id: 0x8733, name: "IT8792E", temperatures: 3, fans: 3, pwms: 3, bank_guard: false, readback_exempt: false },
];

pub fn chip_spec(id: u16) -> Option<&'static ChipSpec> {
    CHIPS.iter().find(|c| c.id == id)
}

/// Chips where entering or leaving configuration mode is what hangs them (Linux's NOCONF): the IT8792E
/// (and the 0x8883 it can answer with once wrongly entered, LHM), the IT87952E, and the IT879x family
/// (the IT8790E is one; the rest, to be safe). Never sent the exit, at either port.
pub fn noconf_family(id: u16) -> bool {
    matches!(id, 0x8733 | 0x8695 | 0x8883) || id & 0xFFF0 == 0x8790
}

/// The configuration ports, by LpcIO's slot, and ITE's key for each.
pub const PORTS: [u16; 2] = [0x2E, 0x4E];
const KEYS: [[u8; 4]; 2] = [[0x87, 0x01, 0x55, 0x55], [0x87, 0x01, 0x55, 0xAA]];

/// Configuration registers: chip ID (two bytes), logical device, base (two bytes), and the one that ends
/// configuration mode.
pub(crate) const CHIP_ID: u8 = 0x20;
pub(crate) const LDN: u8 = 0x07;
const BASE: u8 = 0x60;
const CONFIG_CONTROL: u8 = 0x02;
/// The environment controller's logical device.
const HWM: u8 = 0x04;

/// Environment controller registers: configuration (bit 4 always set), bank (bits 6:5), fans 4-6 on (bits
/// 4, 5, 2), fans 1-3 on (bits 4-6), vendor (0x90 or 0x7F), the first temperature, and each fan's count.
pub(crate) const CONFIGURATION: u8 = 0x00;
pub(crate) const BANK: u8 = 0x06;
pub(crate) const FANS_4_TO_6: u8 = 0x0C;
pub(crate) const FANS_1_TO_3: u8 = 0x13;
pub(crate) const VENDOR: u8 = 0x58;
const VENDORS: [u8; 2] = [0x90, 0x7F];
pub(crate) const TEMPERATURE: u8 = 0x29;
pub(crate) const FAN_LOW: [u8; 6] = [0x0D, 0x0E, 0x0F, 0x80, 0x82, 0x4C];
pub(crate) const FAN_HIGH: [u8; 6] = [0x18, 0x19, 0x1A, 0x81, 0x83, 0x4D];

/// The shared lock's wait: for one detection attempt, and for one chip's poll.
pub(crate) const DETECT_MS: u32 = 100;
pub(crate) const POLL_MS: u32 = 10;
/// When a slot is tried again: after nothing answered once, and after each of the 3 retries.
const ABSENT_AGAIN: Duration = Duration::from_secs(2);
const RETRY_AFTER: [Duration; 3] = [Duration::from_secs(2), Duration::from_secs(10), Duration::from_secs(60)];
/// Polls in a row whose vendor check failed before a chip is missing; how long it's missing before it's
/// looked for again in configuration space, how long after a sleep that waits, how many times a reader
/// does it, and the longest wait between tries of a missing chip's registers.
const MISSES: u32 = 3;
const REDETECT_AFTER: Duration = Duration::from_secs(120);
const AFTER_RESUME: Duration = Duration::from_secs(30);
const REDETECTIONS: u32 = 2;
const LONGEST_WAIT: Duration = Duration::from_secs(300);
/// Polls in a row the lock was busy for, before a note.
const BUSY_NOTE: u32 = 3;

pub(crate) const BUSY: &str = "Another program kept the motherboard sensor chip busy, so some readings were skipped.";
pub(crate) const BANK_SWITCHED: &str =
    "Another program switched the motherboard sensor chip's register bank, so its readings were skipped this time.";
pub(crate) const NONE_FOUND: &str =
    "No supported motherboard sensor chip was found (only ITE chips are read so far), so there are no fan or board readings.";
/// The note for a slot whose chip didn't answer as expected at any try.
fn gave_up(port: u16) -> String {
    format!("The motherboard sensor chip at 0x{port:02X} didn't answer as expected, so its fans and temperatures are \
             missing until the player restarts.")
}

pub(crate) const STOPPED_ANSWERING: &str =
    "The motherboard sensor chip stopped answering, so fan and board readings are off until the player restarts.";

/// The note for a slot whose chip was sent the key, then couldn't be asked its ID (so it isn't sent the exit,
/// as it may be one that mustn't be).
fn left_keyed(port: u16) -> String {
    format!("The motherboard sensor chip at 0x{port:02X} didn't answer after it was put in configuration mode, so it may \
             stay in that mode until the PC restarts.")
}

/// How many times the ID is read after the key, before giving up on it.
const ID_TRIES: u32 = 3;

/// A fan's count: low byte | high byte << 8, in RPM. None when no fan is there (0x3F or less); 0 when it's
/// stopped or not connected (0xFFFF), which can't be told apart, and a stopped pump must show.
pub fn fan_rpm(count: u16) -> Option<f64> {
    match count {
        0..=0x3F => None,
        0xFFFF => Some(0.0),
        count => Some(1.35e6 / (2.0 * count as f64)),
    }
}

/// A temperature register, in °C: a signed byte, None outside 1 to 126 (no sensor there).
pub fn temperature(value: u8) -> Option<f64> {
    let t = value as i8;
    (1..=126).contains(&t).then_some(t as f64)
}

/// The module's functions, with the cells each takes and gives.
fn call(h: &dyn Exec, function: &str, input: &[u64]) -> Result<(), IoError> {
    h.execute(function, input, &mut [])
}

fn call1(h: &dyn Exec, function: &str, input: &[u64]) -> Result<u64, IoError> {
    let mut out = [0u64];
    h.execute(function, input, &mut out)?;
    Ok(out[0])
}

fn pio_outb(h: &dyn Exec, port: u16, value: u8) -> Result<(), IoError> {
    call(h, "ioctl_pio_outb", &[port as u64, value as u64])
}

fn pio_inb(h: &dyn Exec, port: u16) -> Result<u8, IoError> {
    call1(h, "ioctl_pio_inb", &[port as u64]).map(|v| v as u8)
}

fn superio_inb(h: &dyn Exec, reg: u8) -> Result<u8, IoError> {
    call1(h, "ioctl_superio_inb", &[reg as u64]).map(|v| v as u8)
}

/// Reads `reg` then `reg + 1`, high byte first.
fn superio_inw(h: &dyn Exec, reg: u8) -> Result<u16, IoError> {
    call1(h, "ioctl_superio_inw", &[reg as u64]).map(|v| v as u16)
}

fn superio_outb(h: &dyn Exec, reg: u8, value: u8) -> Result<(), IoError> {
    call(h, "ioctl_superio_outb", &[reg as u64, value as u64])
}

/// Why a detection attempt is to be tried again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// The process is ending: nothing more was done.
    Stopping,
    /// The module couldn't be opened: nothing was done.
    NoModule,
    /// Another program held the shared lock.
    Busy,
    /// The environment controller's logical device wouldn't be selected.
    Ldn,
    /// Its base didn't hold still, or can't be one.
    Base,
    /// Another program had switched the register bank.
    Bank,
    /// The registers at the base didn't answer as ITE's do.
    Vendor,
    /// An index didn't read back.
    ReadBack,
    Io(IoError),
}

/// What one detection attempt at one slot found.
pub enum Outcome {
    Found(Chip),
    /// Nothing answered.
    Absent,
    /// The chip at the other slot, answering at both.
    Duplicate,
    /// A chip that isn't in `CHIPS`.
    Unsupported(u16),
    Retry(Why),
}

impl Outcome {
    fn describe(&self) -> String {
        match self {
            Outcome::Found(chip) => format!("found {} at 0x{:X}", chip.spec.name, chip.base),
            Outcome::Absent => "absent".to_string(),
            Outcome::Duplicate => "duplicate".to_string(),
            Outcome::Unsupported(id) => format!("unsupported 0x{id:04X}"),
            Outcome::Retry(why) => format!("retry: {why:?}"),
        }
    }
}

/// One detection attempt, step by step, for the probe.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Attempt {
    pub port: u16,
    /// When, on the reader's clock, in ms.
    pub at_ms: u64,
    pub id_before: Option<u16>,
    pub key_sent: bool,
    pub id_after: Option<u16>,
    pub chip: Option<&'static str>,
    pub ldn_before: Option<u8>,
    /// "ok", or the error find_bars gave.
    pub find_bars: Option<String>,
    /// The base, read twice.
    pub base: Vec<u16>,
    pub ldn_restored: bool,
    pub exit_sent: bool,
    /// The key was sent but the ID couldn't be read after it, even on a second and third try: the chip may
    /// still be in configuration mode, as it isn't sent the exit without knowing what it is.
    pub left_keyed: bool,
    pub outcome: String,
}

/// Counts over a chip's polls, for the probe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// Registers whose index didn't read back.
    pub read_backs: u32,
    /// Polls the bank was switched for.
    pub bank_trips: u32,
    /// Polls whose vendor check failed.
    pub misses: u32,
}

/// What we changed in a chip's configuration space, so that it's put back however the attempt ends: an
/// early return, the process stopping, or a panic. If we entered configuration mode and the chip isn't
/// one that mustn't leave it, it leaves; else, if we changed the logical device, it's put back.
struct Session<'a> {
    h: &'a dyn Exec,
    port: u16,
    keyed: bool,
    answered: Option<u16>,
    ldn: Option<u8>,
    ldn_changed: bool,
    record: &'a RefCell<Attempt>,
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        let mut record = self.record.try_borrow_mut().ok();
        if self.keyed && self.answered.is_some_and(|id| !noconf_family(id)) {
            let left = pio_outb(self.h, self.port, CONFIG_CONTROL).and_then(|()| pio_outb(self.h, self.port + 1, 0x02));
            if let Some(record) = &mut record {
                record.exit_sent = left.is_ok();
            }
        } else if let (Some(ldn), true) = (self.ldn, self.ldn_changed) {
            let put_back = superio_outb(self.h, LDN, ldn);
            if let Some(record) = &mut record {
                record.ldn_restored = put_back.is_ok();
            }
        }
    }
}

/// Finds the chip at `slot` through its configuration space, and leaves that as it was found: its spec and
/// its environment controller's base, or the outcome. The configuration space is left before it returns.
fn configure(h: &dyn Exec, slot: usize, poll: &Poll, record: &RefCell<Attempt>) -> Result<(&'static ChipSpec, u16), Outcome> {
    let (io, hw) = (|e| Outcome::Retry(Why::Io(e)), poll.hw);
    let port = PORTS[slot];
    call(h, "ioctl_select_slot", &[slot as u64]).map_err(io)?;
    // the ID is read before anything that could change a mode
    let mut id = superio_inw(h, CHIP_ID).map_err(io)?;
    record.borrow_mut().id_before = Some(id);
    if hw.stopping() {
        return Err(Outcome::Retry(Why::Stopping));
    }
    let mut session = Session { h, port, keyed: false, answered: None, ldn: None, ldn_changed: false, record };
    if id == 0xFFFF {
        // the key, then the ID straight after, whatever happens (a key byte that failed may still have gone
        // through): once keyed, whether to leave depends on it
        session.keyed = true;
        let key = KEYS[slot].into_iter().try_for_each(|byte| pio_outb(h, port, byte));
        record.borrow_mut().key_sent = key.is_ok();
        id = match id_after_key(h, poll) {
            Ok(id) => id,
            // it can't be sent the exit without knowing what it is, as it may be one that mustn't be
            Err(e) => {
                record.borrow_mut().left_keyed = true;
                return Err(io(e));
            }
        };
        record.borrow_mut().id_after = Some(id);
        if id == 0xFFFF {
            key.map_err(io)?;
            return Err(Outcome::Absent);
        }
        if id != 0 {
            session.answered = Some(id);
        }
        key.map_err(io)?;
    }
    if id == 0 {
        return Err(Outcome::Absent);
    }
    session.answered = Some(id);
    let Some(spec) = chip_spec(id) else { return Err(Outcome::Unsupported(id)) };
    record.borrow_mut().chip = Some(spec.name);
    let ldn = superio_inb(h, LDN).map_err(io)?;
    session.ldn = Some(ldn);
    record.borrow_mut().ldn_before = Some(ldn);
    if hw.stopping() {
        return Err(Outcome::Retry(Why::Stopping));
    }
    session.ldn_changed = true;
    let bars = call(h, "ioctl_find_bars", &[]);
    record.borrow_mut().find_bars = Some(bars.map_or_else(io_text, |()| "ok".to_string()));
    bars.map_err(io)?;
    if hw.stopping() {
        return Err(Outcome::Retry(Why::Stopping));
    }
    superio_outb(h, LDN, HWM).map_err(io)?;
    if superio_inb(h, LDN).map_err(io)? != HWM {
        return Err(Outcome::Retry(Why::Ldn));
    }
    let base = superio_inw(h, BASE).map_err(io)?;
    poll.clock.sleep(Duration::from_millis(1));
    let again = superio_inw(h, BASE).map_err(io)?;
    record.borrow_mut().base = vec![base, again];
    drop(session);
    if base != again || base < 0x100 || base & 0xF007 != 0 {
        return Err(Outcome::Retry(Why::Base));
    }
    Ok((spec, base))
}

/// The chip ID, just after the key: read again, a millisecond apart, if it can't be read, as a chip left in
/// configuration mode can be sent the exit only once it's known not to be one that mustn't be.
fn id_after_key(h: &dyn Exec, poll: &Poll) -> Result<u16, IoError> {
    let mut tries = 1;
    loop {
        match superio_inw(h, CHIP_ID) {
            Err(_) if tries < ID_TRIES => {
                tries += 1;
                poll.clock.sleep(Duration::from_millis(1));
            }
            read => return read,
        }
    }
}

/// One detection attempt at `slot`, on a module handle of its own: through the configuration space, then
/// the environment controller's registers. `taken` are the bases of chips already kept.
fn detect(h: Box<dyn Exec>, slot: usize, poll: &Poll, taken: &[u16], record: &RefCell<Attempt>) -> Outcome {
    let (spec, base) = match configure(&*h, slot, poll, record) {
        Ok(found) => found,
        Err(outcome) => return outcome,
    };
    if poll.hw.stopping() {
        return Outcome::Retry(Why::Stopping);
    }
    if taken.contains(&base) {
        return Outcome::Duplicate;
    }
    let mut chip = Chip { h, slot, spec, base, fans: [false; 6], counts: Counts::default(), misses: 0, missing: None, busy: 0 };
    match chip.validate() {
        Ok(None) => Outcome::Found(chip),
        Ok(Some(why)) => Outcome::Retry(why),
        Err(e) => Outcome::Retry(Why::Io(e)),
    }
}

/// A chip that's missing: its registers stopped answering.
#[derive(Clone, Copy, Debug)]
struct Missing {
    since: Duration,
    /// Polls until its registers are tried again, the number that doubles each time, and the last try.
    polls_left: u32,
    gap: u32,
    tried: Duration,
}

/// A found chip, on its own module handle, whose ports are allowed for its base.
pub struct Chip {
    h: Box<dyn Exec>,
    pub slot: usize,
    pub spec: &'static ChipSpec,
    pub base: u16,
    /// Which fan inputs are on.
    pub fans: [bool; 6],
    pub counts: Counts,
    misses: u32,
    missing: Option<Missing>,
    busy: u32,
}

/// One poll's reading of a chip.
enum Sample {
    Read(Vec<(Channel, f64)>),
    /// The vendor check failed, or the bank couldn't be read: its registers don't answer as ITE's do.
    Miss,
    /// Another program had the bank switched.
    Bank,
    Stopped,
}

impl Sample {
    /// A sample the bank guard stopped: skipped for a switched bank; a miss when the bank didn't read back.
    fn guarded(why: Why) -> Sample {
        if why == Why::Bank { Sample::Bank } else { Sample::Miss }
    }
}

impl Chip {
    /// Whether its registers stopped answering (`poll_chip`): they're then tried only now and then, and
    /// nothing else should read them.
    pub fn is_missing(&self) -> bool {
        self.missing.is_some()
    }

    /// One environment controller register, or None when its index didn't read back (another program's
    /// index got in between).
    pub fn read_reg(&mut self, reg: u8) -> Result<Option<u8>, IoError> {
        pio_outb(&*self.h, self.base + 5, reg)?;
        let value = pio_inb(&*self.h, self.base + 6)?;
        let index = pio_inb(&*self.h, self.base + 5)?;
        if index == reg || self.spec.readback_exempt {
            Ok(Some(value))
        } else {
            self.counts.read_backs += 1;
            Ok(None)
        }
    }

    /// The register bank (reg 0x06 bits 6:5), never switched here; None if it didn't read back.
    fn bank(&mut self) -> Result<Option<u8>, IoError> {
        Ok(self.read_reg(BANK)?.map(|v| (v >> 5) & 3))
    }

    /// The bank guard, for a chip that has banks: None in bank 0; `Bank` in another; `ReadBack` when reg 0x06
    /// didn't read back (another program's index got in between, or nothing answers at the base any more),
    /// which isn't taken for a switched bank. None always, for a chip without banks.
    fn bank_guard(&mut self) -> Result<Option<Why>, IoError> {
        if !self.spec.bank_guard {
            return Ok(None);
        }
        Ok(match self.bank()? {
            Some(0) => None,
            Some(_) => Some(Why::Bank),
            None => Some(Why::ReadBack),
        })
    }

    /// Checks that ITE's environment controller answers at the base, and which fans are on. Some(why) when
    /// it doesn't.
    fn validate(&mut self) -> Result<Option<Why>, IoError> {
        if let Some(why) = self.bank_guard()? {
            return Ok(Some(why));
        }
        let vendor = self.read_reg(VENDOR)?;
        let configuration = self.read_reg(CONFIGURATION)?;
        let (Some(vendor), Some(configuration)) = (vendor, configuration) else { return Ok(Some(Why::ReadBack)) };
        if !VENDORS.contains(&vendor) || configuration & 0x10 == 0 {
            return Ok(Some(Why::Vendor));
        }
        // read once here and never written: a fan input that's off can hold an old count. LHM doesn't look at
        // fans 1-3's bits (Linux does); on this PC they hide no fan LHM shows (0x77, the hardware probe)
        let Some(main) = self.read_reg(FANS_1_TO_3)? else { return Ok(Some(Why::ReadBack)) };
        for (i, on) in self.fans.iter_mut().enumerate().take(self.spec.fans.min(3)) {
            *on = main & (1 << (4 + i)) != 0;
        }
        if self.spec.fans >= 5 {
            let Some(more) = self.read_reg(FANS_4_TO_6)? else { return Ok(Some(Why::ReadBack)) };
            for (i, bit) in [4, 5, 2].into_iter().enumerate().take(self.spec.fans - 3) {
                self.fans[3 + i] = more & (1 << bit) != 0;
            }
        }
        self.bank_guard()
    }

    /// One poll's readings: in bank 0 (checked before and after), with the vendor check first. A register
    /// whose index didn't read back is left out. Readings aren't held back to filter out spikes: the hardware
    /// probe saw none (no read-back failure, bank switch or vendor miss in 60 polls), and a filter would delay
    /// real changes by a poll.
    fn sample(&mut self, hw: &HwLock) -> Result<Sample, IoError> {
        if let Some(why) = self.bank_guard()? {
            return Ok(Sample::guarded(why));
        }
        if !self.read_reg(VENDOR)?.is_some_and(|v| VENDORS.contains(&v)) {
            return Ok(Sample::Miss);
        }
        let mut read = Vec::new();
        for i in 0..self.spec.temperatures {
            if hw.stopping() {
                return Ok(Sample::Stopped);
            }
            if let Some(t) = self.read_reg(TEMPERATURE + i as u8)?.and_then(temperature) {
                read.push((Channel::Temperature(i), t));
            }
        }
        let fans = self.fans;
        for i in (0..self.spec.fans).filter(|&i| fans[i]) {
            if hw.stopping() {
                return Ok(Sample::Stopped);
            }
            // the low byte first, as LHM and Linux do: reading it latches the high byte, so the two halves
            // are of the same count
            let Some(low) = self.read_reg(FAN_LOW[i])? else { continue };
            let Some(high) = self.read_reg(FAN_HIGH[i])? else { continue };
            if let Some(rpm) = fan_rpm(low as u16 | (high as u16) << 8) {
                read.push((Channel::Fan(i), rpm));
            }
        }
        if let Some(why) = self.bank_guard()? {
            return Ok(Sample::guarded(why));
        }
        Ok(Sample::Read(read))
    }
}

/// Where one slot is at.
enum Slot {
    /// Not found yet: tried again from `next` on.
    Looking { next: Duration, absent: u32, retries: u32 },
    Kept(Chip),
    /// Nothing more to do; its note, if it has one, is given at every read.
    Done { note: Option<String>, found: bool },
}

/// The Super I/O chips: found on the first read, then read at each.
pub struct SuperIo {
    slots: [Slot; 2],
    /// The name of the chip each slot ever had, for its keys.
    names: [Option<&'static str>; 2],
    /// Why the module couldn't be opened: then nothing is read.
    unopened: Option<String>,
    redetections: u32,
    /// The slots whose chip may have been left in configuration mode (`Attempt::left_keyed`): a note at
    /// every read.
    left_keyed: [bool; 2],
    /// Every detection attempt, for the probe.
    pub attempts: Vec<Attempt>,
}

impl Default for SuperIo {
    fn default() -> SuperIo {
        let looking = || Slot::Looking { next: Duration::ZERO, absent: 0, retries: 0 };
        SuperIo { slots: [looking(), looking()], names: [None; 2], unopened: None, redetections: 0, left_keyed: [false; 2],
                  attempts: Vec::new() }
    }
}

impl SuperIo {
    /// The chips found and still read.
    pub fn chips(&self) -> impl Iterator<Item = &Chip> {
        self.slots.iter().filter_map(|s| if let Slot::Kept(chip) = s { Some(chip) } else { None })
    }

    /// The chips found and still read, to read more of (the probe).
    pub fn chips_mut(&mut self) -> impl Iterator<Item = &mut Chip> {
        self.slots.iter_mut().filter_map(|s| if let Slot::Kept(chip) = s { Some(chip) } else { None })
    }

    /// One read: detection when it's due, then each chip found, under `isa` (the shared lock).
    pub fn read(&mut self, poll: &Poll, open: &OpenModule, isa: Option<&dyn Lock>, out: &mut Vec<(Sensor, f64)>,
                notes: &mut Vec<String>) {
        let Some(isa) = isa else { return notes.push(Bus::Isa.unshared()) };
        if let Some(note) = &self.unopened {
            return notes.push(note.clone());
        }
        // 0x4E first: reading a valid ID there changes nothing
        for slot in [1, 0] {
            self.find(slot, poll, open, isa);
            if self.unopened.is_some() {
                return notes.extend(self.unopened.clone());
            }
        }
        let names: Vec<&str> = self.names.iter().flatten().copied().collect();
        let keys = hardware(&names);
        let key = |slot: usize| keys[self.names[..slot].iter().flatten().count()].clone();
        for slot in [0, 1] {
            let Slot::Kept(chip) = &mut self.slots[slot] else { continue };
            for (channel, value) in poll_chip(chip, poll, isa, notes) {
                out.push((Sensor::Chip { chip: chip.spec.name, hardware: key(slot), channel }, value));
            }
        }
        for slot in &self.slots {
            if let Slot::Done { note: Some(note), .. } = slot {
                notes.push(note.clone());
            }
        }
        for slot in [1, 0].into_iter().filter(|&slot| self.left_keyed[slot]) {
            notes.push(left_keyed(PORTS[slot]));
        }
        if self.slots.iter().all(|s| matches!(s, Slot::Done { found: false, .. })) {
            notes.push(NONE_FOUND.to_string());
        }
    }

    /// Detection at one slot when it's due: the first, its retries, and a missing chip's re-detection.
    fn find(&mut self, slot: usize, poll: &Poll, open: &OpenModule, isa: &dyn Lock) {
        let port = PORTS[slot];
        match &mut self.slots[slot] {
            Slot::Looking { next, absent, retries } if poll.now >= *next => {
                let (absent, retries) = (*absent, *retries);
                let outcome = self.attempt(slot, poll, open, isa);
                self.slots[slot] = match outcome {
                    Outcome::Found(chip) => {
                        self.names[slot] = Some(chip.spec.name);
                        Slot::Kept(chip)
                    }
                    Outcome::Absent if absent == 0 => Slot::Looking { next: poll.now + ABSENT_AGAIN, absent: 1, retries },
                    Outcome::Absent => Slot::Done { note: None, found: false },
                    Outcome::Duplicate => Slot::Done {
                        note: Some(format!("The motherboard sensor chips at 0x{:02X} and 0x{port:02X} are the same one, so it's read once.",
                                           PORTS[1 - slot])),
                        found: false,
                    },
                    Outcome::Unsupported(id) => Slot::Done {
                        note: Some(format!("Found Super I/O chip 0x{id:04X} at 0x{port:02X}, which isn't supported yet.")),
                        found: false,
                    },
                    // the process is ending, or nothing can be read: as it was
                    Outcome::Retry(Why::Stopping | Why::NoModule) => Slot::Looking { next: poll.now, absent, retries },
                    Outcome::Retry(_) if (retries as usize) < RETRY_AFTER.len() => {
                        Slot::Looking { next: poll.now + RETRY_AFTER[retries as usize], absent, retries: retries + 1 }
                    }
                    Outcome::Retry(_) => Slot::Done { note: Some(gave_up(port)), found: false },
                };
            }
            Slot::Kept(chip) => {
                let Some(missing) = chip.missing else { return };
                let resumed = poll.resumed_at.is_some_and(|at| poll.now.saturating_sub(at) < AFTER_RESUME);
                if poll.now.saturating_sub(missing.since) < REDETECT_AFTER || resumed {
                    return;
                }
                if self.redetections >= REDETECTIONS {
                    self.slots[slot] = Slot::Done { note: Some(STOPPED_ANSWERING.to_string()), found: true };
                    return;
                }
                let outcome = self.attempt(slot, poll, open, isa);
                // one of the few only if it ran: not when another program held the lock, or nothing could
                if !matches!(outcome, Outcome::Retry(Why::Busy | Why::NoModule | Why::Stopping)) {
                    self.redetections += 1;
                }
                match outcome {
                    Outcome::Found(chip) => self.slots[slot] = Slot::Kept(chip),
                    // still missing: read through the old handle as before, and looked for again later
                    _ => {
                        if let Slot::Kept(chip) = &mut self.slots[slot]
                            && let Some(missing) = &mut chip.missing
                        {
                            missing.since = poll.now;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// One detection attempt at `slot`, on a new module handle, under the shared lock (100 ms) and the
    /// process's hardware lock. Recorded in `attempts`.
    fn attempt(&mut self, slot: usize, poll: &Poll, open: &OpenModule, isa: &dyn Lock) -> Outcome {
        let record = RefCell::new(Attempt { port: PORTS[slot], at_ms: poll.now.as_millis() as u64, ..Attempt::default() });
        let other = match &self.slots[1 - slot] {
            Slot::Kept(chip) => vec![chip.base],
            _ => Vec::new(),
        };
        let outcome = match open(ModuleId::LpcIo) {
            Err(e) => {
                self.unopened = Some(open_note(ModuleId::LpcIo, e));
                Outcome::Retry(Why::NoModule)
            }
            Ok(h) => match isa.acquire(DETECT_MS) {
                None => Outcome::Retry(Why::Busy),
                Some(_shared) => match poll.hw.hold() {
                    Some(_hw) => detect(h, slot, poll, &other, &record),
                    None => Outcome::Retry(Why::Stopping),
                },
            },
        };
        let mut record = record.into_inner();
        self.left_keyed[slot] |= record.left_keyed;
        record.outcome = outcome.describe();
        self.attempts.push(record);
        outcome
    }
}

/// Adds a note, unless it's there already (two chips can give the same).
fn note(notes: &mut Vec<String>, text: &str) {
    if !notes.iter().any(|n| n == text) {
        notes.push(text.to_string());
    }
}

/// One chip's poll, under the shared lock (10 ms) and the process's hardware lock: its readings, or none
/// when it's skipped this time. A missing chip's registers are only tried now and then.
fn poll_chip(chip: &mut Chip, poll: &Poll, isa: &dyn Lock, notes: &mut Vec<String>) -> Vec<(Channel, f64)> {
    if let Some(missing) = &mut chip.missing {
        missing.polls_left = missing.polls_left.saturating_sub(1);
        if missing.polls_left > 0 && poll.now.saturating_sub(missing.tried) < LONGEST_WAIT {
            return Vec::new();
        }
    }
    let Some(_shared) = isa.acquire(POLL_MS) else {
        chip.busy += 1;
        if chip.busy >= BUSY_NOTE {
            note(notes, BUSY);
        }
        return Vec::new();
    };
    chip.busy = 0;
    let Some(_hw) = poll.hw.hold() else { return Vec::new() };
    match chip.sample(poll.hw) {
        Ok(Sample::Read(read)) => {
            chip.misses = 0;
            chip.missing = None;
            read
        }
        Ok(Sample::Bank) => {
            chip.counts.bank_trips += 1;
            note(notes, BANK_SWITCHED);
            Vec::new()
        }
        Ok(Sample::Stopped) => Vec::new(),
        Ok(Sample::Miss) | Err(_) => {
            chip.counts.misses += 1;
            chip.misses += 1;
            match &mut chip.missing {
                Some(missing) => {
                    missing.gap *= 2;
                    missing.polls_left = missing.gap;
                    missing.tried = poll.now;
                }
                None if chip.misses >= MISSES => {
                    chip.missing = Some(Missing { since: poll.now, polls_left: 2, gap: 2, tried: poll.now });
                }
                None => {}
            }
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::pawnio::fake::{self, FakeChip, FakeClock, FakeHwm, FakeLock, FakeLpc};
    use crate::pawnio::reader::Clock;

    /// A Super I/O reader on a fake bus, with a lock and a clock the test controls.
    struct Rig {
        lpc: FakeLpc,
        superio: SuperIo,
        open: OpenModule,
        lock: FakeLock,
        clock: FakeClock,
        hw: Arc<HwLock>,
        resumed_at: Option<Duration>,
    }

    impl Rig {
        fn new(lpc: FakeLpc) -> Rig {
            Rig { open: fake::opener(Err(crate::pawnio::OpenError::NotSupported), Ok(lpc.clone())), lpc,
                  superio: SuperIo::default(), lock: FakeLock::default(), clock: FakeClock::default(), hw: Arc::default(),
                  resumed_at: None }
        }

        fn read(&mut self) -> (Vec<(String, f64)>, Vec<String>) {
            let poll = Poll { hw: &self.hw, clock: &self.clock, now: self.clock.since_boot(), resumed_at: self.resumed_at };
            let (mut out, mut notes) = (Vec::new(), Vec::new());
            self.superio.read(&poll, &self.open, Some(&self.lock), &mut out, &mut notes);
            (out.into_iter().map(|(s, v)| (s.key(), v)).collect(), notes)
        }

        fn keys(&mut self) -> Vec<String> {
            self.read().0.into_iter().map(|(k, _)| k).collect()
        }

        fn later(&mut self, secs: u64) -> &mut Rig {
            self.clock.advance(Duration::from_secs(secs));
            self
        }

        fn outcomes(&self) -> Vec<String> {
            self.superio.attempts.iter().map(|a| a.outcome.clone()).collect()
        }
    }

    /// One chip at `port`, with an environment controller at `base` that answers as ITE's do, with three
    /// fans on and temperatures 40 and 50.
    fn one(port: u16, id: u16, base: u16) -> (FakeChip, FakeHwm) {
        let mut hwm = FakeHwm::new(base);
        hwm.regs[0x58] = 0x90;
        hwm.regs[0x00] = 0x10;
        hwm.regs[0x13] = 0x70;
        hwm.regs[0x29] = 40;
        hwm.regs[0x2A] = 50;
        hwm.regs[0x0D] = 107;
        (FakeChip::new(port, id, base), hwm)
    }

    fn bus(chips: Vec<(FakeChip, FakeHwm)>) -> FakeLpc {
        let (chips, hwms) = chips.into_iter().unzip();
        FakeLpc::new(chips, hwms)
    }

    /// The port-level rules every case keeps (spec, write-set tests):
    /// (a) nothing written to a base's data port (base+6) or base+0..4 and base+7, ever;
    /// (b) configuration data writes only to reg 0x07 (the logical device) and reg 0x02 (leaving, with 02);
    /// (c) after detection (from write `polls_from`), writes only to a base+5.
    fn check_writes(writes: &[(u16, u8)], bases: &[u16], polls_from: usize) {
        let mut index = std::collections::HashMap::new();
        for (n, &(port, value)) in writes.iter().enumerate() {
            for &base in bases {
                assert!(!(base..base + 8).contains(&port) || port == base + 5, "write {n}: {port:X} <- {value:X}");
            }
            if PORTS.contains(&port) {
                index.insert(port, value);
            } else if PORTS.contains(&port.wrapping_sub(1)) {
                match index.get(&(port - 1)).copied() {
                    Some(0x07) => {}
                    Some(0x02) => assert_eq!(value, 0x02, "write {n}"),
                    other => panic!("write {n}: config data {port:X} <- {value:X} at reg {other:X?}"),
                }
            } else {
                assert!(bases.iter().any(|b| port == b + 5), "write {n}: {port:X} <- {value:X}");
            }
            if n >= polls_from {
                assert!(bases.iter().any(|b| port == b + 5), "poll write {n}: {port:X} <- {value:X}");
            }
        }
    }

    /// The writes that key, and that leave, at `port`.
    fn keys_and_exits(writes: &[(u16, u8)], port: u16) -> (usize, usize) {
        let key = [0x87, 0x01, 0x55, if port == 0x4E { 0xAA } else { 0x55 }];
        let keyed = writes.windows(4).filter(|w| w.iter().copied().eq(key.iter().map(|&b| (port, b)))).count();
        let exits = writes.windows(2).filter(|w| w[0] == (port, 0x02) && w[1] == (port + 1, 0x02)).count();
        (keyed, exits)
    }

    /// The decodes: fan counts, temperatures.
    #[test]
    fn decodes() {
        assert!((fan_rpm(107).unwrap() - 6308.4).abs() < 0.05);
        assert!((fan_rpm(285).unwrap() - 2368.4).abs() < 0.05);
        assert_eq!(fan_rpm(0xFFFF), Some(0.0));
        assert_eq!(fan_rpm(0x3F), None);
        assert_eq!(fan_rpm(0), None);
        assert!(fan_rpm(0x40).is_some());
        assert_eq!(temperature(0x46), Some(70.0));
        for hidden in [0x80, 0x7F, 0x00, 0xFF] {
            assert_eq!(temperature(hidden), None, "{hidden:X}");
        }
        assert_eq!(temperature(0x7E), Some(126.0));
        assert_eq!(temperature(0x01), Some(1.0));
        // as the hardware probe read them: the pump's count, and an absent sensor's register
        assert_eq!(fan_rpm(0x6C), Some(6250.0));
        assert_eq!(temperature(0xC9), None);
    }

    /// The chip table, and the chips never sent the exit.
    #[test]
    fn chips() {
        assert_eq!(chip_spec(0x8696).map(|c| (c.temperatures, c.fans, c.bank_guard)), Some((6, 6, true)));
        assert_eq!(chip_spec(0x8695).map(|c| (c.name, c.temperatures, c.fans, c.bank_guard)), Some(("IT87952E", 3, 3, false)));
        assert!(chip_spec(0x8688).unwrap().readback_exempt);
        assert_eq!(chip_spec(0x8686).unwrap().temperatures, 6);
        assert_eq!(chip_spec(0x8733).unwrap().name, "IT8792E");
        assert_eq!(chip_spec(0x8790), None);
        for id in [0x8733, 0x8695, 0x8883, 0x8790, 0x8795, 0x879F] {
            assert!(noconf_family(id), "{id:X}");
        }
        for id in [0x8696, 0x8689, 0x8688, 0x8686, 0x1234, 0x8780] {
            assert!(!noconf_family(id), "{id:X}");
        }
    }

    /// This PC, as the hardware probe found it: both chips found, read as the probe read them, and each
    /// left as found: the IT87952E at 0x4E (already in configuration mode) with no key and no exit and its
    /// logical device put back; the IT8696E at 0x2E keyed and exited. The IT8696E's 6th temperature (0xC9)
    /// and its fan 6 (off) aren't published, nor the IT87952E's 2nd temperature.
    #[test]
    fn this_pc() {
        let mut rig = Rig::new(FakeLpc::this_pc());
        let (values, notes) = rig.read();
        assert_eq!(notes, Vec::<String>::new());
        let rounded: Vec<(&str, f64)> = values.iter().map(|(k, v)| (k.as_str(), v.round())).collect();
        assert_eq!(rounded, [
            ("pawnio.ite_it8696e.temperature.temperature_1", 37.0), ("pawnio.ite_it8696e.temperature.temperature_2", 47.0),
            ("pawnio.ite_it8696e.temperature.temperature_3", 72.0), ("pawnio.ite_it8696e.temperature.temperature_4", 44.0),
            ("pawnio.ite_it8696e.temperature.temperature_5", 51.0), ("pawnio.ite_it8696e.fan.fan_1", 2368.0),
            ("pawnio.ite_it8696e.fan.fan_2", 1221.0), ("pawnio.ite_it8696e.fan.fan_3", 1184.0),
            ("pawnio.ite_it8696e.fan.fan_4", 1170.0), ("pawnio.ite_it8696e.fan.fan_5", 0.0),
            ("pawnio.ite_it87952e.temperature.temperature_1", 40.0), ("pawnio.ite_it87952e.temperature.temperature_3", 41.0),
            ("pawnio.ite_it87952e.fan.fan_1", 0.0), ("pawnio.ite_it87952e.fan.fan_2", 6250.0),
            ("pawnio.ite_it87952e.fan.fan_3", 0.0),
        ]);
        assert_eq!(values.iter().find(|(k, _)| k == "pawnio.ite_it87952e.fan.fan_2").unwrap().1, 6250.0);
        let writes = rig.lpc.writes();
        assert_eq!(keys_and_exits(&writes, 0x4E), (0, 0));
        assert_eq!(keys_and_exits(&writes, 0x2E), (1, 1));
        let mut bus = rig.lpc.bus();
        assert!(bus.chip(0x4E).configured && bus.chip(0x4E).ldn == 0x0F);
        assert!(!bus.chip(0x2E).configured);
        drop(bus);
        let polls_from = writes.len();
        assert_eq!(rig.outcomes(), ["found IT87952E at 0xA60", "found IT8696E at 0xA40"]);
        let a = &rig.superio.attempts;
        assert!(!a[0].key_sent && a[0].ldn_restored && !a[0].exit_sent && a[0].id_before == Some(0x8695) && a[0].ldn_before == Some(0x0F));
        assert!(a[1].key_sent && !a[1].ldn_restored && a[1].exit_sent && a[1].id_before == Some(0xFFFF) && a[1].id_after == Some(0x8696)
                && a[1].ldn_before == Some(0x07));
        // the fans on, from regs 0x13 and 0x0C as read at detection: the IT8696E's 1-5, the IT87952E's 1-3
        let fans: Vec<[bool; 6]> = rig.superio.chips().map(|c| c.fans).collect();
        assert_eq!(fans, [[true, true, true, true, true, false], [true, true, true, false, false, false]]);
        // a second read: no configuration access at all
        rig.later(60).read();
        let writes = rig.lpc.writes();
        check_writes(&writes, &[0xA40, 0xA60], polls_from);
        assert_eq!(rig.lpc.bus().opens, 2);
        assert_eq!(rig.lock.waits(), [100, 100, 10, 10, 10, 10]);
    }

    /// The write-set cases (spec §6.1), each checked against the port-level rules.
    #[test]
    fn write_sets() {
        struct Case {
            name: &'static str,
            port: u16,
            id: u16,
            configured: bool,
            keyed_by: bool,
            /// Keys sent, exits sent, logical device put back, find_bars run.
            expect: (usize, usize, bool, bool),
        }
        let cases = [
            Case { name: "1: 0x4E, valid ID", port: 0x4E, id: 0x8696, configured: true, keyed_by: true, expect: (0, 0, true, true) },
            Case { name: "2: 0x4E keyed, known", port: 0x4E, id: 0x8696, configured: false, keyed_by: true, expect: (1, 1, false, true) },
            Case { name: "3: 0x4E keyed, IT87952E", port: 0x4E, id: 0x8695, configured: false, keyed_by: true, expect: (1, 0, true, true) },
            Case { name: "4: 0x2E keyed", port: 0x2E, id: 0x8696, configured: false, keyed_by: true, expect: (1, 1, false, true) },
            Case { name: "5: 0x2E already in", port: 0x2E, id: 0x8696, configured: true, keyed_by: true, expect: (0, 0, true, true) },
            Case { name: "6: 0x2E keyed, IT8792E", port: 0x2E, id: 0x8733, configured: false, keyed_by: true, expect: (1, 0, true, true) },
            Case { name: "7: 0x4E keyed, unknown", port: 0x4E, id: 0x1234, configured: false, keyed_by: true, expect: (1, 1, false, false) },
            Case { name: "7: 0x2E keyed, unknown", port: 0x2E, id: 0x1234, configured: false, keyed_by: true, expect: (1, 1, false, false) },
            Case { name: "8: 0x2E keyed, 0x8790", port: 0x2E, id: 0x8790, configured: false, keyed_by: true, expect: (1, 0, false, false) },
            Case { name: "8: 0x4E keyed, 0x8883", port: 0x4E, id: 0x8883, configured: false, keyed_by: true, expect: (1, 0, false, false) },
            Case { name: "9: unknown, already in", port: 0x4E, id: 0x1234, configured: true, keyed_by: true, expect: (0, 0, false, false) },
            Case { name: "10: nothing after the key", port: 0x2E, id: 0x8696, configured: false, keyed_by: false, expect: (1, 0, false, false) },
        ];
        for case in cases {
            let (mut chip, hwm) = one(case.port, case.id, 0xA40);
            chip.configured = case.configured;
            chip.keyed_by = case.keyed_by;
            let mut rig = Rig::new(bus(vec![(chip, hwm)]));
            rig.read();
            let writes = rig.lpc.writes();
            let (keys, exits) = keys_and_exits(&writes, case.port);
            let attempt = rig.superio.attempts.iter().find(|a| a.port == case.port).unwrap();
            let found_bars = rig.lpc.calls().iter().any(|c| c == "ioctl_find_bars");
            assert_eq!((keys, exits, attempt.ldn_restored, found_bars), case.expect, "{}", case.name);
            assert_eq!(attempt.exit_sent, exits == 1, "{}", case.name);
            check_writes(&writes, &[0xA40], writes.len());
            let mut bus = rig.lpc.bus();
            let chip = bus.chip(case.port);
            // its logical device as found, unless it left configuration mode, which is as found anyway
            if exits == 0 {
                assert_eq!(chip.ldn, 0x0A, "{}", case.name);
            }
            // left as found, but for the chips that mustn't leave configuration mode once we entered it
            let stays = case.keyed_by && noconf_family(case.id);
            assert_eq!(chip.configured, case.configured || stays, "{}", case.name);
            // nothing at the other port
            let other = if case.port == 0x2E { 0x4E } else { 0x2E };
            assert!(writes.iter().all(|(p, _)| *p != other + 1), "{}", case.name);
        }
    }

    /// One IT8696E at `port`, found in configuration mode or not, on a bus of its own.
    fn alone(port: u16, configured: bool) -> Rig {
        let (mut chip, hwm) = one(port, 0x8696, 0xA40);
        chip.configured = configured;
        Rig::new(bus(vec![(chip, hwm)]))
    }

    /// Whether the chip at `port` is as it was found: in configuration mode or not, as before, and, if it
    /// was in it, with its logical device as before.
    fn as_found(rig: &Rig, port: u16, configured: bool) -> bool {
        let mut bus = rig.lpc.bus();
        let chip = bus.chip(port);
        chip.configured == configured && (!configured || chip.ldn == 0x0A)
    }

    /// Stopping after any write of a detection attempt (the parent died): the chip is still left as found
    /// before the attempt returns, and nothing more is written. Tried after every write but those inside
    /// find_bars, which is one call to the driver, so one step: for those, after its last.
    #[test]
    fn stop_at_any_write() {
        for (port, configured) in [(0x2E, false), (0x4E, true), (0x4E, false)] {
            let mut whole = alone(port, configured);
            whole.read();
            let bus = whole.lpc.bus();
            let steps: Vec<usize> = bus.calls.iter().zip(&bus.spans)
                .flat_map(|((function, _), span)| if function == "ioctl_find_bars" { span.end..span.end + 1 } else { span.start + 1..span.end + 1 })
                .filter(|&n| n > 0)
                .collect();
            assert!(steps.len() > 20 && bus.writes.len() > 3000, "{} {}", steps.len(), bus.writes.len());
            drop(bus);
            for n in steps {
                let mut rig = alone(port, configured);
                let hw = rig.hw.clone();
                rig.lpc.bus().after_write = Some((n, Box::new(move || hw.ask_to_stop())));
                rig.read();
                let writes = rig.lpc.writes();
                check_writes(&writes, &[0xA40], writes.len());
                assert!(as_found(&rig, port, configured), "port {port:X}, stopped after write {n}");
                assert!(n <= writes.len());
                rig.read();
                assert_eq!(rig.lpc.writes().len(), writes.len(), "port {port:X}, stopped after write {n}");
            }
        }
    }

    /// The parent dies after the key, and during find_bars: whoever ends the process (`HwLock::stop`, on
    /// another thread) gets the lock only once the exit, or the logical device's restore, is written.
    #[test]
    fn stop_waits_for_the_restore() {
        for (port, configured, after) in [(0x2E, false, "the key"), (0x2E, false, "find_bars"), (0x4E, true, "find_bars")] {
            // the write after which the parent dies: the key's last byte, or one in find_bars
            let n = {
                let mut whole = alone(port, configured);
                whole.read();
                let writes = whole.lpc.writes();
                match after {
                    "the key" => writes.iter().position(|w| *w == (port, 0x55)).unwrap() + 2,
                    _ => writes.iter().position(|w| *w == (port + 1, 0x80)).unwrap() + 1,
                }
            };
            let mut rig = alone(port, configured);
            let stopper: Arc<std::sync::Mutex<Option<std::thread::JoinHandle<bool>>>> = Arc::default();
            let (hw, lpc, started) = (rig.hw.clone(), rig.lpc.clone(), stopper.clone());
            rig.lpc.bus().after_write = Some((n, Box::new(move || {
                let (stopping, lpc) = (hw.clone(), lpc.clone());
                *started.lock().unwrap() = Some(std::thread::spawn(move || {
                    let _exiting = stopping.stop();
                    // the process would end here: the chip must be as found already
                    let mut bus = lpc.bus();
                    let chip = bus.chip(port);
                    chip.configured == configured && (!configured || chip.ldn == 0x0A)
                }));
                // until the other thread has asked, and waits for the lock this attempt holds
                while !hw.stopping() {
                    std::thread::yield_now();
                }
            })));
            rig.read();
            let stopper = stopper.lock().unwrap().take().unwrap();
            assert!(stopper.join().unwrap(), "port {port:X}, the parent died after {after}");
        }
    }

    /// Fan 6 is off when reg 0x0C bit 2 is clear; fan 2 when reg 0x13 bit 5 is.
    #[test]
    fn fans_off() {
        let (chip, mut hwm) = one(0x2E, 0x8696, 0xA40);
        hwm.regs[0x13] = 0x50; // fans 1 and 3
        hwm.regs[0x0C] = 0x30; // fans 4 and 5
        for i in 0..6 {
            hwm.regs[FAN_LOW[i] as usize] = 100 + i as u8;
        }
        let mut rig = Rig::new(bus(vec![(chip, hwm)]));
        let fans: Vec<String> = rig.keys().into_iter().filter(|k| k.contains(".fan.")).collect();
        assert_eq!(fans, ["pawnio.ite_it8696e.fan.fan_1", "pawnio.ite_it8696e.fan.fan_3", "pawnio.ite_it8696e.fan.fan_4",
                          "pawnio.ite_it8696e.fan.fan_5"]);
        // and a fan that's off isn't read at all
        let reads: Vec<u8> = rig.lpc.bus().calls.iter()
            .filter(|(f, a)| f == "ioctl_pio_outb" && a[0] == 0xA45).map(|(_, a)| a[1] as u8).collect();
        assert!(!reads.contains(&0x0E) && !reads.contains(&0x4C));
        rig.lpc.bus().hwm(0xA40).regs[0x0C] = 0x34;
        assert_eq!(rig.later(60).keys().len(), 2 + 4); // read once, at detection
    }

    /// A register whose index doesn't read back is left out (that channel only); the IT8688E doesn't read
    /// back at all, and is read anyway.
    #[test]
    fn read_back() {
        let (chip, mut hwm) = one(0x2E, 0x8696, 0xA40);
        hwm.bad_readback = [0x2A, 0x0D].into();
        let (second, mut exempt) = one(0x4E, 0x8688, 0xA60);
        exempt.no_readback = true;
        let mut rig = Rig::new(bus(vec![(chip, hwm), (second, exempt)]));
        assert_eq!(rig.keys(), ["pawnio.ite_it8696e.temperature.temperature_1", "pawnio.ite_it8688e.temperature.temperature_1",
                                "pawnio.ite_it8688e.temperature.temperature_2", "pawnio.ite_it8688e.fan.fan_1"]);
        assert!(rig.superio.chips().find(|c| c.spec.name == "IT8696E").unwrap().counts.read_backs >= 2);
    }

    /// The low byte of a fan's count is read before its high byte.
    #[test]
    fn low_byte_first() {
        let mut rig = Rig::new(FakeLpc::this_pc());
        rig.read();
        let indexes: Vec<u8> = rig.lpc.writes().iter().filter(|(p, _)| *p == 0xA45).map(|(_, v)| *v).collect();
        for i in 0..5 {
            let low = indexes.iter().position(|&r| r == FAN_LOW[i]).unwrap();
            let high = indexes.iter().position(|&r| r == FAN_HIGH[i]).unwrap();
            assert!(low < high, "fan {}", i + 1);
        }
    }

    /// The bank guard: another bank at the start skips the chip with a note; a switch during the sample
    /// throws it away; at detection, it's a retry, not a lost chip. The IT87952E has no banks.
    #[test]
    fn bank_guard() {
        let mut rig = Rig::new(FakeLpc::this_pc());
        rig.read();
        rig.lpc.bus().hwm(0xA40).regs[0x06] = 0x20;
        let (values, notes) = rig.later(60).read();
        assert_eq!(notes, [BANK_SWITCHED]);
        assert!(values.iter().all(|(k, _)| k.contains("it87952e")) && values.len() == 5);

        // switched by another program in the middle: the whole sample goes
        rig.lpc.bus().hwm(0xA40).regs[0x06] = 0;
        rig.lpc.bus().hwm(0xA40).on_read = Some(Box::new(|reg, regs| if reg == 0x2B { regs[0x06] = 0x40 }));
        let (values, notes) = rig.later(60).read();
        assert_eq!(notes, [BANK_SWITCHED]);
        assert!(values.iter().all(|(k, _)| k.contains("it87952e")));
        rig.lpc.bus().hwm(0xA40).on_read = None;
        rig.lpc.bus().hwm(0xA40).regs[0x06] = 0;
        assert_eq!(rig.later(60).read().0.len(), 15);
        assert_eq!(rig.superio.chips().find(|c| c.slot == 0).unwrap().counts.bank_trips, 2);
        // never written
        assert!(rig.lpc.writes().iter().all(|&(p, _)| p != 0xA46));

        // at detection: a retry 2 s later, then found
        let lpc = FakeLpc::this_pc();
        lpc.bus().hwm(0xA40).regs[0x06] = 0x60;
        let mut rig = Rig::new(lpc);
        assert_eq!(rig.read().0.len(), 5);
        assert_eq!(rig.outcomes(), ["found IT87952E at 0xA60", "retry: Bank"]);
        rig.lpc.bus().hwm(0xA40).regs[0x06] = 0;
        assert_eq!(rig.later(1).read().0.len(), 5);
        assert_eq!(rig.later(1).read().0.len(), 15);
        assert_eq!(rig.outcomes().last().unwrap(), "found IT8696E at 0xA40");
        assert_eq!(keys_and_exits(&rig.lpc.writes(), 0x2E), (2, 2));
    }

    /// A second chip at the same base is the first one answering twice: dropped, with a note.
    #[test]
    fn duplicate_base() {
        let (first, hwm) = one(0x4E, 0x8696, 0xA40);
        let mut second = FakeChip::new(0x2E, 0x8696, 0xA40);
        second.configured = true;
        let mut rig = Rig::new(FakeLpc::new(vec![first, second], vec![hwm]));
        let (values, notes) = rig.read();
        assert_eq!(notes, ["The motherboard sensor chips at 0x4E and 0x2E are the same one, so it's read once."]);
        assert_eq!(values.len(), 3);
        assert!(values.iter().all(|(k, _)| k.starts_with("pawnio.ite_it8696e.")));
        assert_eq!(rig.outcomes(), ["found IT8696E at 0xA40", "duplicate"]);
        rig.later(60).read();
        assert_eq!(rig.superio.attempts.len(), 2);
    }

    /// Nothing at a slot: tried once more 2 s on, then that's final. With nothing at either, a note.
    #[test]
    fn absent_twice_is_final() {
        let mut rig = Rig::new(FakeLpc::new(Vec::new(), Vec::new()));
        assert_eq!(rig.read(), (vec![], vec![]));
        assert_eq!(rig.outcomes(), ["absent", "absent"]);
        assert_eq!(rig.later(1).read(), (vec![], vec![]));
        assert_eq!(rig.superio.attempts.len(), 2);
        assert_eq!(rig.later(1).read(), (vec![], vec![NONE_FOUND.to_string()]));
        assert_eq!(rig.outcomes(), ["absent", "absent", "absent", "absent"]);
        let times: Vec<u64> = rig.superio.attempts.iter().map(|a| a.at_ms).collect();
        assert_eq!(times, [0, 0, 2000, 2000]);
        rig.later(3600).read();
        assert_eq!(rig.superio.attempts.len(), 4);
        // every attempt keyed and none left: nothing answered
        assert_eq!(keys_and_exits(&rig.lpc.writes(), 0x2E), (2, 0));
    }

    /// A chip that answers only the second time (its key was trodden on) is found then.
    #[test]
    fn absent_then_found() {
        let (mut chip, hwm) = one(0x2E, 0x8696, 0xA40);
        chip.keyed_by = false;
        let mut rig = Rig::new(bus(vec![(chip, hwm)]));
        rig.read();
        rig.lpc.bus().chip(0x2E).keyed_by = true;
        assert_eq!(rig.later(2).keys().len(), 3);
        assert_eq!(rig.outcomes(), ["absent", "absent", "absent", "found IT8696E at 0xA40"]);
    }

    /// An attempt that goes wrong is tried again 2 s, 10 s and 60 s later, then given up with a note; each
    /// that keyed also left.
    #[test]
    fn retries() {
        let (chip, mut hwm) = one(0x2E, 0x8696, 0xA40);
        hwm.regs[0x58] = 0x12; // not ITE's vendor ID
        let mut rig = Rig::new(bus(vec![(chip, hwm)]));
        rig.read();
        let at_2e = |rig: &Rig| rig.superio.attempts.iter().filter(|a| a.port == 0x2E).map(|a| a.at_ms / 1000).collect::<Vec<_>>();
        for (wait, attempts) in [(1, vec![0]), (1, vec![0, 2]), (9, vec![0, 2]), (1, vec![0, 2, 12]), (59, vec![0, 2, 12])] {
            rig.later(wait).read();
            assert_eq!(at_2e(&rig), attempts);
        }
        let (_, notes) = rig.later(1).read();
        assert_eq!(at_2e(&rig), [0, 2, 12, 72]);
        assert_eq!(notes, [gave_up(0x2E), NONE_FOUND.to_string()]);
        assert!(gave_up(0x2E).starts_with("The motherboard sensor chip at 0x2E didn't answer as expected, so its fans and"));
        rig.later(3600).read();
        assert_eq!(at_2e(&rig).len(), 4);
        assert_eq!(keys_and_exits(&rig.lpc.writes(), 0x2E), (4, 4));
        assert!(rig.superio.attempts.iter().filter(|a| a.port == 0x2E).all(|a| a.outcome == "retry: Vendor"));
    }

    /// The shared lock busy at detection is a retry; at a poll, the chip is skipped, with a note after three
    /// in a row.
    #[test]
    fn busy() {
        let mut rig = Rig::new(FakeLpc::this_pc());
        rig.lock.set_busy(true);
        rig.read();
        assert_eq!(rig.outcomes(), ["retry: Busy", "retry: Busy"]);
        assert!(rig.lpc.writes().is_empty());
        rig.lock.set_busy(false);
        assert_eq!(rig.later(2).read().0.len(), 15);
        rig.lock.set_busy(true);
        assert_eq!(rig.later(60).read(), (vec![], vec![]));
        assert_eq!(rig.later(60).read(), (vec![], vec![]));
        assert_eq!(rig.later(60).read(), (vec![], vec![BUSY.to_string()]));
        rig.lock.set_busy(false);
        assert_eq!(rig.later(60).read().1, Vec::<String>::new());
        assert_eq!(rig.lock.held(), 0);
    }

    /// Unknown chips: a note, and nothing else read.
    #[test]
    fn unsupported() {
        let (chip, hwm) = one(0x2E, 0x8628, 0xA40);
        let mut rig = Rig::new(bus(vec![(chip, hwm)]));
        let (values, notes) = rig.read();
        assert!(values.is_empty());
        assert_eq!(notes, ["Found Super I/O chip 0x8628 at 0x2E, which isn't supported yet."]);
        rig.later(3).read();
        assert_eq!(rig.outcomes(), ["absent", "unsupported 0x8628", "absent"]);
    }

    /// A chip that stops answering (three vendor misses): its registers are then tried only every 2nd, 4th,
    /// 8th poll, at least every 5 minutes, with no configuration access; it's looked for again in
    /// configuration space only after 2 minutes, and 30 s after a sleep, twice at most; then a note.
    #[test]
    fn redetection() {
        let mut rig = Rig::new(FakeLpc::this_pc());
        rig.read();
        let detected = rig.lpc.writes().len();
        rig.lpc.bus().hwm(0xA40).regs[0x58] = 0;
        let vendor_reads = |rig: &Rig| rig.lpc.writes().iter().filter(|w| **w == (0xA45, 0x58)).count();
        let before = vendor_reads(&rig);
        // 10 s polls: three misses, then missing
        let mut tried = Vec::new();
        for poll in 1..=11 {
            let reads = vendor_reads(&rig);
            rig.later(10).read();
            if vendor_reads(&rig) > reads {
                tried.push(poll);
            }
        }
        assert_eq!(tried, [1, 2, 3, 5, 9]);
        assert_eq!(vendor_reads(&rig), before + 5);
        check_writes(&rig.lpc.writes(), &[0xA40, 0xA60], detected);
        // 2 minutes after it went missing (at 30 s): one full detection, which finds nothing new
        rig.lpc.bus().chip(0x2E).id = 0x1234;
        rig.later(39).read();
        assert_eq!(rig.superio.attempts.len(), 2);
        rig.later(1).read();
        assert_eq!(rig.superio.attempts.len(), 3);
        assert_eq!(rig.outcomes()[2], "unsupported 0x1234");
        // a sleep: not again until 30 s after it, though 2 minutes have passed
        rig.later(119).read();
        rig.resumed_at = Some(rig.clock.since_boot());
        rig.later(1).read();
        rig.later(28).read();
        assert_eq!(rig.superio.attempts.len(), 3);
        rig.later(1).read();
        assert_eq!(rig.superio.attempts.len(), 4);
        // two is the most: 2 minutes on, a note, and the chip is given up
        let (values, notes) = rig.later(120).read();
        assert_eq!(rig.superio.attempts.len(), 4);
        assert_eq!(notes, [STOPPED_ANSWERING]);
        assert_eq!(values.len(), 5);
        let writes = rig.lpc.writes().len();
        assert_eq!(rig.later(600).read().1, [STOPPED_ANSWERING]);
        assert!(rig.lpc.writes()[writes..].iter().all(|&(p, _)| p == 0xA65));
    }

    /// A missing chip that answers again, at a try of its registers, is read again; one found again by a
    /// re-detection too.
    #[test]
    fn missing_comes_back() {
        let mut rig = Rig::new(FakeLpc::this_pc());
        rig.read();
        rig.lpc.bus().hwm(0xA40).regs[0x58] = 0;
        for _ in 0..3 {
            rig.later(10).read();
        }
        rig.lpc.bus().hwm(0xA40).regs[0x58] = 0x90;
        assert_eq!(rig.later(10).read().0.len(), 5); // not tried this poll
        assert_eq!(rig.later(10).read().0.len(), 15);
        assert_eq!(rig.later(10).read().0.len(), 15);

        // gone for long enough for a re-detection, which finds it at a new base
        rig.lpc.bus().hwm(0xA40).regs[0x58] = 0;
        for _ in 0..3 {
            rig.later(60).read();
        }
        {
            let mut bus = rig.lpc.bus();
            bus.chip(0x2E).bases.insert(4, [0xA80, 0]);
            let mut moved = FakeHwm::new(0xA80);
            moved.regs = bus.hwm(0xA40).regs;
            moved.regs[0x58] = 0x90;
            bus.hwms.push(moved);
        }
        let (values, _) = rig.later(120).read();
        assert_eq!(values.len(), 15);
        assert_eq!(rig.outcomes().last().unwrap(), "found IT8696E at 0xA80");
    }

    /// A chip with banks whose registers stop answering (its ports read 0xFF, as with nothing there) goes
    /// missing as any chip does: its bank register not reading back isn't a switched bank, so there's no
    /// bank note or bank trip, and its registers are then tried only now and then. At detection, that's a
    /// read-back retry.
    #[test]
    fn gone_is_not_a_switched_bank() {
        let mut rig = Rig::new(FakeLpc::this_pc());
        rig.read();
        rig.lpc.bus().hwms.retain(|h| h.base != 0xA40);
        let banks = |rig: &Rig| rig.lpc.writes().iter().filter(|w| **w == (0xA45, BANK)).count();
        let mut tried = Vec::new();
        for poll in 1..=9 {
            let before = banks(&rig);
            let (values, notes) = rig.later(10).read();
            assert_eq!((values.len(), notes), (5, Vec::<String>::new()), "poll {poll}");
            if banks(&rig) > before {
                tried.push(poll);
            }
        }
        assert_eq!(tried, [1, 2, 3, 5, 9]);
        let chip = rig.superio.chips().find(|c| c.slot == 0).unwrap();
        assert_eq!((chip.counts.bank_trips, chip.counts.misses, chip.counts.read_backs), (0, 5, 5));
        assert!(chip.is_missing());

        let lpc = FakeLpc::this_pc();
        lpc.bus().hwms.retain(|h| h.base != 0xA40);
        let mut rig = Rig::new(lpc);
        rig.read();
        assert_eq!(rig.outcomes(), ["found IT87952E at 0xA60", "retry: ReadBack"]);
    }

    /// A re-detection that couldn't run, as another program held the lock, isn't one of the two.
    #[test]
    fn busy_redetection_is_not_counted() {
        let mut rig = Rig::new(FakeLpc::this_pc());
        rig.read();
        rig.lpc.bus().hwm(0xA40).regs[0x58] = 0;
        // missing from 30 s; looked for again from 150 s, when the lock is busy
        for _ in 0..3 {
            rig.later(10).read();
        }
        rig.lock.set_busy(true);
        rig.later(120).read();
        rig.lock.set_busy(false);
        assert_eq!(rig.outcomes()[2..], ["retry: Busy"]);
        // then two that run, and find it still not answering, and then the note
        rig.later(120).read();
        rig.later(120).read();
        assert_eq!(rig.outcomes()[2..], ["retry: Busy", "retry: Vendor", "retry: Vendor"]);
        assert_eq!(rig.later(120).read().1, [STOPPED_ANSWERING]);
        assert_eq!(rig.superio.attempts.len(), 5);
    }

    /// A slot's module calls that fail (the driver's say), from the `nth` (from 1) of `function` with `args`
    /// after the slot is selected, for `count` calls.
    fn failing(slot: u64, function: &'static str, args: &'static [u64], nth: usize, count: usize)
               -> Box<dyn FnMut(&str, &[u64]) -> Option<IoError> + Send> {
        let (mut selected, mut n) = (None, 0);
        Box::new(move |called, given| {
            if called == "ioctl_select_slot" {
                selected = given.first().copied();
            }
            if selected != Some(slot) || called != function || given != args {
                return None;
            }
            n += 1;
            (nth..nth + count).contains(&n).then_some(IoError::Other(1117))
        })
    }

    /// The ID after the key is read again if the driver fails it: a chip that answers the second or third
    /// time is let go as ever. One that never does isn't sent the exit, as it may be one that mustn't be; the
    /// attempt says it may be left in configuration mode, as a note does at every read from then on. Its
    /// retry then finds it in configuration mode, and reads it with no key and no exit.
    #[test]
    fn id_after_the_key_is_read_again() {
        for fails in [1, 2, 3] {
            let mut rig = alone(0x2E, false);
            rig.lpc.bus().fail = Some(failing(0, "ioctl_superio_inw", &[0x20], 2, fails));
            let (values, notes) = rig.read();
            let writes = rig.lpc.writes();
            check_writes(&writes, &[0xA40], writes.len());
            let a = rig.superio.attempts.iter().find(|a| a.port == 0x2E).unwrap().clone();
            assert!(a.key_sent, "{fails}");
            if fails < ID_TRIES as usize {
                assert_eq!((values.len(), notes), (3, Vec::<String>::new()), "{fails}");
                assert!(a.exit_sent && !a.left_keyed && a.id_after == Some(0x8696), "{fails}");
                assert_eq!(keys_and_exits(&writes, 0x2E), (1, 1));
                assert!(as_found(&rig, 0x2E, false));
                continue;
            }
            assert_eq!((values.len(), notes), (0, vec![left_keyed(0x2E)]));
            assert!(!a.exit_sent && a.left_keyed && a.id_after.is_none() && a.outcome == "retry: Io(Other(1117))");
            assert_eq!(keys_and_exits(&writes, 0x2E), (1, 0));
            assert!(rig.lpc.bus().chip(0x2E).configured);
            let (values, notes) = rig.later(2).read();
            assert_eq!((values.len(), notes), (3, vec![left_keyed(0x2E)]));
            let a = rig.superio.attempts.last().unwrap();
            assert!(a.id_before == Some(0x8696) && !a.key_sent && !a.exit_sent && a.ldn_restored && !a.left_keyed);
            assert_eq!(keys_and_exits(&rig.lpc.writes(), 0x2E), (1, 0));
        }
    }

    /// A key byte the driver fails may still have gone through: the ID is read anyway, and a chip that
    /// answers it is let go, as after any key; the attempt is tried again later.
    #[test]
    fn a_failed_key_is_let_go() {
        let mut rig = alone(0x2E, false);
        // the key's last byte, 0x55 a second time
        rig.lpc.bus().fail = Some(failing(0, "ioctl_pio_outb", &[0x2E, 0x55], 2, 1));
        rig.read();
        let a = rig.superio.attempts.iter().find(|a| a.port == 0x2E).unwrap();
        assert!(!a.key_sent && a.id_after == Some(0x8696) && a.exit_sent && !a.left_keyed);
        assert_eq!(a.outcome, "retry: Io(Other(1117))");
        assert!(as_found(&rig, 0x2E, false));
        assert_eq!(rig.later(2).keys().len(), 3);
        assert_eq!(keys_and_exits(&rig.lpc.writes(), 0x2E), (2, 2));
    }

    /// The only module functions used, and what each writes: the configuration index and data only through
    /// the module's own functions, ports only at the configuration port (the key and the exit) and base+5.
    #[test]
    fn appendix_a() {
        let mut rig = Rig::new(FakeLpc::this_pc());
        for _ in 0..3 {
            rig.later(60).read();
        }
        for (function, args) in rig.lpc.bus().calls.iter() {
            match function.as_str() {
                "ioctl_select_slot" | "ioctl_find_bars" | "ioctl_superio_inb" | "ioctl_superio_inw" | "ioctl_pio_inb" => {}
                "ioctl_superio_outb" => assert_eq!(args[0], 0x07, "{args:X?}"),
                "ioctl_pio_outb" => assert!([0x2E, 0x2F, 0x4E, 0x4F, 0xA45, 0xA65].contains(&args[0]), "{args:X?}"),
                other => panic!("{other}"),
            }
        }
    }

    /// Without the shared lock, or the module, nothing is read, with a note.
    #[test]
    fn nothing_without_lock_or_module() {
        let mut rig = Rig::new(FakeLpc::this_pc());
        let poll = Poll { hw: &rig.hw, clock: &rig.clock, now: Duration::ZERO, resumed_at: None };
        let (mut out, mut notes) = (Vec::new(), Vec::new());
        rig.superio.read(&poll, &rig.open, None, &mut out, &mut notes);
        assert_eq!(notes, [Bus::Isa.unshared()]);
        assert!(rig.lpc.calls().is_empty() && out.is_empty());

        rig.open = fake::opener(Err(crate::pawnio::OpenError::NotSupported), Err(crate::pawnio::OpenError::TooOld));
        assert_eq!(rig.read(), (vec![], vec![crate::pawnio::reader::TOO_OLD.to_string()]));
        assert_eq!(rig.later(60).read(), (vec![], vec![crate::pawnio::reader::TOO_OLD.to_string()]));
        assert_eq!(rig.superio.attempts.len(), 1);
    }
}
