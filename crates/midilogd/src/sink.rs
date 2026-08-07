//! Each event is synced before return, so a crash can lose only the in-flight
//! event; the raw log remains usable without finalizing an SMF on exit.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use chrono::NaiveDate;
use midi_event::Event;

pub struct JsonlSink {
    root: PathBuf,
    open: Option<OpenDay>,
}

struct OpenDay {
    date: NaiveDate,
    file: File,
}

impl JsonlSink {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            open: None,
        }
    }

    /// Append one event to the file for `date`, rotating on day change and
    /// syncing to disk before returning.
    pub fn append(&mut self, event: &Event, date: NaiveDate) -> io::Result<()> {
        if self.open.as_ref().is_none_or(|o| o.date != date) {
            let path = midi_event::capture_path(&self.root, date);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = OpenOptions::new()
                .read(true)
                .create(true)
                .append(true)
                .open(&path)?;
            // A previous process may have died mid-line, leaving a record with
            // no terminating newline. Close it off so this append starts a fresh
            // line instead of splicing onto the torn remnant.
            if ends_mid_line(&mut file)? {
                file.write_all(b"\n")?;
                file.sync_data()?;
            }
            self.open = Some(OpenDay { date, file });
        }

        let file = &mut self.open.as_mut().expect("open set above").file;
        let mut line = serde_json::to_string(event).map_err(io::Error::other)?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        file.sync_data()?;
        Ok(())
    }
}

fn ends_mid_line(file: &mut File) -> io::Result<bool> {
    let len = file.seek(SeekFrom::End(0))?;
    if len == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8];
    file.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
}

#[cfg(test)]
mod tests {
    use super::*;
    use midi_event::Message;

    fn ev(t_mono_ns: u64) -> Event {
        Event {
            t_mono_ns,
            t_wall: "2026-07-09T21:30:00Z".into(),
            src: "24:0".into(),
            group: 0,
            msg: Message::NoteOn {
                ch: 0,
                note: 21,
                vel: 55,
            },
        }
    }

    #[test]
    fn rotates_by_day_and_stays_readable() {
        let dir = std::env::temp_dir().join(format!("midilogd_sink_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let mut sink = JsonlSink::new(&dir);
        let d1 = NaiveDate::from_ymd_opt(2026, 7, 9).unwrap();
        let d2 = NaiveDate::from_ymd_opt(2026, 7, 10).unwrap();
        sink.append(&ev(1), d1).unwrap();
        sink.append(&ev(2), d1).unwrap();
        sink.append(&ev(3), d2).unwrap();

        let day1 = fs::read_to_string(dir.join("2026/07/09.jsonl")).unwrap();
        assert_eq!(day1.lines().count(), 2);
        let first: Event = serde_json::from_str(day1.lines().next().unwrap()).unwrap();
        assert_eq!(first, ev(1));

        let day2 = fs::read_to_string(dir.join("2026/07/10.jsonl")).unwrap();
        assert_eq!(day2.lines().count(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_torn_trailing_line_is_closed_before_the_next_append() {
        let dir = std::env::temp_dir().join(format!("midilogd_sink_torn_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let d = NaiveDate::from_ymd_opt(2026, 7, 9).unwrap();
        let path = midi_event::capture_path(&dir, d);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, br#"{"t_mono_ns":1,"kind":"note"#).unwrap();

        JsonlSink::new(&dir).append(&ev(2), d).unwrap();

        let body = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(serde_json::from_str::<Event>(lines[1]).unwrap(), ev(2));
        let _ = fs::remove_dir_all(&dir);
    }
}
