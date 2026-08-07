//! Preserve wire values in the log; sessions are a derived silence-gap view, so
//! normalization and threshold choices stay out of the on-disk data.

use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Monotonic clock (CLOCK_MONOTONIC) at capture, in nanoseconds. Sole basis
    /// for ordering and inter-event gaps; immune to wall-clock jumps (NTP);
    /// stalls during suspend and restarts from zero on reboot (see
    /// [`sessions`]).
    pub t_mono_ns: u64,
    /// Wall-clock at capture, RFC 3339. Answers "when did I play this".
    pub t_wall: String,
    /// ALSA sequencer source address, e.g. `"24:0"`.
    pub src: String,
    /// UMP group. Reserved for future MIDI 2.0 input; always 0 for MIDI 1.0.
    #[serde(default)]
    pub group: u8,
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
    /// so the log stays forward-compatible.
    Other {
        raw: String,
    },
}

/// On-disk location of the capture log for `date`: `<root>/YYYY/MM/DD.jsonl`.
///
/// The append log is partitioned by local date. The writer and every reader
/// must agree on this layout byte-for-byte, so it lives here beside the line
/// schema instead of being reimplemented at each end.
pub fn capture_path(root: &Path, date: NaiveDate) -> PathBuf {
    root.join(format!("{:04}", date.year()))
        .join(format!("{:02}", date.month()))
        .join(format!("{:02}.jsonl", date.day()))
}

/// The silence-gap rule that delimits a playing session. Consecutive events
/// stay in the same session while the monotonic clock advances by at most
/// `gap`; a longer advance is silence between sessions, and a backwards step is
/// a reboot (the monotonic clock restarts from zero). Both begin a new session.
///
/// This is the single owner of the boundary rule: [`sessions`] groups by it and
/// the exporter accumulates played time with it, so the two never drift.
#[derive(Clone, Copy)]
pub struct SessionGap {
    gap_ns: u64,
}

impl SessionGap {
    pub fn new(gap: Duration) -> Self {
        Self {
            gap_ns: gap.as_nanos().min(u64::MAX as u128) as u64,
        }
    }

    /// The monotonic nanoseconds from `prev` to `cur` that count as continuous
    /// playing, or `None` when the step crosses a session boundary.
    pub fn played(&self, prev: u64, cur: u64) -> Option<u64> {
        match cur.checked_sub(prev) {
            Some(dt) if dt <= self.gap_ns => Some(dt),
            _ => None,
        }
    }
}

/// Split events into sessions by [`SessionGap`]. Returns borrowed slices in
/// input order; the input is expected in log (append) order.
pub fn sessions(events: &[Event], gap: Duration) -> Vec<&[Event]> {
    let gap = SessionGap::new(gap);
    let mut out = Vec::new();
    let mut start = 0;
    for i in 1..events.len() {
        if gap
            .played(events[i - 1].t_mono_ns, events[i].t_mono_ns)
            .is_none()
        {
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
    fn capture_path_layout() {
        let p = capture_path(
            Path::new("capture"),
            NaiveDate::from_ymd_opt(2026, 7, 9).unwrap(),
        );
        assert_eq!(p, Path::new("capture/2026/07/09.jsonl"));
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
        let s = 1_000_000_000u64;
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
    fn session_gap_played_span_or_boundary() {
        let s = 1_000_000_000u64;
        let gap = SessionGap::new(Duration::from_secs(300));
        assert_eq!(gap.played(0, 2 * s), Some(2 * s)); // within the gap
        assert_eq!(gap.played(0, 301 * s), None); // silence beyond the gap
        assert_eq!(gap.played(200, 100), None); // backwards step = reboot
        assert_eq!(gap.played(5, 5), Some(0)); // no advance stays in-session
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
