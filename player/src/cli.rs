//! The command line, shared by wvplay.exe (console) and wvplayw.exe (no window, for starting with Windows).

use std::error::Error;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use image::ImageFormat;

use crate::panel::Panel;
use crate::player::{log_to, play};
use crate::{art, playlist};

const USAGE: &str = "\
wvplay: drives the Thermalright Wonder Vision 360 screen directly, without TRCC.

  wvplay play <what>          play on the screen until stopped. <what> is a GIF / PNG / animated PNG /
                              WebP / MP4 (or other video), a folder of them, or a playlist .json.
                              Picks up edits to any of these while playing, and reconnects to the
                              panel on its own.
       [--seconds S]          how long each piece in a folder plays, default 30
       [--no-rotate]          don't turn the picture 180 degrees (the panel is mounted upside down)
       [--quality Q]          JPEG quality sent to the panel, default 95
       [--ffmpeg PATH]        ffmpeg for video pieces (default: $WVPLAY_FFMPEG, else ffmpeg on PATH)
       [--log FILE]           write what happens to FILE instead of the console
  wvplay prepare <what>       get everything ready as `play` would, without sending it anywhere
  wvplay render <playlist.json> <item> <out.png> [--frame N]
                              write frame N (default 0) of item number <item> (counting from 0), with
                              its text drawn on, upright, as a PNG: for checking text without the screen
  wvplay info                 handshake and print what the panel reports
  wvplay send <file.jpg> <width> <height> [--repeat N] [--seconds S] [--fps F]
                              send a ready-made JPEG (for testing the panel)
";

fn option(args: &[String], name: &str) -> Result<Option<f64>, Box<dyn Error>> {
    match args.iter().position(|a| a == name) {
        Some(i) => Ok(Some(args.get(i + 1).ok_or(format!("{name} needs a number"))?.parse()?)),
        None => Ok(None),
    }
}

fn art_options(args: &[String]) -> Result<art::Options, Box<dyn Error>> {
    let ffmpeg = match args.iter().position(|a| a == "--ffmpeg") {
        Some(i) => PathBuf::from(args.get(i + 1).ok_or("--ffmpeg needs a path")?),
        None => std::env::var_os("WVPLAY_FFMPEG").map_or_else(|| PathBuf::from("ffmpeg"), PathBuf::from),
    };
    Ok(art::Options {
        rotate180: !args.iter().any(|a| a == "--no-rotate"),
        quality: option(args, "--quality")?.unwrap_or(95.0).clamp(30.0, 100.0) as u8,
        ffmpeg,
    })
}

pub fn run(args: &[String]) -> Result<(), Box<dyn Error>> {
    match args.first().map(String::as_str) {
        Some("play") if args.len() >= 2 => {
            if let Some(i) = args.iter().position(|a| a == "--log") {
                log_to(Path::new(args.get(i + 1).ok_or("--log needs a file")?))?;
            }
            let seconds = option(args, "--seconds")?.unwrap_or(30.0).max(1.0);
            play(Path::new(&args[1]), art_options(args)?, seconds)
        }
        Some("prepare") if args.len() >= 2 => {
            // everything `play` does before it sends anything: handy for timing and for checking files
            let opts = art_options(args)?;
            for item in playlist::read(Path::new(&args[1]), 30.0)? {
                let start = Instant::now();
                let frames = art::load(&item, &opts, true)?;
                let kb = frames.iter().map(|f| f.jpeg.len()).sum::<usize>() / 1024;
                println!("{}: {} frame(s), {kb} KB, {} ms", item.path.display(), frames.len(), start.elapsed().as_millis());
            }
            Ok(())
        }
        Some("render") if args.len() >= 4 => {
            let items = playlist::read(Path::new(&args[1]), 30.0)?;
            let index: usize = args[2].parse().map_err(|_| format!("<item> should be a number, not {}", args[2]))?;
            let item = items.get(index).ok_or(format!("there's no item {index}: the playlist has {}", items.len()))?;
            let frame = option(args, "--frame")?.unwrap_or(0.0).max(0.0) as usize;
            let start = Instant::now();
            art::render(item, &art_options(args)?, frame)?.save_with_format(&args[3], ImageFormat::Png)?;
            println!("{}: frame {frame} with {} widget(s) -> {} ({} ms)", item.path.display(), item.text().count(),
                     args[3], start.elapsed().as_millis());
            Ok(())
        }
        Some("info") => {
            let panel = Panel::open()?;
            let i = &panel.info;
            println!("identifier {:?}  PM 0x{:02X}  SUB 0x{:02X}  flags 0x{:02X}", i.identifier, i.pm, i.sub, i.flags);
            println!("reply ({} bytes): {:02x?}", i.reply.len(), &i.reply[..i.reply.len().min(64)]);
            Ok(())
        }
        Some("send") if args.len() >= 4 => {
            let jpeg = std::fs::read(&args[1])?;
            let (w, h): (u32, u32) = (args[2].parse()?, args[3].parse()?);
            let repeat = option(args, "--repeat")?.unwrap_or(1.0) as u64;
            let seconds = option(args, "--seconds")?.map(Duration::from_secs_f64);
            let gap = option(args, "--fps")?.map(|fps| Duration::from_secs_f64(1.0 / fps));
            let mut panel = Panel::open()?;
            let start = Instant::now();
            let mut sent = 0u64;
            while sent < repeat || seconds.is_some_and(|s| start.elapsed() < s) {
                if let Some(due) = gap.map(|g| start + g * sent as u32) {
                    sleep(due.saturating_duration_since(Instant::now()));
                }
                panel.send_jpeg(&jpeg, w, h)?;
                sent += 1;
            }
            let took = start.elapsed().max(Duration::from_millis(1));
            println!("sent {sent} x {} KB as {w}x{h} in {:.0} ms ({:.1} frames/s)", jpeg.len() / 1024,
                     took.as_secs_f64() * 1000.0, sent as f64 / took.as_secs_f64());
            Ok(())
        }
        _ => {
            print!("{USAGE}");
            Ok(())
        }
    }
}

