//! wvplay without a console window, for starting with Windows: `wvplayw play <what> --log <file>`.
#![windows_subsystem = "windows"]

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match wvplay::cli::run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            wvplay::player::log(format_args!("error: {e}")); // there's no console to see it on
            ExitCode::FAILURE
        }
    }
}
