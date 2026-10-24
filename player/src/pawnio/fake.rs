//! Tests: fake modules for the CPU and the Super I/O chips, a lock that's always free, and a clock the test
//! moves.
//!
//! `FakeLpc` is the LpcIO module (player/pawnio/src/LpcIO.p) over a pretend LPC bus: each module function
//! does the port reads and writes the real one does, so the bus's log is every byte the reader would write
//! to the hardware. On the bus, each Super I/O chip has its configuration ports (with ITE's key, chip ID,
//! logical devices and their base registers), and each environment controller its register file at its
//! base (index, data and read-back, banks).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use super::reader::Clock;
use super::{Exec, Held, IoError, Lock, ModuleId, OpenError, OpenModule};

/// A module opener with these fakes: each LpcIO open is a new handle on the same bus.
pub fn opener(amd: Result<FakeAmd, OpenError>, lpc: Result<FakeLpc, OpenError>) -> OpenModule {
    Box::new(move |module| match module {
        ModuleId::AmdFamily17 => amd.clone().map(|amd| Box::new(amd) as Box<dyn Exec>),
        ModuleId::LpcIo => lpc.clone().map(|lpc| Box::new(lpc.open()) as Box<dyn Exec>),
    })
}

/// The AMDFamily17 module: SMN registers (all ones where not set, as a read of nothing gives) and the two
/// energy MSRs, which the test sets, and every call made.
#[derive(Clone, Default)]
pub struct FakeAmd(Arc<Mutex<AmdState>>);

#[derive(Default)]
struct AmdState {
    smn: HashMap<u32, u32>,
    msr: HashMap<u32, u64>,
    smn_error: Option<IoError>,
    calls: Vec<(String, u64)>,
}

impl FakeAmd {
    /// This PC's Ryzen 9 9950X3D as the hardware probe found it (its first sample, probe-all.json
    /// `cpu_raw.samples[0]`): Tctl 72.125 °C (it reads 49 °C high, both flags set), CCDs at 39 and 63 °C in
    /// slots 0 and 1, the other six of the 8 slots reading 0, and the energy counter's unit (16) and count.
    pub fn this_pc() -> FakeAmd {
        let amd = FakeAmd::default();
        amd.set_smn(0x5_9800, 0x793B_0000);
        for (slot, raw) in [0xAC0, 0xB80, 0, 0, 0, 0, 0, 0].into_iter().enumerate() {
            amd.set_smn(0x5_9B08 + 4 * slot as u32, raw);
        }
        amd.set_msr(0xC001_0299, 0x000A_1000);
        amd.set_msr(0xC001_029B, 0x78_B3A0_434E);
        amd
    }

    pub fn set_smn(&self, address: u32, value: u32) {
        self.0.lock().unwrap().smn.insert(address, value);
    }

    pub fn set_msr(&self, msr: u32, value: u64) {
        self.0.lock().unwrap().msr.insert(msr, value);
    }

    /// An MSR as set (0 where not).
    pub fn msr(&self, msr: u32) -> u64 {
        self.0.lock().unwrap().msr.get(&msr).copied().unwrap_or(0)
    }

    /// Every SMN read fails with this from now on (None: they work).
    pub fn fail_smn(&self, error: Option<IoError>) {
        self.0.lock().unwrap().smn_error = error;
    }

    /// The calls so far: function and first argument.
    pub fn calls(&self) -> Vec<(String, u64)> {
        self.0.lock().unwrap().calls.clone()
    }
}

