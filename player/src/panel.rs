//! The Wonder Vision 360's USB display protocol ("ChiZhu" bulk protocol, as TRCC speaks it).
//!
//! The panel is a WinUSB device, VID 87AD / PID 70DB. Everything goes over one bulk OUT and one
//! bulk IN endpoint:
//!
//! * Handshake: 64 bytes, `12 34 56 78` magic, `1` at offset 56. The reply (up to 1024 bytes)
//!   carries an identifier such as `SSCRM-V3` at 4.., PM at 24 and SUB at 28. While the panel is
//!   still booting it replies `A1 A2 A3 A4` at 4..8 instead.
//! * Frame: a 64-byte header (magic, format `2` = JPEG, width, height, `2` at 56, payload length
//!   at 60) followed by the JPEG, as ONE transfer, plus a zero-length packet when the total is a
//!   multiple of 512.
//!
//! Sources: InfoPanel fork by emaspa (ThermalrightPanelDeviceTask.cs) and the TRCC decompile notes
//! in Lexonight1/thermalright-trcc-linux (dev/decompiler/AUDIT_WIRE.md).

use std::io::{Error, ErrorKind, Result};
use std::thread::sleep;
use std::time::{Duration, Instant};

use nusb::transfer::{Buffer, Bulk, In, Out};
use nusb::{Endpoint, MaybeFuture};

pub const VID: u16 = 0x87AD;
pub const PID: u16 = 0x70DB;
const EP_OUT: u8 = 0x01;
const EP_IN: u8 = 0x81;
const MAGIC: [u8; 4] = [0x12, 0x34, 0x56, 0x78];
const HEADER: usize = 64;
const FORMAT_JPEG: u32 = 2;
const TIMEOUT: Duration = Duration::from_secs(5);
/// TRCC waits this long between frames; the panel's firmware is said to need it.
const FRAME_GAP: Duration = Duration::from_millis(15);

pub struct Panel {
    out: Endpoint<Bulk, Out>,
    inp: Endpoint<Bulk, In>,
    last_frame: Option<Instant>,
    pub info: Info,
}

#[derive(Debug, Clone)]
pub struct Info {
    pub identifier: String,
    pub pm: u8,
    pub sub: u8,
    pub flags: u8,
    pub reply: Vec<u8>,
}

impl Panel {
    pub fn open() -> Result<Panel> {
        let info = nusb::list_devices()
            .wait()?
            .find(|d| d.vendor_id() == VID && d.product_id() == PID)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "no Wonder Vision panel (USB 87AD:70DB) found"))?;
        let device = info.open().wait()?;
        let interface = device.claim_interface(0).wait().map_err(|e| {
            let e = Error::from(e);
            Error::new(e.kind(), format!("couldn't claim the panel ({e}); is TRCC or InfoPanel running?"))
        })?;
        let mut panel = Panel {
            out: interface.endpoint::<Bulk, Out>(EP_OUT)?,
            inp: interface.endpoint::<Bulk, In>(EP_IN)?,
            last_frame: None,
            info: Info { identifier: String::new(), pm: 0, sub: 0, flags: 0, reply: Vec::new() },
        };
        panel.info = panel.handshake()?;
        Ok(panel)
    }

    fn handshake(&mut self) -> Result<Info> {
        let mut hello = [0u8; HEADER];
        hello[..4].copy_from_slice(&MAGIC);
        hello[56..60].copy_from_slice(&1u32.to_le_bytes());
        sleep(Duration::from_millis(50));
        for _ in 0..5 {
            self.write(&hello)?;
            let reply = self.inp.transfer_blocking(Buffer::new(1024), TIMEOUT).into_result()?.to_vec();
            if reply.len() >= 8 && reply[4..8] == [0xA1, 0xA2, 0xA3, 0xA4] {
                sleep(Duration::from_secs(3)); // still booting
                continue;
            }
            if reply.len() < 29 {
                return Err(Error::new(ErrorKind::InvalidData, format!("short handshake reply: {reply:02x?}")));
            }
            let id_end = reply.len().min(20);
            let identifier = String::from_utf8_lossy(&reply[4..id_end]).trim_end_matches('\0').to_string();
            sleep(Duration::from_millis(200));
            return Ok(Info { identifier, pm: reply[24], sub: reply[28], flags: reply[20], reply });
        }
        Err(Error::new(ErrorKind::TimedOut, "panel still booting after 5 tries"))
    }

    /// Shows one JPEG frame. `width`/`height` are what the header declares the JPEG to be.
    pub fn send_jpeg(&mut self, jpeg: &[u8], width: u32, height: u32) -> Result<()> {
        let mut packet = Vec::with_capacity(HEADER + jpeg.len());
        packet.extend_from_slice(&MAGIC);
        packet.extend_from_slice(&FORMAT_JPEG.to_le_bytes());
        packet.extend_from_slice(&width.to_le_bytes());
        packet.extend_from_slice(&height.to_le_bytes());
        packet.resize(56, 0);
        packet.extend_from_slice(&2u32.to_le_bytes()); // frame type: picture
        packet.extend_from_slice(&(jpeg.len() as u32).to_le_bytes());
        packet.extend_from_slice(jpeg);
        if let Some(wait) = self.last_frame.and_then(|t| FRAME_GAP.checked_sub(t.elapsed())) {
            sleep(wait);
        }
        self.write(&packet)?;
        if packet.len() % 512 == 0 {
            self.write(&[])?; // zero-length packet ends the transfer
        }
        self.last_frame = Some(Instant::now());
        Ok(())
    }

    fn write(&mut self, data: &[u8]) -> Result<()> {
        self.out.transfer_blocking(data.to_vec().into(), TIMEOUT).into_result()?;
        Ok(())
    }
}
