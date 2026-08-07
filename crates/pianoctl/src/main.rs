mod map;
mod render;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod profile;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod sysex;

#[cfg(target_os = "linux")]
mod online;
#[cfg(target_os = "linux")]
mod seq;

#[cfg(not(target_os = "linux"))]
mod online {
    use crate::map::{AddressMap, Param};
    use crate::profile::Step;
    use crate::sysex::Codec;

    const ELSEWHERE: &str =
        "reaching the piano needs the ALSA sequencer; pianoctl talks to it on Linux only";

    pub fn identity(_: &AddressMap, _: &Codec, _: &str) -> Result<bool, String> {
        Err(ELSEWHERE.into())
    }

    pub fn read(_: &AddressMap, _: &Codec, _: &str, _: &[&Param]) -> Result<bool, String> {
        Err(ELSEWHERE.into())
    }

    pub fn watch(_: &AddressMap, _: &Codec, _: &str) -> Result<bool, String> {
        Err(ELSEWHERE.into())
    }

    pub fn diff(_: &AddressMap, _: &Codec, _: &str, _: &[Step<'_>]) -> Result<bool, String> {
        Err(ELSEWHERE.into())
    }

    pub fn apply(_: &AddressMap, _: &Codec, _: &str, _: &[Step<'_>]) -> Result<bool, String> {
        Err(ELSEWHERE.into())
    }
}

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::map::{AddressMap, Param};
use crate::profile::Profile;
use crate::sysex::{Codec, parse_hex, spaced_hex};

#[derive(Parser)]
#[command(about = "Read side of the Roland FP-30X control plane (SysEx over ALSA)")]
struct Args {
    /// Address map describing the device and its parameters. The default is
    /// relative to the working directory, so an installed binary is pointed at
    /// its data with this flag or with PIANOCTL_MAP.
    #[arg(
        long,
        env = "PIANOCTL_MAP",
        default_value = "data/fp30x-address-map.json",
        global = true
    )]
    map: PathBuf,
    /// Substring matched (case-insensitive) against ALSA client/port names.
    #[arg(long, default_value = "Roland", global = true)]
    port: String,
    /// Device ID to address, as hex, overriding the map's default. `00`
    /// broadcasts — the first thing to try when nothing answers, since the
    /// map's default is itself an unverified guess (SYSEX.md §4).
    #[arg(long, global = true)]
    device_id: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Ask the piano who it is (Universal Identity Request).
    Identity,
    /// Read parameters from the piano (RQ1).
    Read {
        #[arg(required = true)]
        params: Vec<String>,
    },
    /// Print every SysEx frame the piano sends, decoded.
    Watch,
    /// Show how the piano differs from a profile, and the frames an apply
    /// would send. Sends nothing.
    Diff { profile: PathBuf },
    /// Bring the piano to what a profile asks for, confirming each write by
    /// reading it back. Sends only what differs.
    Apply { profile: PathBuf },
    /// Check the address map for internal consistency, without hardware.
    CheckMap,
    /// Decode SysEx frames given as hex, e.g. a `raw` field from the capture log.
    Decode {
        #[arg(required = true)]
        frames: Vec<String>,
    },
}

/// A phase 0 session is a shell session (SYSEX.md §8), so the three outcomes a
/// script has to tell apart get three codes: everything answered, something did
/// not answer, and nothing could be measured at all. Silence is a result about
/// the firmware; a broken map is not.
const NO_ANSWER: u8 = 1;
const CANNOT_MEASURE: u8 = 2;

fn main() -> ExitCode {
    // CLI output may be piped to a reader that exits early. Rust ignores
    // SIGPIPE, which would turn the next `println!` into a panic; restore the
    // usual command-line behaviour.
    #[cfg(unix)]
    // SAFETY: setting a signal disposition before any thread is spawned.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args = Args::parse();
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(NO_ANSWER),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(CANNOT_MEASURE)
        }
    }
}

