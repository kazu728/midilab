//! Shared event schema for the midilogd pipeline.
//!
//! The capture log is the source of truth: one JSON object per line, each a
//! faithful record of a MIDI event carrying both a monotonic and a wall-clock
//! timestamp. Values are stored as they arrive on the wire (7-bit velocity in a
//! `u16`, centered pitch bend); any normalization to MIDI 2.0 widths is a
//! downstream *view*, not something the logger bakes in.
//!
//! Sessions are likewise a view: [`sessions`] derives them from silence gaps at
//! read time, so the gap threshold stays tunable and is never written to disk.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// One captured MIDI event — a single line in the append-only log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Monotonic clock (CLOCK_MONOTONIC) at capture, in nanoseconds. Sole basis
    /// for ordering and inter-event gaps; immune to wall-clock jumps (NTP). It
    /// stalls during suspend and restarts from zero on reboot, so a backwards
    /// step between consecutive lines marks a reboot (see [`sessions`]).
    pub t_mono_ns: u64,
    /// Wall-clock at capture, RFC 3339. Answers "when did I play this".
    pub t_wall: String,
    /// ALSA sequencer source address, e.g. `"24:0"`.
    pub src: String,
    /// UMP group. Reserved for future MIDI 2.0 input; always 0 for MIDI 1.0.
    #[serde(default)]
    pub group: u8,
    /// The message itself, tagged by `kind` on the wire.
    #[serde(flatten)]
    pub msg: Message,
}

/// A MIDI message payload. Serialized with an internal `kind` tag so each log
/// line self-describes, e.g. `{"kind":"note_on","ch":0,"note":21,"vel":55}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Message {
    NoteOn {
        ch: u8,
        note: u8,
        vel: u16,
    },
    NoteOff {
        ch: u8,
        note: u8,
        vel: u16,
    },
    /// Control change; `val` is the raw 0-127 controller value.
    Cc {
        ch: u8,
        cc: u8,
        val: i32,
    },
    /// Pitch bend, centered as ALSA reports it: -8192..=8191.
    PitchBend {
        ch: u8,
        val: i32,
    },
    Program {
        ch: u8,
        prog: u8,
    },
    ChannelPressure {
        ch: u8,
        val: u8,
    },
    PolyPressure {
        ch: u8,
        note: u8,
        val: u8,
    },
    /// System exclusive, kept as lowercase hex of the full `F0..F7` frame.
    Sysex {
        raw: String,
    },
    /// Any other MIDI message, kept as hex of the wire bytes ALSA emits for it
    /// so the log stays forward-compatible. Real-time stream chatter (clock,
    /// tick, active sensing) is deliberately dropped at capture, not logged.
    Other {
        raw: String,
    },
}

/// Split events into sessions, starting a new one whenever the monotonic gap to
/// the previous event exceeds `gap`. Returns borrowed slices in input order;
/// the input is expected in log (append) order. A backwards step in
/// [`Event::t_mono_ns`] means a reboot happened between the two lines, so it is
/// always a session boundary regardless of `gap`.
pub fn sessions(events: &[Event], gap: Duration) -> Vec<&[Event]> {
    let gap_ns = gap.as_nanos().min(u64::MAX as u128) as u64;
    let mut out = Vec::new();
    let mut start = 0;
    for i in 1..events.len() {
        let (prev, cur) = (events[i - 1].t_mono_ns, events[i].t_mono_ns);
        if cur < prev || cur - prev > gap_ns {
            out.push(&events[start..i]);
            start = i;
        }
    }
    if !events.is_empty() {
        out.push(&events[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t_mono_ns: u64, msg: Message) -> Event {
        Event {
            t_mono_ns,
            t_wall: "2026-07-09T21:30:00Z".into(),
            src: "24:0".into(),
            group: 0,
            msg,
        }
    }

    #[test]
    fn note_on_json_shape() {
        let e = ev(
            1_000,
            Message::NoteOn {
                ch: 0,
                note: 21,
                vel: 55,
            },
        );
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(
            json,
            r#"{"t_mono_ns":1000,"t_wall":"2026-07-09T21:30:00Z","src":"24:0","group":0,"kind":"note_on","ch":0,"note":21,"vel":55}"#
        );
    }

    #[test]
    fn round_trip_every_variant() {
        let msgs = [
            Message::NoteOn {
                ch: 1,
                note: 60,
                vel: 100,
            },
            Message::NoteOff {
                ch: 1,
                note: 60,
                vel: 0,
            },
            Message::Cc {
                ch: 0,
                cc: 64,
                val: 127,
            },
            Message::PitchBend { ch: 0, val: -8192 },
            Message::Program { ch: 2, prog: 5 },
            Message::ChannelPressure { ch: 0, val: 40 },
            Message::PolyPressure {
                ch: 0,
                note: 60,
                val: 40,
            },
            Message::Sysex {
                raw: "f07e7f0601f7".into(),
            },
            Message::Other { raw: "f8".into() },
        ];
        for m in msgs {
            let e = ev(42, m);
            let json = serde_json::to_string(&e).unwrap();
            let back: Event = serde_json::from_str(&json).unwrap();
            assert_eq!(e, back);
        }
    }

    #[test]
    fn group_defaults_when_absent() {
        // Older lines written without a `group` field still parse.
        let line = r#"{"t_mono_ns":1,"t_wall":"2026-07-09T21:30:00Z","src":"24:0","kind":"note_on","ch":0,"note":21,"vel":55}"#;
        let e: Event = serde_json::from_str(line).unwrap();
        assert_eq!(e.group, 0);
    }

    #[test]
    fn sessions_split_on_gap() {
        let s = 1_000_000_000u64; // 1 second in ns
        let events = vec![
            ev(
                0,
                Message::NoteOn {
                    ch: 0,
                    note: 21,
                    vel: 55,
                },
            ),
            ev(
                s,
                Message::NoteOff {
                    ch: 0,
                    note: 21,
                    vel: 0,
                },
            ),
            // 6-minute silence -> new session
            ev(
                361 * s,
                Message::NoteOn {
                    ch: 0,
                    note: 23,
                    vel: 66,
                },
            ),
            ev(
                362 * s,
                Message::NoteOff {
                    ch: 0,
                    note: 23,
                    vel: 0,
                },
            ),
        ];
        let groups = sessions(&events, Duration::from_secs(300));
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 2);
        assert_eq!(groups[1].len(), 2);
    }

    #[test]
    fn sessions_split_on_clock_regression() {
        // t_mono_ns going backwards = reboot between the lines: always a boundary.
        let n = |t| {
            ev(
                t,
                Message::NoteOn {
                    ch: 0,
                    note: 21,
                    vel: 55,
                },
            )
        };
        let events = vec![n(100), n(200), n(50), n(60)];
        let groups = sessions(&events, Duration::from_secs(300));
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 2);
        assert_eq!(groups[1].len(), 2);
    }

    #[test]
    fn sessions_empty_and_single() {
        assert!(sessions(&[], Duration::from_secs(300)).is_empty());
        let one = vec![ev(
            0,
            Message::NoteOn {
                ch: 0,
                note: 21,
                vel: 55,
            },
        )];
        assert_eq!(sessions(&one, Duration::from_secs(300)).len(), 1);
    }
}
