//! Names for people: a board's own, from a map compiled in, where its board is known; generic ones
//! otherwise.
//!
//! A board is known by the names Windows keeps for it (`Board::current`), matched exactly but for case. Its
//! map names what each chip channel is wired to, as on the board ("System Fan #6 / Pump"), copied from Libre
//! Hardware Monitor's maps for the same boards. A board gets a map only once its channels are known to be
//! the same as the map's: a wrong name is worse than a generic one.

use super::keys::{Channel, Cpu, Sensor};

/// The board, as Windows names it (`HKLM\HARDWARE\DESCRIPTION\System\BIOS`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Board {
    pub manufacturer: String,
    pub product: String,
}

impl Board {
    /// This PC's board, read from the registry (no administrator rights needed); empty names when Windows
    /// has none.
    pub fn current() -> Board {
        let path = r"HARDWARE\DESCRIPTION\System\BIOS";
        Board { manufacturer: win::machine_string(path, "BaseBoardManufacturer").unwrap_or_default(),
                product: win::machine_string(path, "BaseBoardProduct").unwrap_or_default() }
    }
}

/// What one chip's channels are wired to on a board, by channel from 0.
struct ChipMap {
    chip: &'static str,
    temperatures: &'static [&'static str],
    fans: &'static [&'static str],
}

/// Gigabyte's X870E AORUS boards with an IT8696E and an IT87952E, from LHM's X870E AORUS PRO maps
/// (SuperIOHardware.cs). LHM has "System Fan #4 " with a space at the end; not here.
const GIGABYTE_X870E_ITE: &[ChipMap] = &[
    ChipMap { chip: "IT8696E", temperatures: &["System #1", "PCH", "CPU", "PCIe x16", "VRM MOS", "External #1"],
              fans: &["CPU Fan", "System Fan #1", "System Fan #2", "System Fan #3", "CPU Optional Fan"] },
    ChipMap { chip: "IT87952E", temperatures: &["PCIe x4", "External #2", "System #2"],
              fans: &["System Fan #5 / Pump", "System Fan #6 / Pump", "System Fan #4"] },
];

/// The boards with a map: manufacturer, product, map. LHM applies the same maps to the PRO, PRO ICE and
/// XTREME AI TOP, and its pull request 2587 to the MASTER X3D and MASTER X3D ICE.
const BOARDS: &[(&str, &str, &[ChipMap])] = &[
    ("Gigabyte Technology Co., Ltd.", "X870E AORUS PRO", GIGABYTE_X870E_ITE),
    ("Gigabyte Technology Co., Ltd.", "X870E AORUS PRO ICE", GIGABYTE_X870E_ITE),
    ("Gigabyte Technology Co., Ltd.", "X870E AORUS XTREME AI TOP", GIGABYTE_X870E_ITE),
    ("Gigabyte Technology Co., Ltd.", "X870E AORUS MASTER X3D", GIGABYTE_X870E_ITE),
    ("Gigabyte Technology Co., Ltd.", "X870E AORUS MASTER X3D ICE", GIGABYTE_X870E_ITE),
];

/// The names for one board's readings.
pub struct Names {
    map: Option<&'static [ChipMap]>,
}

impl Names {
    /// The names for `board`: its map when it has one (the names must match exactly, but for case and
    /// spaces at either end), generic names otherwise.
    pub fn for_board(board: &Board) -> Names {
        let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b);
        let map = BOARDS.iter().find(|(maker, product, _)| same(&board.manufacturer, maker) && same(&board.product, product));
        Names { map: map.map(|(_, _, map)| *map) }
    }

    /// Whether the board has a map.
    pub fn mapped(&self) -> bool {
        self.map.is_some()
    }

    /// A reading's name: from the board's map where it has one for that channel, else generic ("CPU Tctl",
    /// "ITE IT8696E fan 2").
    pub fn label(&self, sensor: &Sensor) -> String {
        if let Sensor::Chip { chip, channel, .. } = sensor {
            let names = self.map.unwrap_or_default().iter().find(|m| m.chip == *chip).and_then(|m| match channel {
                Channel::Temperature(i) => m.temperatures.get(*i),
                Channel::Fan(i) => m.fans.get(*i),
            });
            if let Some(name) = names {
                return name.to_string();
            }
        }
        generic(sensor)
    }
}

/// A reading's generic name.
pub fn generic(sensor: &Sensor) -> String {
    match sensor {
        Sensor::Cpu(Cpu::Tctl) => "CPU Tctl".to_string(),
        Sensor::Cpu(Cpu::Tdie) => "CPU Tdie".to_string(),
        Sensor::Cpu(Cpu::Ccd(slot)) => format!("CPU CCD{}", slot + 1),
        Sensor::Cpu(Cpu::CcdMax) => "CPU CCDs max".to_string(),
        Sensor::Cpu(Cpu::CcdAverage) => "CPU CCDs average".to_string(),
        Sensor::Cpu(Cpu::Package) => "CPU package power".to_string(),
        Sensor::Chip { chip, channel: Channel::Temperature(i), .. } => format!("ITE {chip} temperature {}", i + 1),
        Sensor::Chip { chip, channel: Channel::Fan(i), .. } => format!("ITE {chip} fan {}", i + 1),
    }
}

