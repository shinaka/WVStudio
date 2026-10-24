//! wvsensors.exe: the small program that runs as administrator and reads PawnIO's sensors for the player
//! (src/pawnio/helper.rs). It reads no data.json, playlist or media, spawns nothing, loads only its two
//! pinned module blobs, and talks only to its parent over a pipe.
#![windows_subsystem = "windows"]

use std::process::ExitCode;

use windows_sys::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories};

fn main() -> ExitCode {
    // first of all, before anything can load a DLL: look for the DLLs this program imports in System32
    // only, never in the folder it runs in, which anyone can write to (D15, S-HIGH-3)
    unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) };
    let args: Vec<String> = std::env::args().skip(1).collect();
    wvplay::pawnio::helper::run(&args)
}
