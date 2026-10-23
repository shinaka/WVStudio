//! Key names: `pawnio.cpu.<type>.<sensor>` for the CPU, and Libre Hardware Monitor's generic names for the
//! chips' channels (`pawnio.ite_it87952e.fan.fan_2`).
//!
//! A chip's keys are what LHM gives on a board it has no map for, with `pawnio.` for `lhm.`: so on this PC
//! they're the user's `lhm.` keys. On a board LHM has a map for, its keys use the map's names instead
//! ("System Fan #6 / Pump"); ours don't, as the names live in labels (labels.rs), never in keys.

use std::collections::HashMap;

use crate::sources::slug;

/// A CPU reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cpu {
    /// The temperature the CPU controls its fans by.
    Tctl,
    /// The die's temperature, where it isn't Tctl (some family 17h parts read Tctl with an offset).
    Tdie,
    /// One CCD's temperature, by its slot from 0: `ccd1` is slot 0.
    Ccd(usize),
    /// The hottest and the average of the CCDs read this time, when there are two or more.
    CcdMax,
    CcdAverage,
    /// The package's power, in watts.
    Package,
}

/// A chip's channel, numbered from 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Temperature(usize),
    Fan(usize),
}

/// What one reading is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sensor {
    Cpu(Cpu),
    /// A Super I/O chip's channel: the chip's name ("IT8696E") and its hardware key (`hardware`).
    Chip { chip: &'static str, hardware: String, channel: Channel },
}

impl Sensor {
    /// Its key in the store.
    pub fn key(&self) -> String {
        match self {
            Sensor::Cpu(cpu) => {
                let sensor = match cpu {
                    Cpu::Tctl => "tctl".to_string(),
                    Cpu::Tdie => "tdie".to_string(),
                    Cpu::Ccd(slot) => format!("ccd{}", slot + 1),
                    Cpu::CcdMax => "ccd_max".to_string(),
                    Cpu::CcdAverage => "ccd_average".to_string(),
                    Cpu::Package => "package".to_string(),
                };
                let kind = if *cpu == Cpu::Package { "power" } else { "temperature" };
                format!("pawnio.cpu.{kind}.{sensor}")
            }
            // the slugs of LHM's "Temperature #n" and "Fan #n", as lhm_walk makes them
            Sensor::Chip { hardware, channel: Channel::Temperature(i), .. } => {
                format!("pawnio.{hardware}.temperature.temperature_{}", i + 1)
            }
            Sensor::Chip { hardware, channel: Channel::Fan(i), .. } => format!("pawnio.{hardware}.fan.fan_{}", i + 1),
        }
    }

    /// Its unit: °C, RPM or W.
    pub fn unit(&self) -> &'static str {
        match self {
            Sensor::Cpu(Cpu::Package) => "W",
            Sensor::Chip { channel: Channel::Fan(_), .. } => "RPM",
            _ => "°C",
        }
    }
}

