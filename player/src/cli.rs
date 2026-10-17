//! The command line, shared by wvplay.exe (console) and wvplayw.exe (no window, for starting with Windows).

use std::error::Error;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use image::ImageFormat;

use crate::panel::Panel;
use crate::player::{log_to, play};
use crate::playlist::Text;
use crate::{art, data, playlist, scene};

const USAGE: &str = "\
wvplay: drives the Thermalright Wonder Vision 360 screen directly, without TRCC.

  wvplay play <what>          play on the screen until stopped. <what> is a GIF / PNG / animated PNG /
                              WebP / MP4 (or other video), a folder of them, or a playlist .json.
                              Picks up edits to any of these while playing, and reconnects to the
                              panel on its own.
       [--seconds S]          how long each piece in a folder plays, default 30
       [--no-rotate]          don't turn the picture 180 degrees (the panel is mounted upside down)
       [--quality Q]          JPEG quality sent to the panel, default 95
       [--ffmpeg PATH]        ffmpeg for video (default: $WVPLAY_FFMPEG, else ffmpeg on PATH)
       [--log FILE]           write what happens to FILE instead of the console
  wvplay prepare <what>       get everything ready as `play` would, without sending it anywhere
  wvplay render <playlist.json> <scene> <out.png> [--time MS]
                              write scene number <scene> (counting from 0) at MS milliseconds into its
                              cycle (default 0), upright, as a PNG: for checking without the screen.
                              Live text is filled in by the running player, else only the time is
  wvplay data <playlist.json> [--serve]
                              poll each source in the data.json beside the playlist once, and print the
                              values. --serve: carry on polling, and serve the local API, as `play` does
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
            let playlist = playlist::read(Path::new(&args[1]), 30.0, &opts.ffmpeg)?;
            for scene in &playlist.scenes {
                let start = Instant::now();
                let prepared = art::load(scene, &playlist.global, &opts, true)?;
                let kb = prepared.frames.iter().map(|f| f.jpeg.len()).sum::<usize>() / 1024;
                println!("{}: {} frame(s), {kb} KB, {} ms", scene.name, prepared.frames.len(), start.elapsed().as_millis());
            }
            Ok(())
        }
        Some("render") if args.len() >= 4 => {
            let opts = art_options(args)?;
            let playlist = playlist::read(Path::new(&args[1]), 30.0, &opts.ffmpeg)?;
            let index: usize = args[2].parse().map_err(|_| format!("<scene> should be a number, not {}", args[2]))?;
            let scene = playlist.scenes.get(index).ok_or(format!("there's no scene {index}: the playlist has {}", playlist.scenes.len()))?;
            let time = Duration::from_millis(option(args, "--time")?.unwrap_or(0.0).max(0.0) as u64);
            let start = Instant::now();
            // live text with the values the player has now; without a player, only the time
            let live: Vec<Text> = scene.layers.iter().chain(&playlist.global).flat_map(|l| l.live().collect::<Vec<_>>()).collect();
            let texts: Vec<String> = live.iter().map(|w| w.text.clone()).collect();
            let filled = match texts.is_empty() {
                true => Vec::new(),
                false => data::ask_player(Path::new(&args[1]), &texts).unwrap_or_else(|| {
                    println!("no player answering: only the time is filled in");
                    texts.iter().map(|t| data::resolve(t)).collect()
                }),
            };
            let live: Vec<Text> = live.into_iter().zip(filled).map(|(w, text)| Text { text, ..w }).collect();
            for w in &live {
                println!("live: {:?}", w.text);
            }
            let frame = scene::render_at(scene, &playlist.global, &opts, time, &live);
            frame.save_with_format(&args[3], ImageFormat::Png)?;
            let times = scene::times(scene, &playlist.global, &opts);
            println!("{}: at {} ms ({} frame(s) in its cycle), {} live -> {} ({} ms)", scene.name, time.as_millis(), times.len(),
                     live.len(), args[3], start.elapsed().as_millis());
            Ok(())
        }
        Some("data") if args.len() >= 2 => {
            let serve = args.iter().any(|a| a == "--serve");
            let hub = data::start(Path::new(&args[1]), serve);
            hub.wait(Duration::from_secs(30));
            print!("{}", hub.report());
            while serve {
                sleep(Duration::from_secs(3600));
            }
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