impl Exec for FakeAmd {
    fn execute(&self, function: &str, input: &[u64], out: &mut [u64]) -> Result<(), IoError> {
        let mut state = self.0.lock().unwrap();
        state.calls.push((function.to_string(), input.first().copied().unwrap_or_default()));
        match (function, input.len(), out.len()) {
            ("ioctl_read_smn", 1, 1) => {
                if let Some(e) = state.smn_error {
                    return Err(e);
                }
                out[0] = state.smn.get(&(input[0] as u32)).copied().unwrap_or(u32::MAX) as u64;
            }
            // the energy MSRs; any other, as the module's allow-list would, is refused
            ("ioctl_read_msr", 1, 1) if [0xC001_0299, 0xC001_029B].contains(&input[0]) => {
                out[0] = state.msr.get(&(input[0] as u32)).copied().unwrap_or(0);
            }
            ("ioctl_read_msr", 1, 1) => return Err(IoError::AccessDenied),
            ("ioctl_write_msr", 2, 0) => {}
            ("ioctl_read_smn" | "ioctl_read_msr" | "ioctl_write_msr", ..) => return Err(IoError::BadArguments),
            _ => return Err(IoError::NoSuchFunction),
        }
        Ok(())
    }
}

/// A Super I/O chip's configuration side, at its port (0x2E or 0x4E; data at port + 1).
pub struct FakeChip {
    pub port: u16,
    pub id: u16,
    /// In configuration mode: its registers answer. Out of it, every read is 0xFF.
    pub configured: bool,
    /// Whether ITE's key for its port puts it in configuration mode (false: something that isn't an ITE
    /// chip, or a broken key).
    pub keyed_by: bool,
    /// The logical device selected (reg 0x07).
    pub ldn: u8,
    /// Each logical device's base registers: 0x60/0x61 and 0x62/0x63.
    pub bases: HashMap<u8, [u16; 2]>,
    index: u8,
    key: usize,
}

impl FakeChip {
    /// An ITE chip at `port`, out of configuration mode, with its environment controller (LDN 4) at
    /// `base`, its GPIO (LDN 7) at 0xA00 in its second base register, and LDN 0x0A selected.
    pub fn new(port: u16, id: u16, base: u16) -> FakeChip {
        let bases = HashMap::from([(4, [base, 0]), (7, [0, 0x0A00])]);
        FakeChip { port, id, configured: false, keyed_by: true, ldn: 0x0A, bases, index: 0, key: 0 }
    }

    fn key(&self) -> [u8; 4] {
        [0x87, 0x01, 0x55, if self.port == 0x4E { 0xAA } else { 0x55 }]
    }

    fn index_write(&mut self, value: u8) {
        if self.configured {
            self.index = value;
            return;
        }
        let key = self.key();
        self.key = if value == key[self.key] { self.key + 1 } else { usize::from(value == key[0]) };
        if self.key == 4 {
            self.key = 0;
            self.configured = self.keyed_by;
        }
    }

    fn data_write(&mut self, value: u8) {
        if !self.configured {
            return;
        }
        match self.index {
            0x07 => self.ldn = value,
            0x02 if value & 0x02 != 0 => self.configured = false,
            _ => {}
        }
    }

    fn data_read(&self) -> u8 {
        if !self.configured {
            return 0xFF;
        }
        let base = |i: usize| self.bases.get(&self.ldn).map_or(0, |b| b[i]);
        match self.index {
            0x20 => (self.id >> 8) as u8,
            0x21 => self.id as u8,
            0x07 => self.ldn,
            0x60 => (base(0) >> 8) as u8,
            0x61 => base(0) as u8,
            0x62 => (base(1) >> 8) as u8,
            0x63 => base(1) as u8,
            _ => 0,
        }
    }
}

/// What a test has happen as a register is read: given the register and the register file.
pub type OnRead = Box<dyn FnMut(u8, &mut [u8; 256]) + Send>;

/// An ITE environment controller: its registers at `base` (index at base+5, data at base+6).
pub struct FakeHwm {
    pub base: u16,
    /// Bank 0, and the rest of the registers; reg 0x06 bits 6:5 is the bank.
    pub regs: [u8; 256],
    /// Doesn't read its index back (IT8688E): base+5 reads 0xFF.
    pub no_readback: bool,
    /// Registers whose index reads back wrong.
    pub bad_readback: HashSet<u8>,
    pub on_read: Option<OnRead>,
    index: u8,
}