/// The hardware keys of chips with these names (`IT8696E`), in order: LHM's `ite_it8696e`, and for a name
/// seen before `_2`, `_3`..., as LHM numbers hardware with the same name.
pub fn hardware(names: &[&str]) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    names
        .iter()
        .map(|name| {
            let key = slug(&format!("ITE {name}"));
            let count = seen.entry(key.clone()).or_default();
            *count += 1;
            if *count > 1 { format!("{key}_{count}") } else { key }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::data::Value;
    use crate::sources::{lhm_sensors, lhm_values};

    fn chip(hardware: &str, channel: Channel) -> String {
        Sensor::Chip { chip: "IT87952E", hardware: hardware.to_string(), channel }.key()
    }

    /// The CPU's keys are our own short names.
    #[test]
    fn cpu_keys() {
        let keys: Vec<String> = [Cpu::Tctl, Cpu::Tdie, Cpu::Ccd(0), Cpu::Ccd(1), Cpu::CcdMax, Cpu::CcdAverage, Cpu::Package]
            .into_iter()
            .map(|c| Sensor::Cpu(c).key())
            .collect();
        assert_eq!(keys, ["pawnio.cpu.temperature.tctl", "pawnio.cpu.temperature.tdie", "pawnio.cpu.temperature.ccd1",
                          "pawnio.cpu.temperature.ccd2", "pawnio.cpu.temperature.ccd_max",
                          "pawnio.cpu.temperature.ccd_average", "pawnio.cpu.power.package"]);
        assert_eq!([Sensor::Cpu(Cpu::Tctl).unit(), Sensor::Cpu(Cpu::Package).unit()], ["°C", "W"]);
    }

    /// A chip's keys are the ones Libre Hardware Monitor gives the same channel on a board it has no map
    /// for, with `pawnio.` for `lhm.`: checked through the player's own LHM reader.
    #[test]
    fn chip_keys_are_lhms() {
        let tree = json!({ "Text": "Sensor", "Children": [{ "Text": "DESKTOP", "Children": [
            { "Text": "X870E AORUS ELITE X3D ICE", "HardwareId": "/motherboard", "Children": [
                { "Text": "ITE IT8696E", "HardwareId": "/lpc/it8696e/0", "Children": [
                    { "Text": "Temperatures", "Children": [
                        { "Text": "Temperature #3", "SensorId": "/lpc/it8696e/0/temperature/2", "Type": "Temperature", "Value": "70.0 °C" }]},
                    { "Text": "Fans", "Children": [
                        { "Text": "Fan #1", "SensorId": "/lpc/it8696e/0/fan/0", "Type": "Fan", "Value": "2368 RPM" }]}]},
                { "Text": "ITE IT87952E", "HardwareId": "/lpc/it87952e/0", "Children": [
                    { "Text": "Fans", "Children": [
                        { "Text": "Fan #2", "SensorId": "/lpc/it87952e/0/fan/1", "Type": "Fan", "Value": "6308 RPM" }]}]},
                { "Text": "ITE IT8696E", "HardwareId": "/lpc/it8696e/1", "Children": [
                    { "Text": "Fans", "Children": [
                        { "Text": "Fan #6", "SensorId": "/lpc/it8696e/1/fan/5", "Type": "Fan", "Value": "900 RPM" }]}]}]}]}]});
        let lhm: Vec<String> = lhm_values(&lhm_sensors(&tree))
            .into_iter()
            .filter(|(k, v)| k.starts_with("lhm.ite_") && matches!(v, Value::Number(_)))
            .map(|(k, _)| k.replacen("lhm.", "pawnio.", 1))
            .collect();
        let hw = hardware(&["IT8696E", "IT87952E", "IT8696E"]);
        let ours = [
            Sensor::Chip { chip: "IT8696E", hardware: hw[0].clone(), channel: Channel::Temperature(2) }.key(),
            Sensor::Chip { chip: "IT8696E", hardware: hw[0].clone(), channel: Channel::Fan(0) }.key(),
            chip(&hw[1], Channel::Fan(1)),
            Sensor::Chip { chip: "IT8696E", hardware: hw[2].clone(), channel: Channel::Fan(5) }.key(),
        ];
        assert_eq!(lhm, ours);
        assert_eq!(ours, ["pawnio.ite_it8696e.temperature.temperature_3", "pawnio.ite_it8696e.fan.fan_1",
                          "pawnio.ite_it87952e.fan.fan_2", "pawnio.ite_it8696e_2.fan.fan_6"]);
    }

    /// A second chip with the same name is `_2`, a third `_3`; other names aren't counted.
    #[test]
    fn same_names_are_numbered() {
        assert_eq!(hardware(&["IT8696E", "IT87952E"]), ["ite_it8696e", "ite_it87952e"]);
        assert_eq!(hardware(&["IT8696E", "IT8696E", "IT8696E"]), ["ite_it8696e", "ite_it8696e_2", "ite_it8696e_3"]);
        assert_eq!(chip("ite_it87952e", Channel::Temperature(0)), "pawnio.ite_it87952e.temperature.temperature_1");
        assert_eq!(Sensor::Chip { chip: "IT87952E", hardware: "x".into(), channel: Channel::Fan(1) }.unit(), "RPM");
        assert_eq!(Sensor::Chip { chip: "IT87952E", hardware: "x".into(), channel: Channel::Temperature(1) }.unit(), "°C");
    }
}
