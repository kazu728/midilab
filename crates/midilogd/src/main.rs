//! Captured events go to crash-safe daily JSONL; the log is the source of truth,
//! while SMF, metrics, and analysis are derived views.

#[cfg(any(target_os = "linux", test))]
mod sink;

#[cfg(target_os = "linux")]
mod event;
#[cfg(target_os = "linux")]
mod seq_source;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[derive(Parser)]
#[command(about = "Always-on ALSA sequencer capture daemon (append-only JSONL)")]
struct Args {
    /// Root directory for capture/YYYY/MM/DD.jsonl.
    #[arg(long, default_value = "capture")]
    capture_dir: PathBuf,
    /// Substring matched (case-insensitive) against ALSA client/port names.
    #[arg(long, default_value = "Roland")]
    source: String,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "linux")]
fn run(args: &Args) -> Result<(), String> {
    seq_source::run(&args.capture_dir, &args.source)
}

#[cfg(not(target_os = "linux"))]
fn run(_args: &Args) -> Result<(), String> {
    Err("midilogd requires the ALSA sequencer and runs on Linux only".into())
}
