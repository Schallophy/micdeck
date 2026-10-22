//! Release builds are GUI-subsystem so a double-click starts the product without a console window;
//! debug builds keep the console for diagnostics.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::process::ExitCode;

fn main() -> ExitCode {
    micdeck_lib::bench::entry()
}