impl FakeHwm {
    pub fn new(base: u16) -> FakeHwm {
        FakeHwm { base, regs: [0; 256], no_readback: false, bad_readback: HashSet::new(), on_read: None, index: 0 }
    }

    fn read(&mut self) -> u8 {
        let reg = self.index;
        // registers 0x10-0xAF are banked: in another bank than 0 they're something else (zero here)
        let banked = (0x10..=0xAF).contains(&reg) && (self.regs[0x06] >> 5) & 3 != 0;
        let value = if banked { 0 } else { self.regs[reg as usize] };
        if let Some(hook) = &mut self.on_read {
            hook(reg, &mut self.regs);
        }
        value
    }

    fn index_read(&self) -> u8 {
        if self.no_readback {
            0xFF
        } else if self.bad_readback.contains(&self.index) {
            self.index ^ 0x40
        } else {
            self.index
        }
    }
}

/// The bus: its chips, its environment controllers, and everything done on it.
pub struct Bus {
    pub chips: Vec<FakeChip>,
    pub hwms: Vec<FakeHwm>,
    /// Every port write (port, value), in order.
    pub writes: Vec<(u16, u8)>,
    /// Every module function called, in order, with its arguments, and the writes it made (by their place
    /// in `writes`).
    pub calls: Vec<(String, Vec<u64>)>,
    pub spans: Vec<std::ops::Range<usize>>,
    /// What to do after the write with this number (from 1): ask the process to stop, in a test.
    pub after_write: Option<(usize, Box<dyn FnMut() + Send>)>,
    /// What the driver says to a module function once it has run, given its name and arguments: Some(error)
    /// fails it, though its port reads and writes were done.
    pub fail: Option<Box<dyn FnMut(&str, &[u64]) -> Option<IoError> + Send>>,
    /// Module handles opened.
    pub opens: usize,
}

impl Bus {
    fn outb(&mut self, port: u16, value: u8) {
        self.writes.push((port, value));
        if let Some(chip) = self.chips.iter_mut().find(|c| c.port == port) {
            chip.index_write(value);
        } else if let Some(chip) = self.chips.iter_mut().find(|c| c.port + 1 == port) {
            chip.data_write(value);
        } else if let Some(hwm) = self.hwms.iter_mut().find(|h| h.base + 5 == port) {
            hwm.index = value;
        } else if let Some(hwm) = self.hwms.iter_mut().find(|h| h.base + 6 == port) {
            hwm.regs[hwm.index as usize] = value;
        }
        if let Some((n, then)) = &mut self.after_write
            && *n == self.writes.len()
        {
            then();
        }
    }

    fn inb(&mut self, port: u16) -> u8 {
        if let Some(chip) = self.chips.iter().find(|c| c.port + 1 == port) {
            chip.data_read()
        } else if let Some(hwm) = self.hwms.iter().find(|h| h.base + 5 == port) {
            hwm.index_read()
        } else if let Some(hwm) = self.hwms.iter_mut().find(|h| h.base + 6 == port) {
            hwm.read()
        } else {
            0xFF
        }
    }

    pub fn chip(&mut self, port: u16) -> &mut FakeChip {
        self.chips.iter_mut().find(|c| c.port == port).unwrap()
    }

    pub fn hwm(&mut self, base: u16) -> &mut FakeHwm {
        self.hwms.iter_mut().find(|h| h.base == base).unwrap()
    }
}