pub(crate) mod win {
    use std::ptr::null_mut;

    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// A text value under HKEY_LOCAL_MACHINE, or None.
    pub fn machine_string(path: &str, name: &str) -> Option<String> {
        let (path, name) = (wide(path), wide(name));
        let mut bytes = 0u32;
        let found = unsafe {
            RegGetValueW(HKEY_LOCAL_MACHINE, path.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), &mut bytes)
        };
        if found != 0 || bytes == 0 {
            return None;
        }
        let mut text = vec![0u16; bytes.div_ceil(2) as usize];
        let read = unsafe {
            RegGetValueW(HKEY_LOCAL_MACHINE, path.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, null_mut(),
                         text.as_mut_ptr().cast(), &mut bytes)
        };
        if read != 0 {
            return None;
        }
        let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
        Some(String::from_utf16_lossy(&text[..end]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board(manufacturer: &str, product: &str) -> Board {
        Board { manufacturer: manufacturer.to_string(), product: product.to_string() }
    }

    fn chip(chip: &'static str, channel: Channel) -> Sensor {
        Sensor::Chip { chip, hardware: format!("ite_{}", chip.to_lowercase()), channel }
    }

    /// A mapped board's channels have its names; a channel the map doesn't have, and the CPU, are generic.
    #[test]
    fn board_names() {
        let names = Names::for_board(&board("Gigabyte Technology Co., Ltd.", "X870E AORUS PRO"));
        assert!(names.mapped());
        let labels: Vec<String> = [chip("IT8696E", Channel::Temperature(2)), chip("IT8696E", Channel::Fan(0)),
                                   chip("IT87952E", Channel::Fan(1)), chip("IT87952E", Channel::Fan(2)),
                                   chip("IT8696E", Channel::Fan(5)), chip("IT8689E", Channel::Fan(0)),
                                   Sensor::Cpu(Cpu::Tctl)]
            .iter()
            .map(|s| names.label(s))
            .collect();
        assert_eq!(labels, ["CPU", "CPU Fan", "System Fan #6 / Pump", "System Fan #4", "ITE IT8696E fan 6",
                            "ITE IT8689E fan 1", "CPU Tctl"]);
        // the MASTER X3D boards have the same maps, from LHM's pull request
        for product in ["X870E AORUS PRO ICE", "X870E AORUS XTREME AI TOP", "X870E AORUS MASTER X3D", "X870E AORUS MASTER X3D ICE"] {
            let names = Names::for_board(&board("Gigabyte Technology Co., Ltd.", product));
            assert_eq!(names.label(&chip("IT87952E", Channel::Fan(1))), "System Fan #6 / Pump", "{product}");
        }
    }

    /// Only the exact board names have a map, though case and spaces at the ends don't count. This PC's
    /// board (ELITE X3D ICE) has none until the hardware probe shows its channels are the PRO's.
    #[test]
    fn only_exact_boards_are_mapped() {
        assert!(Names::for_board(&board("GIGABYTE TECHNOLOGY CO., LTD. ", " x870e aorus pro ice")).mapped());
        for (maker, product) in [("Gigabyte Technology Co., Ltd.", "X870E AORUS ELITE X3D ICE"),
                                 ("Gigabyte Technology Co., Ltd.", "X870E AORUS PRO WIFI"),
                                 ("Gigabyte Technology Co., Ltd.", "X870E AORUS"), ("Gigabyte", "X870E AORUS PRO"),
                                 ("ASUSTeK COMPUTER INC.", "X870E AORUS PRO"), ("", "")] {
            let names = Names::for_board(&board(maker, product));
            assert!(!names.mapped(), "{maker} {product}");
            assert_eq!(names.label(&chip("IT87952E", Channel::Fan(1))), "ITE IT87952E fan 2");
            assert_eq!(names.label(&chip("IT8696E", Channel::Temperature(2))), "ITE IT8696E temperature 3");
        }
    }

    /// The generic names.
    #[test]
    fn generic_names() {
        let labels: Vec<String> = [Cpu::Tctl, Cpu::Tdie, Cpu::Ccd(0), Cpu::CcdMax, Cpu::CcdAverage, Cpu::Package]
            .into_iter()
            .map(|c| generic(&Sensor::Cpu(c)))
            .collect();
        assert_eq!(labels, ["CPU Tctl", "CPU Tdie", "CPU CCD1", "CPU CCDs max", "CPU CCDs average", "CPU package power"]);
    }

    /// This PC's board comes from the registry, which anyone can read; a value that isn't there is None.
    #[test]
    fn board_from_the_registry() {
        let board = Board::current();
        assert!(!board.manufacturer.contains('\0') && !board.product.contains('\0'));
        let windows = win::machine_string(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", "ProductName");
        assert!(windows.is_some_and(|name| name.starts_with("Windows")));
        assert_eq!(win::machine_string(r"HARDWARE\DESCRIPTION\System\BIOS", "NoSuchValueHere"), None);
        assert_eq!(win::machine_string(r"HARDWARE\DESCRIPTION\NoSuchKeyHere", "BaseBoardProduct"), None);
    }
}