fn run(args: &Args) -> Result<bool, String> {
    let map = AddressMap::load(&args.map)?;
    let codec = match &args.device_id {
        Some(text) => match parse_hex(text)?[..] {
            [id] if id <= 0x7F => map.codec().with_device_id(id),
            _ => {
                return Err(format!(
                    "--device-id takes one 7-bit hex byte, got {text:?}"
                ));
            }
        },
        None => map.codec(),
    };
    match &args.command {
        Command::CheckMap => {
            check_map(&map, &args.map);
            Ok(true)
        }
        Command::Decode { frames } => decode(&map, &codec, frames),
        Command::Identity => online::identity(&map, &codec, &args.port),
        Command::Read { params } => {
            let params = params
                .iter()
                .map(|name| map.readable(name))
                .collect::<Result<Vec<_>, _>>()?;
            online::read(&map, &codec, &args.port, &params)
        }
        Command::Watch => online::watch(&map, &codec, &args.port),
        Command::Diff { profile } => {
            let steps = Profile::load(profile)?.plan(&map)?;
            online::diff(&map, &codec, &args.port, &steps)
        }
        Command::Apply { profile } => {
            let steps = Profile::load(profile)?.plan(&map)?;
            online::apply(&map, &codec, &args.port, &steps)
        }
    }
}

fn check_map(map: &AddressMap, path: &Path) {
    println!("{} — {}", map.device.name, path.display());
    println!(
        "model {}, device id {:02X}, {} checksum",
        spaced_hex(&map.device.model_id),
        map.device.device_id_default,
        map.device.checksum.as_str(),
    );
    if let Some(notice) = &map.notice {
        println!("{notice}");
    }
    for corroboration in &map.device.corroborations {
        println!("corroborated by: {corroboration}");
    }
    println!();
    for param in &map.params {
        println!("{}", inventory(param));
        println!("    from {} — {}", param.source.as_str(), param.source_ref);
        for corroboration in &param.corroborations {
            println!("    corroborated by: {corroboration}");
        }
        for confirmation in &param.verified {
            println!(
                "    verified {} on firmware {} by {}",
                confirmation.date,
                confirmation.firmware,
                confirmation.method.as_str(),
            );
        }
        if let Some(notes) = &param.notes {
            println!("    {notes}");
        }
    }
    println!();
    let verified = map.params.iter().filter(|p| !p.verified.is_empty()).count();
    let mut by_source: BTreeMap<&str, usize> = BTreeMap::new();
    for param in &map.params {
        *by_source.entry(param.source.as_str()).or_default() += 1;
    }
    let sources: Vec<String> = by_source
        .iter()
        .map(|(source, count)| format!("{count} {source}"))
        .collect();
    println!(
        "{} parameters ({}), {verified} verified on hardware, {} example frames, no problems",
        map.params.len(),
        sources.join(" + "),
        map.examples.len(),
    );
}

fn inventory(param: &Param) -> String {
    let direction = match (param.read_address, param.write_address) {
        (Some(_), Some(_)) => "rw",
        (Some(_), None) => "r-",
        (None, Some(_)) => "-w",
        (None, None) => "??",
    };
    let address = param
        .read_address
        .or(param.write_address)
        .map_or_else(String::new, |addr| addr.to_string());
    let asymmetric = match (param.read_address, param.write_address) {
        (Some(read), Some(write)) if read != write => format!(" (writes {write})"),
        _ => String::new(),
    };
    let values = if param.labels.is_empty() {
        param.range.map_or_else(String::new, |range| {
            format!("  {}..={}", range.min, range.max)
        })
    } else {
        format!("  {}", param.label_list())
    };
    let trust = if param.verified.is_empty() {
        "unverified"
    } else {
        "verified"
    };
    format!(
        "{:<18} {direction} {address}{asymmetric}  {} × {}{values}  [{trust}]",
        param.name, param.size, param.encoding
    )
}

/// Decode frames handed in as hex. A frame this device would never send is
/// still decoded successfully — saying so is the answer; the run is
/// unsuccessful only for a frame this build cannot turn into a message at all:
/// bad checksum, torn, non-7-bit, or a Roland command it does not implement.
///
/// One unusable argument does not stop the rest: these are pasted in batches
/// from the capture log, and dropping the frames after a typo would hide
/// whatever they had to say.
fn decode(map: &AddressMap, codec: &Codec, frames: &[String]) -> Result<bool, String> {
    let mut all_well_formed = true;
    for text in frames {
        match parse_hex(text) {
            Err(e) => {
                println!("!!  {e:<54} {text}");
                all_well_formed = false;
            }
            Ok(raw) => {
                let decoded = codec.decode(&raw);
                all_well_formed &= decoded.is_ok();
                println!("{}", render::frame(map, codec, &raw, decoded));
            }
        }
    }
    Ok(all_well_formed)
}