/// This PC's IT8696E, every register the hardware probe read in its first sample (probe-all.json,
/// `chips[0].regs[0]`): fans 1-3 on (0x13 = 0x77), 4 and 5 on and 6 off (0x0C = 0x30), bank 0, temperatures
/// 37, 47, 72, 44 and 51 °C with the 6th absent (0xC9), and fan counts 285, 553, 570 and 577, with fan 5
/// stopped (0xFFFF).
const IT8696E_REGS: [(u8, u8); 45] = [
    (0x00, 0x19), (0x06, 0x00), (0x0C, 0x30), (0x0D, 0x1D), (0x0E, 0x29), (0x0F, 0x3A), (0x13, 0x77), (0x15, 0xD0),
    (0x16, 0xC0), (0x17, 0xC0), (0x18, 0x01), (0x19, 0x02), (0x1A, 0x02), (0x20, 0x71), (0x21, 0xAC), (0x22, 0xA9),
    (0x23, 0xAB), (0x24, 0x69), (0x25, 0x60), (0x26, 0x78), (0x27, 0x8A), (0x28, 0x84), (0x29, 0x25), (0x2A, 0x2F),
    (0x2B, 0x48), (0x2C, 0x2C), (0x2D, 0x33), (0x2E, 0xC9), (0x2F, 0x80), (0x4C, 0x00), (0x4D, 0x00), (0x58, 0x90),
    (0x63, 0x42), (0x6B, 0x3F), (0x73, 0x3F), (0x7B, 0x3F), (0x7F, 0xC0), (0x80, 0x41), (0x81, 0x02), (0x82, 0xFF),
    (0x83, 0xFF), (0xA3, 0x42), (0xA7, 0xD0), (0xAB, 0x80), (0xAF, 0x98),
];

/// This PC's IT87952E, the same way (`chips[1].regs[0]`): temperatures 40 and 41 °C with the 2nd absent
/// (0xC9), and fan 2, the pump, at count 0x6C (6250 RPM), with fans 1 and 3 stopped.
const IT87952E_REGS: [(u8, u8); 30] = [
    (0x00, 0x19), (0x06, 0x00), (0x0C, 0x00), (0x0D, 0xFF), (0x0E, 0x6C), (0x0F, 0xFF), (0x13, 0x77), (0x15, 0x00),
    (0x16, 0x00), (0x17, 0x02), (0x18, 0xFF), (0x19, 0x00), (0x1A, 0xFF), (0x20, 0xA3), (0x21, 0xFF), (0x22, 0xA5),
    (0x23, 0x09), (0x24, 0xFF), (0x25, 0xB3), (0x26, 0x84), (0x27, 0x99), (0x28, 0x90), (0x29, 0x28), (0x2A, 0xC9),
    (0x2B, 0x29), (0x2F, 0x99), (0x58, 0x90), (0x63, 0xFF), (0x6B, 0xFF), (0x73, 0xA3),
];

/// The LpcIO module over a fake bus. Clones share the bus; each `open` is a module handle of its own,
/// with its own slot and allowed ports, as with the real driver.
#[derive(Clone)]
pub struct FakeLpc(Arc<Mutex<Bus>>);

impl FakeLpc {
    pub fn new(chips: Vec<FakeChip>, hwms: Vec<FakeHwm>) -> FakeLpc {
        FakeLpc(Arc::new(Mutex::new(Bus { chips, hwms, writes: Vec::new(), calls: Vec::new(), spans: Vec::new(), after_write: None,
                                         fail: None, opens: 0 })))
    }

    /// This PC as the hardware probe found it (its first sample): an IT8696E at 0x2E, which we key, with its
    /// environment controller at 0xA40, and an IT87952E at 0x4E, already in configuration mode as LHM
    /// leaves it, at 0xA60; each with every register the probe read, as it read them.
    pub fn this_pc() -> FakeLpc {
        let mut it8696 = FakeHwm::new(0xA40);
        for (reg, value) in IT8696E_REGS {
            it8696.regs[reg as usize] = value;
        }
        let mut it87952 = FakeHwm::new(0xA60);
        for (reg, value) in IT87952E_REGS {
            it87952.regs[reg as usize] = value;
        }
        let mut first = FakeChip::new(0x2E, 0x8696, 0xA40);
        first.ldn = 0x07;
        let mut second = FakeChip::new(0x4E, 0x8695, 0xA60);
        second.configured = true;
        second.ldn = 0x0F;
        FakeLpc::new(vec![first, second], vec![it8696, it87952])
    }

