//! AMD family 17h to 1Ah, through the AMDFamily17 module: Tctl, each CCD's temperature, and the package
//! power from the energy counter.
//!
//! The temperatures are SMN registers (read through PCI 0:0.0, so under the PCI lock shared with other
//! monitoring programs), decoded as Linux's k10temp does; the power is the package's energy counter, an MSR
//! that needs no lock. Only reads: the module's SMN read writes the SMN address to PCI 0:0.0 0x60, which
//! is the only write here.

use std::time::Duration;

use super::keys::Cpu;
use super::mutex::Bus;
use super::reader::{Poll, io_text};
use super::{Exec, IoError, Lock};

/// Where Tctl is, and where the CCDs' temperatures start (`ccd_slots` gives the offset from it).
const TCTL: u32 = 0x0005_9800;
/// The energy counter's unit, and the package's energy counter (its low 32 bits count).
const PWR_UNIT: u32 = 0xC001_0299;
const PKG_ENERGY: u32 = 0xC001_029B;

/// Tctl's flags that say it reads 49 °C high: RANGE_SEL, or both TJ_SEL bits.
const RANGE_SEL: u32 = 0x8_0000;
const TJ_SEL: u32 = 0x3_0000;
/// A CCD's temperature is valid when this bit is set.
const CCD_VALID: u32 = 0x800;

/// Temperatures outside this range are another register's value (another program moved the SMN index
/// between the module's write and its read), never the CPU's.
const PLAUSIBLE: std::ops::RangeInclusive<f64> = -20.0..=130.0;
/// And power outside this, a counter that was reset.
const WATTS: std::ops::RangeInclusive<f64> = 0.0..=1000.0;
/// Power is worked out over at least this long, so a read straight after another (a Test) doesn't make
/// it noisy.
const SHORTEST: Duration = Duration::from_millis(200);
/// And at most this: the 32-bit counter wraps after 65 536 J, 120 s at 546 W, so a longer gap could hide a
/// wrap.
const LONGEST: Duration = Duration::from_secs(120);
/// How often the CCD slots that weren't valid are tried again, as a CCD can be off at first.
const REPROBE: Duration = Duration::from_secs(60);

/// The lock's wait, as the other programs that share it wait.
const LOCK_MS: u32 = 10;
/// Skips in a row, for the lock, before a note.
const BUSY_NOTE: u32 = 3;

pub(crate) const BUSY: &str = "Another program kept the CPU's temperature sensors busy, so they were skipped this time.";
pub(crate) const LONG_INTERVAL: &str = "Set the pc interval to 2 minutes or less for CPU power.";

/// What CPUID says about the CPU.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cpuid {
    /// "AuthenticAMD", "GenuineIntel"...
    pub vendor: String,
    pub family: u32,
    pub model: u32,
    /// "AMD Ryzen 9 9950X3D 16-Core Processor"
    pub brand: String,
}

impl Cpuid {
    /// This PC's CPU.
    pub fn current() -> Cpuid {
        #[allow(unused_unsafe)]
        let leaf = |n: u32| {
            let r = unsafe { std::arch::x86_64::__cpuid(n) };
            [r.eax, r.ebx, r.ecx, r.edx]
        };
        let highest = leaf(0x8000_0000)[0];
        let brand = if highest >= 0x8000_0004 { [leaf(0x8000_0002), leaf(0x8000_0003), leaf(0x8000_0004)] } else { [[0; 4]; 3] };
        Cpuid::from_leaves(leaf(0), leaf(1)[0], brand)
    }

