//! Faithful mapping from an ALSA sequencer event to the capture schema.
//!
//! Values are recorded as ALSA hands them over — a note-on with velocity 0 stays
//! a `note_on`, pitch bend stays centered — because the log is the source of
//! truth and any interpretation belongs to a later view. Messages not modeled
//! explicitly are kept as [`Message::Other`], holding the MIDI 1.0 wire bytes
//! ALSA emits for them, with two deliberate exceptions: real-time stream
//! chatter (clock, tick, active sensing) is dropped as noise, and channel-voice
//! values outside the 7-bit wire range (injectable via other sequencer clients,
//! never sent by the piano) are dropped rather than silently truncated.

use alsa::seq::{Addr, EvCtrl, EvNote, Event, EventType, MidiEvent};
use midi_event::Message;

/// `"client:port"` of the event's source, e.g. `"24:0"`.
pub fn source_of(ev: &Event) -> String {
    let Addr { client, port } = ev.get_source();
    format!("{client}:{port}")
}

/// Map an ALSA event to a capture [`Message`], or `None` for the deliberate
/// drops described in the module docs (and for non-MIDI sequencer events).
pub fn to_message(codec: &MidiEvent, ev: &mut Event) -> Option<Message> {
    match ev.get_type() {
        EventType::Noteon => {
            let n: EvNote = ev.get_data()?;
            Some(Message::NoteOn {
                ch: n.channel,
                note: n.note,
                vel: n.velocity as u16,
            })
        }
        EventType::Noteoff => {
            let n: EvNote = ev.get_data()?;
            Some(Message::NoteOff {
                ch: n.channel,
                note: n.note,
                vel: n.velocity as u16,
            })
        }
        EventType::Keypress => {
            let n: EvNote = ev.get_data()?;
            Some(Message::PolyPressure {
                ch: n.channel,
                note: n.note,
                val: n.velocity,
            })
        }
        EventType::Controller => {
            let c: EvCtrl = ev.get_data()?;
            Some(Message::Cc {
                ch: c.channel,
                cc: wire7(c.param)?,
                val: c.value,
            })
        }
        EventType::Pgmchange => {
            let c: EvCtrl = ev.get_data()?;
            Some(Message::Program {
                ch: c.channel,
                prog: wire7(c.value)?,
            })
        }
        EventType::Chanpress => {
            let c: EvCtrl = ev.get_data()?;
            Some(Message::ChannelPressure {
                ch: c.channel,
                val: wire7(c.value)?,
            })
        }
        EventType::Pitchbend => {
            let c: EvCtrl = ev.get_data()?;
            Some(Message::PitchBend {
                ch: c.channel,
                val: c.value,
            })
        }
        EventType::Sysex => Some(Message::Sysex {
            raw: to_hex(ev.get_ext()?),
        }),
        // Real-time stream chatter: hundreds of events per second, no musical
        // content — the one thing deliberately not logged.
        EventType::Clock | EventType::Tick | EventType::Sensing => None,
        _ => other(codec, ev),
    }
}

/// A value goes into a 7-bit schema field only if it could appear on a MIDI 1.0
/// wire; out-of-range values make the caller drop the event instead of writing
/// a truncated value that masquerades as real.
fn wire7<T: TryInto<u8>>(v: T) -> Option<u8> {
    v.try_into().ok().filter(|b| *b <= 127)
}

/// Keep an unmodeled message as the wire bytes ALSA emits for it. Non-MIDI
/// sequencer events (client/port management, …) fail to decode and drop out.
fn other(codec: &MidiEvent, ev: &mut Event) -> Option<Message> {
    // Longest non-SysEx decode is an (N)RPN expansion: 4 CCs = 12 bytes.
    let mut buf = [0u8; 16];
    let len = codec.decode(&mut buf, ev).ok()?;
    (len > 0).then(|| Message::Other {
        raw: to_hex(&buf[..len]),
    })
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}