    /// The bus, to look at or change.
    pub fn bus(&self) -> MutexGuard<'_, Bus> {
        self.0.lock().unwrap()
    }

    /// Every port write so far.
    pub fn writes(&self) -> Vec<(u16, u8)> {
        self.bus().writes.clone()
    }

    /// Every module function called so far, by name.
    pub fn calls(&self) -> Vec<String> {
        self.bus().calls.iter().map(|(f, _)| f.clone()).collect()
    }

    /// A module handle: no slot selected yet, no ports allowed.
    pub fn open(&self) -> LpcHandle {
        self.bus().opens += 1;
        LpcHandle { bus: self.0.clone(), port: Mutex::new(None), bars: Mutex::new(Vec::new()) }
    }
}

/// One LpcIO module handle.
pub struct LpcHandle {
    bus: Arc<Mutex<Bus>>,
    port: Mutex<Option<u16>>,
    bars: Mutex<Vec<u16>>,
}

impl LpcHandle {
    fn allowed(&self, port: u16) -> bool {
        let Some(config) = *self.port.lock().unwrap() else { return false };
        if (0xCF8..=0xCFF).contains(&port) {
            return false;
        }
        port == config || port == config + 1 || port == 0x25C || port == 0x25D || self.bars.lock().unwrap().contains(&(port & 0xFFF8))
    }

    /// LpcIO.p's find_bars: every logical device's base registers, twice, keeping those that held still.
    fn find_bars(&self, bus: &mut Bus, port: u16) -> Result<(), IoError> {
        let inb = |bus: &mut Bus, reg: u8| {
            bus.outb(port, reg);
            bus.inb(port + 1)
        };
        let inw = |bus: &mut Bus, reg: u8| (inb(bus, reg) as u16) << 8 | inb(bus, reg + 1) as u16;
        let select = |bus: &mut Bus, ldn: u8| {
            bus.outb(port, 0x07);
            bus.outb(port + 1, ldn);
            inb(bus, 0x07) == ldn
        };
        let id = inb(bus, 0x20);
        if id == 0x00 || id == 0xFF {
            return Err(IoError::Other(1168));
        }
        let mut first = [[0u16; 2]; 255];
        for ldn in 0..0xFF {
            if select(bus, ldn) {
                first[ldn as usize] = [inw(bus, 0x60), inw(bus, 0x62)];
            }
        }
        let mut bars = self.bars.lock().unwrap();
        bars.clear();
        for ldn in 0..0xFF {
            if select(bus, ldn) {
                for (i, value) in [inw(bus, 0x60), inw(bus, 0x62)].into_iter().enumerate() {
                    let mut bar = first[ldn as usize][i];
                    if bar != value || bar == 0 || bar == 0xFFFF || bar < 0x100 {
                        continue;
                    }
                    if bar & 7 == 5 {
                        bar &= 0xFFF8;
                    }
                    if !bars.contains(&bar) {
                        bars.push(bar);
                    }
                }
            }
        }
        Ok(())
    }
}

impl Exec for LpcHandle {
    fn execute(&self, function: &str, input: &[u64], out: &mut [u64]) -> Result<(), IoError> {
        let mut bus = self.bus.lock().unwrap();
        bus.calls.push((function.to_string(), input.to_vec()));
        let before = bus.writes.len();
        let result = self.run(&mut bus, function, input, out);
        let after = bus.writes.len();
        bus.spans.push(before..after);
        match bus.fail.as_mut().and_then(|fail| fail(function, input)) {
            Some(e) => Err(e),
            None => result,
        }
    }
}

