//! Append-only JSONL sink with per-day file rotation.
//!
//! Every line is fsync'd before returning, so an abrupt `kill -9` or power loss
//! costs at most the in-flight event — never the whole day. This is the crux of
//! why the capture is a raw append log rather than a single SMF finalized on
//! exit (which loses everything if the writer dies).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
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

    /// `<root>/YYYY/MM/DD.jsonl` for the given day.
    pub fn path_for(&self, date: NaiveDate) -> PathBuf {
        midi_event::capture_path(&self.root, date)
    }

    /// Append one event to the file for `date`, rotating on day change and
    /// syncing to disk before returning.
    pub fn append(&mut self, event: &Event, date: NaiveDate) -> io::Result<()> {
        if self.open.as_ref().is_none_or(|o| o.date != date) {
            let path = self.path_for(date);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
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
}