    /// From CPUID leaf 0 (eax, ebx, ecx, edx), leaf 1's eax, and leaves 0x80000002 to 4 (the brand).
    pub fn from_leaves(zero: [u32; 4], one: u32, brand: [[u32; 4]; 3]) -> Cpuid {
        let text = |words: &[u32]| {
            let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).take_while(|&b| b != 0).collect();
            String::from_utf8_lossy(&bytes).trim().to_string()
        };
        let base_family = (one >> 8) & 0xF;
        let family = if base_family == 0xF { base_family + ((one >> 20) & 0xFF) } else { base_family };
        let model = if base_family == 0xF || base_family == 0x6 { (((one >> 16) & 0xF) << 4) | ((one >> 4) & 0xF) }
                    else { (one >> 4) & 0xF };
        Cpuid { vendor: text(&[zero[1], zero[3], zero[2]]), family, model, brand: text(brand.as_flattened()) }
    }

    /// Whether the AMDFamily17 module reads it: AMD, family 17h to 1Ah.
    pub fn amd_zen(&self) -> bool {
        self.vendor == "AuthenticAMD" && (0x17..=0x1A).contains(&self.family)
    }
}

/// Tctl from its register, in °C: None when the read failed (all ones) or gave what can't be a temperature.
pub fn tctl(raw: u32) -> Option<f64> {
    if raw == u32::MAX {
        return None;
    }
    let mut t = (raw >> 21) as f64 * 0.125;
    if raw & RANGE_SEL != 0 || raw & TJ_SEL == TJ_SEL {
        t -= 49.0;
    }
    Some(t).filter(|t| PLAUSIBLE.contains(t))
}

/// What Tdie is below Tctl, for the family 17h parts that read Tctl high (k10temp's table, matched in the
/// brand): None for every other CPU, where Tctl is the die's temperature.
pub fn tdie_offset(cpu: &Cpuid) -> Option<f64> {
    const OFFSETS: [(&str, f64); 6] = [
        ("AMD Ryzen 5 1600X", 20.0), ("AMD Ryzen 7 1700X", 20.0), ("AMD Ryzen 7 1800X", 20.0), ("AMD Ryzen 7 2700X", 10.0),
        ("AMD Ryzen Threadripper 19", 27.0), ("AMD Ryzen Threadripper 29", 27.0),
    ];
    if cpu.family != 0x17 {
        return None;
    }
    OFFSETS.iter().find(|(name, _)| cpu.brand.contains(name)).map(|(_, offset)| *offset)
}

/// Where a CPU's CCD temperatures are: their offset from Tctl's register, and how many slots there are to
/// look in (k10temp's table). None for a model without them.
pub fn ccd_slots(family: u32, model: u32) -> Option<(u32, usize)> {
    match (family, model) {
        (0x17 | 0x18, 0x01 | 0x08 | 0x11 | 0x18) => Some((0x154, 4)),
        (0x17 | 0x18, 0x31 | 0x47 | 0x60 | 0x68 | 0x71) => Some((0x154, 8)),
        (0x17 | 0x18, 0xA0..=0xAF) => Some((0x300, 8)),
        (0x19, 0x00..=0x01 | 0x08 | 0x21 | 0x50..=0x5F) => Some((0x154, 8)),
        (0x19, 0x40..=0x4F) => Some((0x300, 8)),
        (0x19, 0x60..=0x7F) => Some((0x308, 8)),
        (0x19, 0x10..=0x1F | 0xA0..=0xAF) => Some((0x300, 12)),
        (0x1A, 0x00..=0x1F) => Some((0x1F0, 16)),
        (0x1A, 0x40..=0x4F) => Some((0x308, 8)),
        _ => None,
    }
}

/// Whether a CCD slot's register holds a temperature: the read worked, and the CCD says it's valid.
pub fn ccd_valid(raw: u32) -> bool {
    raw != u32::MAX && raw & CCD_VALID != 0
}

/// A CCD's temperature from its register, in °C: None unless it's valid and could be a temperature.
pub fn ccd(raw: u32) -> Option<f64> {
    if !ccd_valid(raw) {
        return None;
    }
    Some((raw & 0x7FF) as f64 * 0.125 - 49.0).filter(|t| PLAUSIBLE.contains(t))
}