impl LpcHandle {
    fn run(&self, bus: &mut Bus, function: &str, input: &[u64], out: &mut [u64]) -> Result<(), IoError> {
        let sized = |ins: usize, outs: usize| if input.len() == ins && out.len() == outs { Ok(()) } else { Err(IoError::BadArguments) };
        if function == "ioctl_select_slot" {
            sized(1, 0)?;
            *self.bars.lock().unwrap() = Vec::new();
            *self.port.lock().unwrap() = match input[0] {
                0 => Some(0x2E),
                1 => Some(0x4E),
                _ => return Err(IoError::BadArguments),
            };
            return Ok(());
        }
        let Some(port) = *self.port.lock().unwrap() else {
            return if function.starts_with("ioctl_") { Err(IoError::NotReady) } else { Err(IoError::NoSuchFunction) };
        };
        match function {
            "ioctl_find_bars" => {
                sized(0, 0)?;
                self.find_bars(bus, port)
            }
            "ioctl_pio_inb" => {
                sized(1, 1)?;
                let at = input[0] as u16;
                if !self.allowed(at) {
                    return Err(IoError::AccessDenied);
                }
                out[0] = bus.inb(at) as u64;
                Ok(())
            }
            "ioctl_pio_outb" => {
                sized(2, 0)?;
                let at = input[0] as u16;
                if !self.allowed(at) {
                    return Err(IoError::AccessDenied);
                }
                bus.outb(at, input[1] as u8);
                Ok(())
            }
            "ioctl_superio_inb" => {
                sized(1, 1)?;
                bus.outb(port, input[0] as u8);
                out[0] = bus.inb(port + 1) as u64;
                Ok(())
            }
            "ioctl_superio_inw" => {
                sized(1, 1)?;
                let reg = input[0] as u8;
                bus.outb(port, reg);
                let high = bus.inb(port + 1) as u64;
                bus.outb(port, reg.wrapping_add(1));
                out[0] = high << 8 | bus.inb(port + 1) as u64;
                Ok(())
            }
            "ioctl_superio_outb" => {
                sized(2, 0)?;
                bus.outb(port, input[0] as u8);
                bus.outb(port + 1, input[1] as u8);
                Ok(())
            }
            _ => Err(IoError::NoSuchFunction),
        }
    }
}

/// A shared lock: free unless the test makes it busy. It keeps the wait each acquire asked for, and how
/// many are held now.
#[derive(Clone, Default)]
pub struct FakeLock(Arc<Mutex<LockState>>);

#[derive(Default)]
struct LockState {
    busy: bool,
    waits: Vec<u32>,
    held: usize,
}

impl FakeLock {
    pub fn set_busy(&self, busy: bool) {
        self.0.lock().unwrap().busy = busy;
    }

    pub fn waits(&self) -> Vec<u32> {
        self.0.lock().unwrap().waits.clone()
    }

    pub fn held(&self) -> usize {
        self.0.lock().unwrap().held
    }
}

impl Lock for FakeLock {
    fn acquire(&self, timeout_ms: u32) -> Option<Held<'_>> {
        let mut state = self.0.lock().unwrap();
        state.waits.push(timeout_ms);
        if state.busy {
            return None;
        }
        state.held += 1;
        Some(Held::new(move || self.0.lock().unwrap().held -= 1))
    }
}

/// A lock that's always free.
pub struct NoLock;

impl Lock for NoLock {
    fn acquire(&self, _timeout_ms: u32) -> Option<Held<'_>> {
        Some(Held::new(|| {}))
    }
}

/// A clock that moves only when the test moves it. Clones share it.
#[derive(Clone, Default)]
pub struct FakeClock(Arc<Mutex<(Duration, Duration)>>);

impl FakeClock {
    /// Time passes, awake.
    pub fn advance(&self, time: Duration) {
        let mut clocks = self.0.lock().unwrap();
        clocks.0 += time;
        clocks.1 += time;
    }

    /// Time passes, asleep: only the clock that counts sleep moves.
    pub fn suspend(&self, time: Duration) {
        self.0.lock().unwrap().0 += time;
    }
}

impl Clock for FakeClock {
    fn since_boot(&self) -> Duration {
        self.0.lock().unwrap().0
    }

    fn awake(&self) -> Duration {
        self.0.lock().unwrap().1
    }

    fn sleep(&self, time: Duration) {
        self.advance(time);
    }
}
