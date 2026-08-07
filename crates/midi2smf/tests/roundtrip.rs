use std::process::Command;

use midly::{MidiMessage, Smf, TrackEventKind};

/// (delta, kind, note, vel) as read back from the projected SMF.
fn read_notes(mid: &[u8]) -> Vec<(u32, &'static str, u8, u8)> {
    let smf = Smf::parse(mid).expect("valid SMF");
    let mut out = Vec::new();
    for track in &smf.tracks {
        for ev in track {
            if let TrackEventKind::Midi { message, .. } = ev.kind {
                match message {
                    MidiMessage::NoteOn { key, vel } => {
                        out.push((ev.delta.as_int(), "on", key.as_int(), vel.as_int()))
                    }
                    MidiMessage::NoteOff { key, vel } => {
                        out.push((ev.delta.as_int(), "off", key.as_int(), vel.as_int()))
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

#[test]
fn projects_dump_events_losslessly() {
    let s = 1_000_000_000u64;
    let jsonl = [
        (0u64, r#"{"kind":"note_on","ch":0,"note":21,"vel":55}"#),
        (1, r#"{"kind":"note_on","ch":0,"note":23,"vel":66}"#),
        (2, r#"{"kind":"note_off","ch":0,"note":21,"vel":103}"#),
        (3, r#"{"kind":"note_on","ch":0,"note":24,"vel":51}"#),
        (4, r#"{"kind":"note_off","ch":0,"note":23,"vel":86}"#),
        (5, r#"{"kind":"note_on","ch":0,"note":26,"vel":58}"#),
    ]
    .iter()
    .map(|(sec, msg)| {
        format!(
            r#"{{"t_mono_ns":{},"t_wall":"2026-07-09T21:30:{sec:02}Z","src":"24:0",{}"#,
            sec * s,
            &msg[1..]
        )
    })
    .collect::<Vec<_>>()
    .join("\n");

    let dir = std::env::temp_dir();
    let input = dir.join(format!("midi2smf_test_{}.jsonl", std::process::id()));
    let output = dir.join(format!("midi2smf_test_{}.mid", std::process::id()));
    std::fs::write(&input, jsonl).unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_midi2smf"))
        .args(["--input", input.to_str().unwrap()])
        .args(["--output", output.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());

    let mid = std::fs::read(&output).unwrap();
    assert_eq!(&mid[0..4], b"MThd", "output is a Standard MIDI File");
    // 1 s between events at the default 120 bpm / PPQ 480 = 960-tick deltas.
    assert_eq!(
        read_notes(&mid),
        vec![
            (0, "on", 21, 55),
            (960, "on", 23, 66),
            (960, "off", 21, 103),
            (960, "on", 24, 51),
            (960, "off", 23, 86),
            (960, "on", 26, 58),
        ]
    );

    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
}

#[test]
fn backwards_t_mono_ns_needs_session_split() {
    // Simulates a mid-day reboot: CLOCK_MONOTONIC restarts from zero, so the
    // second line's t_mono_ns is below the first one's.
    let jsonl = concat!(
        r#"{"t_mono_ns":5000000000,"t_wall":"2026-07-09T21:30:00Z","src":"24:0","kind":"note_on","ch":0,"note":21,"vel":55}"#,
        "\n",
        r#"{"t_mono_ns":1000000000,"t_wall":"2026-07-09T21:40:00Z","src":"24:0","kind":"note_on","ch":0,"note":23,"vel":66}"#,
    );

    let dir = std::env::temp_dir();
    let input = dir.join(format!("midi2smf_reboot_{}.jsonl", std::process::id()));
    let output = dir.join(format!("midi2smf_reboot_{}.mid", std::process::id()));
    std::fs::write(&input, jsonl).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_midi2smf"))
        .args(["--input", input.to_str().unwrap()])
        .args(["--output", output.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("backwards"));

    // The reboot is a session boundary, so per-session projection works.
    let status = Command::new(env!("CARGO_BIN_EXE_midi2smf"))
        .args(["--input", input.to_str().unwrap()])
        .args(["--output", output.to_str().unwrap()])
        .args(["--session", "1"])
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        read_notes(&std::fs::read(&output).unwrap()),
        vec![(0, "on", 23, 66)]
    );

    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
}
