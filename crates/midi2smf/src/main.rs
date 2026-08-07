//! Use a fixed tempo so the result remains a plain, reversible view.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use midi_event::{Event, Message};
use midly::num::{u4, u7, u15, u24, u28};
use midly::{
    Format, Header, MetaMessage, MidiMessage, PitchBend, Smf, Timing, TrackEvent, TrackEventKind,
};

#[derive(Parser)]
#[command(about = "Project a captured JSONL session to SMF Format 0")]
struct Args {
    /// Input JSONL file(s) in chronological order (one captured event per
    /// line); pass several to reassemble a session that crosses midnight.
    #[arg(short, long, required = true)]
    input: Vec<PathBuf>,
    /// Output .mid path.
    #[arg(short, long)]
    output: PathBuf,
    /// Export only the Nth session (0-indexed). Omit to export every event.
    #[arg(long)]
    session: Option<usize>,
    /// Silence gap that starts a new session, in seconds (with --session).
    #[arg(long, default_value_t = 300)]
    gap_secs: u64,
    /// Pulses per quarter note.
    // SMF metrical timing is 15-bit, hence the 32767 cap.
    #[arg(long, default_value_t = 480, value_parser = clap::value_parser!(u16).range(1..=32767))]
    ppq: u16,
    /// Fixed tempo applied to the whole projection.
    // 60e6/bpm must fit the 24-bit tempo meta (bpm >= 4) and stay >= 1 µs.
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u32).range(4..=60_000_000))]
    bpm: u32,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(skipped) => {
            if skipped > 0 {
                eprintln!("note: {skipped} event(s) not representable in SMF were skipped");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<usize, String> {
    let mut events = Vec::new();
    for path in &args.input {
        events.extend(read_jsonl(path)?);
    }
    let selected: &[Event] = match args.session {
        Some(n) => {
            let groups =
                midi_event::sessions(&events, std::time::Duration::from_secs(args.gap_secs));
            groups
                .get(n)
                .ok_or_else(|| format!("session {n} out of range ({} found)", groups.len()))?
        }
        None => &events,
    };
    if selected.is_empty() {
        return Err("no events to project".into());
    }
    // A backwards t_mono_ns mixes two timebases; `sessions` always splits
    // there, so per-session projection stays available.
    if let Some(i) =
        (1..selected.len()).find(|&i| selected[i].t_mono_ns < selected[i - 1].t_mono_ns)
    {
        return Err(format!(
            "t_mono_ns goes backwards at event {} (reboot inside the log, or inputs out of order?); project with --session instead",
            i + 1
        ));
    }

    let (track, skipped) = build_track(selected, args.ppq, args.bpm);
    let smf = Smf {
        header: Header::new(Format::SingleTrack, Timing::Metrical(u15::from(args.ppq))),
        tracks: vec![track],
    };
    smf.save(&args.output)
        .map_err(|e| format!("writing {}: {e}", args.output.display()))?;
    Ok(skipped)
}

fn read_jsonl(path: &PathBuf) -> Result<Vec<Event>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut events = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let event: Event = serde_json::from_str(line)
            .map_err(|e| format!("{}:{}: {e}", path.display(), i + 1))?;
        events.push(event);
    }
    Ok(events)
}

/// Build a single Format-0 track: tempo meta, then every event at its projected
/// tick, then end-of-track. Returns the track and the count of skipped events.
fn build_track(events: &[Event], ppq: u16, bpm: u32) -> (Vec<TrackEvent<'static>>, usize) {
    let t0 = events[0].t_mono_ns;
    let mut track = Vec::with_capacity(events.len() + 2);
    let mut prev_tick = 0u64;
    let mut skipped = 0;

    let micros_per_quarter = 60_000_000 / bpm;
    track.push(TrackEvent {
        delta: u28::from(0),
        kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::from(micros_per_quarter))),
    });

    for event in events {
        let Some((channel, message)) = to_midi(&event.msg) else {
            skipped += 1;
            continue;
        };
        let tick = tick_of(event.t_mono_ns - t0, ppq, bpm);
        let delta = (tick - prev_tick).min(u28::max_value().as_int() as u64);
        prev_tick = tick;
        track.push(TrackEvent {
            delta: u28::from(delta as u32),
            kind: TrackEventKind::Midi { channel, message },
        });
    }

    track.push(TrackEvent {
        delta: u28::from(0),
        kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
    });
    (track, skipped)
}

/// Convert a captured message to a channel-voice SMF event. Returns `None` for
/// messages with no faithful SMF form (SysEx/Other), which the log keeps anyway.
fn to_midi(msg: &Message) -> Option<(u4, MidiMessage)> {
    let ch = |c: u8| u4::from(c);
    let vel7 = |v: u16| u7::from(v.min(127) as u8);
    Some(match *msg {
        Message::NoteOn { ch: c, note, vel } => (
            ch(c),
            MidiMessage::NoteOn {
                key: u7::from(note),
                vel: vel7(vel),
            },
        ),
        Message::NoteOff { ch: c, note, vel } => (
            ch(c),
            MidiMessage::NoteOff {
                key: u7::from(note),
                vel: vel7(vel),
            },
        ),
        Message::Cc { ch: c, cc, val } => (
            ch(c),
            MidiMessage::Controller {
                controller: u7::from(cc),
                value: u7::from(val.clamp(0, 127) as u8),
            },
        ),
        Message::PitchBend { ch: c, val } => {
            let centered = val.clamp(-8192, 8191) as i16;
            (
                ch(c),
                MidiMessage::PitchBend {
                    bend: PitchBend::from_int(centered),
                },
            )
        }
        Message::Program { ch: c, prog } => (
            ch(c),
            MidiMessage::ProgramChange {
                program: u7::from(prog),
            },
        ),
        Message::ChannelPressure { ch: c, val } => (
            ch(c),
            MidiMessage::ChannelAftertouch {
                vel: u7::from(val.min(127)),
            },
        ),
        Message::PolyPressure { ch: c, note, val } => (
            ch(c),
            MidiMessage::Aftertouch {
                key: u7::from(note),
                vel: u7::from(val.min(127)),
            },
        ),
        Message::Sysex { .. } | Message::Other { .. } => return None,
    })
}

fn tick_of(dt_ns: u64, ppq: u16, bpm: u32) -> u64 {
    let num = dt_ns as u128 * ppq as u128 * bpm as u128;
    let den = 60_000_000_000u128;
    ((num + den / 2) / den) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_rounding_at_120bpm_480ppq() {
        // 0.5s at 120bpm = 1 beat = 480 ticks.
        assert_eq!(tick_of(500_000_000, 480, 120), 480);
        assert_eq!(tick_of(0, 480, 120), 0);
    }
}
