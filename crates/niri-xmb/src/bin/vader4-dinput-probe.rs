//! Scoped probe for the Vader 4 DInput report spike.
//!
//! This binary deliberately does not start the gamepad service, grab a device,
//! create a virtual gamepad, or touch navi state. It reads one 32-byte report
//! from a fixture or hidraw node and prints the decoded state.

#[path = "../dinput.rs"]
mod dinput;

use dinput::{VADER4_DINPUT_PID, VADER4_DINPUT_REPORT_LEN, VADER4_DINPUT_VID, Vader4State};
use std::env;
use std::fs::File;
use std::io::{self, Read};
use std::process::ExitCode;

fn usage(program: &str) -> String {
    format!(
        "usage: {program} <fixture.bin|/dev/hidrawN>\n\
         reads one {VADER4_DINPUT_REPORT_LEN}-byte Vader 4 DInput report\n\
         device IDs: {VADER4_DINPUT_VID:04x}:{VADER4_DINPUT_PID:04x}, report 0x04"
    )
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args();
    let program = args.next().unwrap_or_else(|| "vader4-dinput-probe".into());
    let Some(path) = args.next() else {
        return Err(usage(&program).into());
    };
    if args.next().is_some() || path == "-h" || path == "--help" {
        return Err(usage(&program).into());
    }

    let mut file = if path == "-" {
        Box::new(io::stdin()) as Box<dyn Read>
    } else {
        Box::new(File::open(&path).map_err(|error| format!("{path}: {error}"))?) as Box<dyn Read>
    };
    let mut report = [0u8; VADER4_DINPUT_REPORT_LEN];
    file.read_exact(&mut report)
        .map_err(|error| format!("{path}: expected {VADER4_DINPUT_REPORT_LEN} bytes: {error}"))?;

    let state = Vader4State::parse(&report)?;
    println!("source: {path}");
    println!("device: {VADER4_DINPUT_VID:04x}:{VADER4_DINPUT_PID:04x}");
    println!("{}", serde_json::to_string_pretty(&state)?);
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