/// The energy counter's unit in joules, from MSR_PWR_UNIT (bits 12:8).
pub fn energy_unit(raw: u64) -> f64 {
    1.0 / (1u64 << ((raw >> 8) & 0x1F)) as f64
}

/// Reads an AMD Ryzen through its module.
pub struct Amd {
    exec: Box<dyn Exec>,
    cpu: Cpuid,
    tdie_offset: Option<f64>,
    ccds: Option<(u32, usize)>,
    /// The CCD slots found valid, and when the others were last looked at (None: not yet, as the lock
    /// hasn't been had).
    kept: Vec<usize>,
    probed_at: Option<Duration>,
    /// Reads in a row that the lock was busy for.
    busy: u32,
    /// The energy counter's unit, once read, and the counter at the last read power was worked out from.
    unit: Option<f64>,
    last: Option<(u32, Duration)>,
}

impl Amd {
    pub fn new(exec: Box<dyn Exec>, cpu: Cpuid) -> Amd {
        Amd { exec, tdie_offset: tdie_offset(&cpu), ccds: ccd_slots(cpu.family, cpu.model), cpu, kept: Vec::new(),
              probed_at: None, busy: 0, unit: None, last: None }
    }

    pub fn cpu(&self) -> &Cpuid {
        &self.cpu
    }

    /// The CCD slots found valid so far.
    pub fn ccds(&self) -> &[usize] {
        &self.kept
    }

    /// One SMN register.
    pub fn smn(&self, address: u32) -> Result<u32, IoError> {
        let mut out = [0u64];
        self.exec.execute("ioctl_read_smn", &[address as u64], &mut out)?;
        Ok(out[0] as u32)
    }

    /// One MSR.
    pub fn msr(&self, msr: u32) -> Result<u64, IoError> {
        let mut out = [0u64];
        self.exec.execute("ioctl_read_msr", &[msr as u64], &mut out)?;
        Ok(out[0])
    }

    /// The temperatures (under `pci`, the shared lock) and the power. What can't be read this time is left
    /// out, with a note when it needs one.
    pub fn read(&mut self, poll: &Poll, pci: Option<&dyn Lock>, out: &mut Vec<(Cpu, f64)>, notes: &mut Vec<String>) {
        match pci {
            Some(pci) => self.temperatures(poll, pci, out, notes),
            None => notes.push(Bus::Pci.unshared()),
        }
        self.power(poll, out, notes);
    }

    fn temperatures(&mut self, poll: &Poll, pci: &dyn Lock, out: &mut Vec<(Cpu, f64)>, notes: &mut Vec<String>) {
        let Some(_shared) = pci.acquire(LOCK_MS) else {
            self.busy += 1;
            if self.busy >= BUSY_NOTE {
                notes.push(BUSY.to_string());
            }
            return;
        };
        self.busy = 0;
        let _hw = poll.hw.hold();
        if poll.hw.stopping() {
            return;
        }
        let failed = |e: IoError| format!("Reading the CPU's temperatures through PawnIO failed ({}), so they were skipped this time.", io_text(e));
        match self.smn(TCTL) {
            Ok(raw) => {
                if let Some(t) = tctl(raw) {
                    out.push((Cpu::Tctl, t));
                    if let Some(offset) = self.tdie_offset {
                        out.push((Cpu::Tdie, t - offset));
                    }
                }
            }
            Err(e) => return notes.push(failed(e)),
        }
        let Some((offset, slots)) = self.ccds else { return };
        // every slot the first time the lock is had, then the slots not found every so often
        let probe = self.probed_at.is_none_or(|at| poll.now.saturating_sub(at) >= REPROBE);
        let mut ccds = Vec::new();
        for slot in 0..slots {
            let kept = self.kept.contains(&slot);
            if !kept && !probe {
                continue;
            }
            if poll.hw.stopping() {
                return;
            }
            let raw = match self.smn(TCTL + offset + 4 * slot as u32) {
                Ok(raw) => raw,
                Err(e) => return notes.push(failed(e)),
            };
            // a kept slot that isn't valid now is left out this time, never read as -49 °C
            if ccd_valid(raw) && !kept {
                self.kept.push(slot);
            }
            if let Some(t) = ccd(raw) {
                ccds.push((slot, t));
            }
        }
        if probe {
            self.probed_at = Some(poll.now);
            self.kept.sort_unstable();
        }
        out.extend(ccds.iter().map(|&(slot, t)| (Cpu::Ccd(slot), t)));
        if ccds.len() >= 2 {
            let temps = ccds.iter().map(|(_, t)| *t);
            out.push((Cpu::CcdMax, temps.clone().fold(f64::MIN, f64::max)));
            out.push((Cpu::CcdAverage, temps.sum::<f64>() / ccds.len() as f64));
        }
    }

    /// The package's power over the time since the last read it was worked out from.
    fn power(&mut self, poll: &Poll, out: &mut Vec<(Cpu, f64)>, notes: &mut Vec<String>) {
        if self.unit.is_none() {
            self.unit = self.msr(PWR_UNIT).ok().map(energy_unit);
        }
        let (Some(unit), Ok(raw)) = (self.unit, self.msr(PKG_ENERGY)) else { return };
        let (energy, now) = (raw as u32, poll.now);
        let Some((last, then)) = self.last else {
            self.last = Some((energy, now));
            return;
        };
        let gap = now.saturating_sub(then);
        let slept = poll.resumed_at.is_some_and(|at| at > then);
        if slept || gap > LONGEST {
            // start again from here: over a sleep or so long a gap, the counter may have wrapped or been reset
            self.last = Some((energy, now));
            if !slept {
                notes.push(LONG_INTERVAL.to_string());
            }
            return;
        }
        if gap < SHORTEST {
            return;
        }
        let watts = energy.wrapping_sub(last) as f64 * unit / gap.as_secs_f64();
        if WATTS.contains(&watts) {
            out.push((Cpu::Package, watts));
            self.last = Some((energy, now));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pawnio::fake::{FakeAmd, FakeClock, FakeLock};
    use crate::pawnio::reader::{Clock, HwLock};

    /// A Ryzen 9 9950X3D (family 1Ah, model 0x44) on fakes the test changes.
    struct Rig {
        fake: FakeAmd,
        amd: Amd,
        lock: FakeLock,
        clock: FakeClock,
        hw: HwLock,
        resumed_at: Option<Duration>,
    }

    impl Rig {
        fn new(cpu: Cpuid) -> Rig {
            let fake = FakeAmd::default();
            fake.set_msr(PWR_UNIT, 0x0000_1000);
            Rig { amd: Amd::new(Box::new(fake.clone()), cpu), fake, lock: FakeLock::default(), clock: FakeClock::default(),
                  hw: HwLock::default(), resumed_at: None }
        }

        fn ryzen() -> Rig {
            Rig::new(Cpuid { vendor: "AuthenticAMD".into(), family: 0x1A, model: 0x44, brand: "AMD Ryzen 9 9950X3D 16-Core Processor".into() })
        }

        fn read(&mut self) -> (Vec<(Cpu, f64)>, Vec<String>) {
            let poll = Poll { hw: &self.hw, clock: &self.clock, now: self.clock.since_boot(), resumed_at: self.resumed_at };
            let (mut out, mut notes) = (Vec::new(), Vec::new());
            self.amd.read(&poll, Some(&self.lock), &mut out, &mut notes);
            (out, notes)
        }

        fn temps(&mut self) -> Vec<(Cpu, f64)> {
            self.read().0.into_iter().filter(|(c, _)| *c != Cpu::Package).collect()
        }

        fn power(&mut self) -> Option<f64> {
            self.read().0.into_iter().find(|(c, _)| *c == Cpu::Package).map(|(_, w)| w)
        }

        fn ccd(&self, slot: u32, raw: u32) {
            self.fake.set_smn(0x59B08 + 4 * slot, raw);
        }
    }

    /// Tctl's vectors from k10temp and LHM: the 49 °C flags, a failed read, and what can't be a temperature.
    #[test]
    fn tctl_decodes() {
        assert_eq!(tctl(0x4660_0000), Some(70.375));
        assert_eq!(tctl(0x4668_0000), Some(21.375)); // RANGE_SEL
        assert_eq!(tctl(0x4663_0000), Some(21.375)); // TJ_SEL = 11
        assert_eq!(tctl(0x4662_0000), Some(70.375)); // TJ_SEL = 10 isn't the flag
        assert_eq!(tctl(0xFFFF_FFFF), None);
        assert_eq!(tctl(1048 << 21), None); // 131 °C
        assert_eq!(tctl(1048 << 21 | RANGE_SEL), Some(82.0));
        assert_eq!(tctl(224 << 21 | RANGE_SEL), None); // -21 °C
        assert_eq!(tctl(232 << 21 | RANGE_SEL), Some(-20.0));
        assert_eq!(tctl(1040 << 21), Some(130.0));
    }

    /// A CCD's register: valid only with its bit set and a read that worked.
    #[test]
    fn ccd_decodes() {
        assert_eq!(ccd(0xB15), Some(49.625));
        assert_eq!(ccd(0x315), None); // VALID clear
        assert_eq!(ccd(0xFFFF_FFFF), None);
        assert_eq!(ccd(0), None);
        assert_eq!(ccd(0x800), None); // -49 °C, valid or not
        assert!(ccd_valid(0x800) && !ccd_valid(0xFFFF_FFFF) && !ccd_valid(0x7FF));
    }

    /// Tdie only for the family 17h parts in k10temp's table; Tctl is the die's temperature on the rest.
    #[test]
    fn tdie_offsets() {
        let cpu = |family, brand: &str| Cpuid { vendor: "AuthenticAMD".into(), family, model: 1, brand: brand.into() };
        assert_eq!(tdie_offset(&cpu(0x17, "AMD Ryzen 7 1800X Eight-Core Processor")), Some(20.0));
        assert_eq!(tdie_offset(&cpu(0x17, "AMD Ryzen 7 2700X Eight-Core Processor")), Some(10.0));
        assert_eq!(tdie_offset(&cpu(0x17, "AMD Ryzen Threadripper 2990WX 32-Core Processor")), Some(27.0));
        assert_eq!(tdie_offset(&cpu(0x17, "AMD Ryzen 7 3700X 8-Core Processor")), None);
        assert_eq!(tdie_offset(&cpu(0x19, "AMD Ryzen 7 1800X")), None);
        assert_eq!(tdie_offset(&cpu(0x1A, "AMD Ryzen 9 9950X3D 16-Core Processor")), None);

        let mut rig = Rig::new(cpu(0x17, "AMD Ryzen 7 1800X Eight-Core Processor"));
        rig.fake.set_smn(TCTL, 0x4660_0000);
        assert_eq!(rig.temps(), [(Cpu::Tctl, 70.375), (Cpu::Tdie, 50.375)]);
    }

    /// k10temp's CCD table, by family and model.
    #[test]
    fn ccd_table() {
        assert_eq!(ccd_slots(0x1A, 0x44), Some((0x308, 8))); // this PC: SMN 0x59B08 + 4i
        assert_eq!(ccd_slots(0x1A, 0x10), Some((0x1F0, 16)));
        assert_eq!(ccd_slots(0x1A, 0x24), None);
        assert_eq!(ccd_slots(0x19, 0x61), Some((0x308, 8)));
        assert_eq!(ccd_slots(0x19, 0x21), Some((0x154, 8)));
        assert_eq!(ccd_slots(0x19, 0x11), Some((0x300, 12)));
        assert_eq!(ccd_slots(0x19, 0x44), Some((0x300, 8)));
        assert_eq!(ccd_slots(0x17, 0x71), Some((0x154, 8)));
        assert_eq!(ccd_slots(0x17, 0x08), Some((0x154, 4)));
        assert_eq!(ccd_slots(0x17, 0xA5), Some((0x300, 8)));
        assert_eq!(ccd_slots(0x17, 0x30), None);
    }

    /// CPUID's family and model: the 9950X3D's leaf 1 is family 1Ah, model 0x44.
    #[test]
    fn cpuid_decodes() {
        let words = |s: &str| -> Vec<u32> { s.as_bytes().chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect() };
        let vendor = words("AuthenticAMD");
        let mut brand = words("AMD Ryzen 9 9950X3D 16-Core Processor\0\0\0\0\0\0\0\0\0\0\0");
        brand.resize(12, 0);
        let brand = [[brand[0], brand[1], brand[2], brand[3]], [brand[4], brand[5], brand[6], brand[7]],
                     [brand[8], brand[9], brand[10], brand[11]]];
        let cpu = Cpuid::from_leaves([0x10, vendor[0], vendor[2], vendor[1]], 0x00B4_0F40, brand);
        assert_eq!(cpu, Cpuid { vendor: "AuthenticAMD".into(), family: 0x1A, model: 0x44,
                                brand: "AMD Ryzen 9 9950X3D 16-Core Processor".into() });
        assert!(cpu.amd_zen());
        // Zen 2 (family 17h, model 0x71), and an Intel family 6 with an extended model
        assert_eq!((Cpuid::from_leaves([0; 4], 0x0087_0F10, [[0; 4]; 3]).family, Cpuid::from_leaves([0; 4], 0x0087_0F10, [[0; 4]; 3]).model), (0x17, 0x71));
        assert_eq!(Cpuid::from_leaves([0; 4], 0x000B_06A2, [[0; 4]; 3]).model, 0xBA);
        assert!(!Cpuid { vendor: "AuthenticAMD".into(), family: 0x15, ..Cpuid::default() }.amd_zen());
        // and this PC's own, which needs no PawnIO
        let here = Cpuid::current();
        assert!(!here.vendor.is_empty() && here.family > 0);
    }

    /// The CCDs valid at the first read are read; max and average need two.
    #[test]
    fn ccds() {
        let mut rig = Rig::ryzen();
        rig.fake.set_smn(TCTL, 0x4660_0000);
        rig.ccd(0, 0xB15);
        rig.ccd(1, 0x800 | 872);
        rig.ccd(2, 0x315); // not valid at the probe: not kept
        rig.ccd(3, 0xFFFF_FFFF); // a failed read: not kept
        assert_eq!(rig.temps(), [(Cpu::Tctl, 70.375), (Cpu::Ccd(0), 49.625), (Cpu::Ccd(1), 60.0), (Cpu::CcdMax, 60.0),
                                 (Cpu::CcdAverage, 54.8125)]);
        assert_eq!(rig.amd.ccds(), [0, 1]);
        // after the probe, only the kept slots are read
        let before = rig.fake.calls().len();
        rig.temps();
        let smn: Vec<u64> = rig.fake.calls()[before..].iter().filter(|(f, _)| f == "ioctl_read_smn").map(|(_, a)| *a).collect();
        assert_eq!(smn, [0x59800, 0x59B08, 0x59B0C]);
    }

    /// A kept CCD that reads 0 or isn't valid is left out that time, never -49 °C, and max and average are
    /// over the rest; with one left, there's neither.
    #[test]
    fn ccd_gone_invalid_is_left_out() {
        let mut rig = Rig::ryzen();
        rig.fake.set_smn(TCTL, 0x4660_0000);
        for slot in 0..3 {
            rig.ccd(slot, 0xB15 + slot * 8);
        }
        assert_eq!(rig.temps().len(), 1 + 3 + 2);
        rig.ccd(2, 0);
        assert_eq!(rig.temps(), [(Cpu::Tctl, 70.375), (Cpu::Ccd(0), 49.625), (Cpu::Ccd(1), 50.625), (Cpu::CcdMax, 50.625),
                                 (Cpu::CcdAverage, 50.125)]);
        rig.ccd(1, 0x315);
        assert_eq!(rig.temps(), [(Cpu::Tctl, 70.375), (Cpu::Ccd(0), 49.625)]);
        // and back when valid again
        rig.ccd(1, 0xB15);
        rig.ccd(2, 0xB15);
        assert_eq!(rig.temps().len(), 1 + 3 + 2);
    }

    /// A slot that wasn't valid at first is looked at again a minute on, and read from then.
    #[test]
    fn ccd_valid_later_appears() {
        let mut rig = Rig::ryzen();
        rig.ccd(0, 0xB15);
        rig.ccd(1, 0x315);
        assert_eq!(rig.temps(), [(Cpu::Ccd(0), 49.625)]);
        rig.ccd(1, 0xB15);
        rig.clock.advance(Duration::from_secs(59));
        assert_eq!(rig.temps(), [(Cpu::Ccd(0), 49.625)]);
        rig.clock.advance(Duration::from_secs(1));
        assert_eq!(rig.temps(), [(Cpu::Ccd(0), 49.625), (Cpu::Ccd(1), 49.625), (Cpu::CcdMax, 49.625), (Cpu::CcdAverage, 49.625)]);
        // a re-probe reads at most the slots not kept, once
        let before = rig.fake.calls().len();
        rig.clock.advance(Duration::from_secs(60));
        rig.temps();
        assert_eq!(rig.fake.calls()[before..].iter().filter(|(f, _)| f == "ioctl_read_smn").count(), 1 + 8);
    }

    /// A lock busy at the first read: no probe until a read that has it; three busy reads in a row are a
    /// note. The energy counter is read whether or not.
    #[test]
    fn busy_lock() {
        let mut rig = Rig::ryzen();
        rig.ccd(0, 0xB15);
        rig.lock.set_busy(true);
        assert_eq!(rig.read(), (vec![], vec![]));
        assert_eq!(rig.read(), (vec![], vec![]));
        assert_eq!(rig.read(), (vec![], vec![BUSY.to_string()]));
        assert!(rig.fake.calls().iter().all(|(f, _)| f == "ioctl_read_msr"));
        assert_eq!(rig.amd.probed_at, None);
        rig.lock.set_busy(false);
        assert_eq!(rig.temps(), [(Cpu::Ccd(0), 49.625)]);
        assert_eq!(rig.lock.waits(), [10, 10, 10, 10]);
        assert_eq!(rig.lock.held(), 0);
    }

    /// Package power: the vectors for the unit, a wrap, a read too soon, too long a gap, a sleep and an
    /// impossible value.
    #[test]
    fn power() {
        assert_eq!(energy_unit(0x0000_1000), 1.0 / 65_536.0);
        assert_eq!(energy_unit(0x000A_0E03), 1.0 / 16_384.0);
        let mut rig = Rig::ryzen();
        rig.fake.set_msr(PKG_ENERGY, 0xFFFF_FFFF_0000_0000);
        assert_eq!(rig.power(), None); // the first read only starts the count
        rig.clock.advance(Duration::from_secs(1));
        rig.fake.set_msr(PKG_ENERGY, 0xFFFF_FFFF_0082_0000);
        assert_eq!(rig.power(), Some(130.0)); // 8 519 680 counts in 1 s; the high 32 bits don't count

        // a wrap: 0xFFFFFF00 to 0x100 is 0x200 counts
        rig.fake.set_msr(PKG_ENERGY, 0xFFFF_FF00);
        rig.clock.advance(Duration::from_secs(150));
        assert_eq!(rig.read(), (vec![], vec![LONG_INTERVAL.to_string()])); // too long: starts again
        rig.fake.set_msr(PKG_ENERGY, 0x100);
        rig.clock.advance(Duration::from_millis(500));
        assert_eq!(rig.power(), Some(0x200 as f64 / 65_536.0 / 0.5));

        // 50 ms on: nothing, and the count isn't moved on, so the next read is over the whole time
        rig.fake.set_msr(PKG_ENERGY, 0x100 + 65_536);
        rig.clock.advance(Duration::from_millis(50));
        assert_eq!(rig.power(), None);
        rig.clock.advance(Duration::from_millis(950));
        rig.fake.set_msr(PKG_ENERGY, 0x100 + 2 * 65_536);
        assert_eq!(rig.power(), Some(2.0));

        // 121 s: nothing, and the count starts again from there
        rig.clock.advance(Duration::from_secs(121));
        rig.fake.set_msr(PKG_ENERGY, 0x100 + 3 * 65_536);
        assert_eq!(rig.power(), None);
        rig.clock.advance(Duration::from_secs(1));
        rig.fake.set_msr(PKG_ENERGY, 0x100 + 4 * 65_536);
        assert_eq!(rig.power(), Some(1.0));

        // a sleep since the last read: nothing, no note, and the count starts again
        rig.clock.advance(Duration::from_secs(10));
        rig.resumed_at = Some(rig.clock.since_boot());
        rig.fake.set_msr(PKG_ENERGY, 0x100 + 900 * 65_536);
        assert_eq!(rig.read(), (vec![], vec![]));
        rig.clock.advance(Duration::from_secs(1));
        rig.fake.set_msr(PKG_ENERGY, 0x100 + 901 * 65_536);
        assert_eq!(rig.power(), Some(1.0));

        // 1 200 W can't be: nothing, and the count isn't moved on
        rig.clock.advance(Duration::from_secs(1));
        rig.fake.set_msr(PKG_ENERGY, 0x100 + 2101 * 65_536);
        assert_eq!(rig.power(), None);
        rig.clock.advance(Duration::from_secs(1));
        rig.fake.set_msr(PKG_ENERGY, 0x100 + 2102 * 65_536);
        assert_eq!(rig.power(), Some(600.5));
    }

    /// The only hardware reached: the SMN registers for Tctl and the CCD slots, and the two energy MSRs.
    /// Never an MSR write.
    #[test]
    fn only_reads() {
        let mut rig = Rig::ryzen();
        rig.ccd(0, 0xB15);
        for _ in 0..3 {
            rig.read();
            rig.clock.advance(Duration::from_secs(61));
        }
        for (function, address) in rig.fake.calls() {
            match function.as_str() {
                "ioctl_read_smn" => assert!(address == 0x59800 || (0x59B08..0x59B08 + 32).contains(&address), "{address:X}"),
                "ioctl_read_msr" => assert!(address == PWR_UNIT as u64 || address == PKG_ENERGY as u64, "{address:X}"),
                other => panic!("{other}"),
            }
        }
    }

    /// An SMN read the module refused is a note, and the power still comes through.
    #[test]
    fn smn_errors_are_notes() {
        let mut rig = Rig::ryzen();
        rig.fake.fail_smn(Some(IoError::AccessDenied));
        rig.fake.set_msr(PKG_ENERGY, 0);
        rig.read();
        rig.clock.advance(Duration::from_secs(1));
        rig.fake.set_msr(PKG_ENERGY, 65_536);
        let (out, notes) = rig.read();
        assert_eq!(out, [(Cpu::Package, 1.0)]);
        assert_eq!(notes, ["Reading the CPU's temperatures through PawnIO failed (access denied), so they were skipped this time."]);
    }

    /// Once the process is ending, no SMN read starts.
    #[test]
    fn stops() {
        let mut rig = Rig::ryzen();
        rig.hw.ask_to_stop();
        rig.read();
        assert!(rig.fake.calls().iter().all(|(f, _)| f == "ioctl_read_msr"));
    }
}
