//! Follow the capture log as midilogd appends to it.
//!
//! The log is laid out as `<dir>/YYYY/MM/DD.jsonl` (local date, one JSON event
//! per line). Attaching starts at the end of today's file: the exporter never
//! replays history, so a restart shows up in Prometheus as an ordinary counter
//! reset instead of a mid-replay ramp that `increase()` would double-count.

use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;

use chrono::NaiveDate;
use midi_event::Event;

pub struct Tail {
    dir: PathBuf,
    current: Option<OpenDay>,
}

struct OpenDay {
    date: NaiveDate,
    path: PathBuf,
    file: File,
    /// Bytes after the last newline seen; a line is parsed only once complete.
    partial: Vec<u8>,
}

impl Tail {
    /// Attach to the log, skipping whatever today's file already contains. A
    /// trailing unterminated line is kept pending so it still parses once
    /// midilogd finishes writing it.
    pub fn new(dir: PathBuf, today: NaiveDate) -> io::Result<Self> {
        let mut tail = Self { dir, current: None };
        if let Some(mut open) = tail.try_open(today)? {
            open.skip_existing()?;
            tail.current = Some(open);
        }
        Ok(tail)
    }

    /// Read every complete line appended since the last call. On a date change
    /// the previous file is drained before switching, so nothing written just
    /// before midnight is lost; until the new day's file exists the old one
    /// stays watched.
    pub fn poll(&mut self, today: NaiveDate) -> io::Result<Vec<Event>> {
        let mut events = Vec::new();
        match &mut self.current {
            None => {
                // The file appeared after we attached: everything in it is new.
                if let Some(mut open) = self.try_open(today)? {
                    open.read_appended(&mut events)?;
                    self.current = Some(open);
                }
            }
            Some(open) => {
                open.read_appended(&mut events)?;
                if open.date != today
                    && let Some(mut next) = self.try_open(today)?
                {
                    next.read_appended(&mut events)?;
                    self.current = Some(next);
                }
            }
        }
        Ok(events)
    }

    fn try_open(&self, date: NaiveDate) -> io::Result<Option<OpenDay>> {
        let path = midi_event::capture_path(&self.dir, date);
        match File::open(&path) {
            Ok(file) => Ok(Some(OpenDay {
                date,
                path,
                file,
                partial: Vec::new(),
            })),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl OpenDay {
    /// Consume the existing contents without emitting events, leaving the
    /// cursor at EOF and any trailing partial line pending.
    fn skip_existing(&mut self) -> io::Result<()> {
        let mut buf = Vec::new();
        self.file.read_to_end(&mut buf)?;
        let after_last_newline = buf
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |pos| pos + 1);
        self.partial = buf.split_off(after_last_newline);
        Ok(())
    }

    /// Parse every line completed since the last read into `events`. Bad lines
    /// are reported and skipped: this is a derived view, so losing one line is
    /// acceptable where killing the exporter is not.
    fn read_appended(&mut self, events: &mut Vec<Event>) -> io::Result<()> {
        self.file.read_to_end(&mut self.partial)?;
        let mut start = 0;
        while let Some(offset) = self.partial[start..].iter().position(|&b| b == b'\n') {
            let line = &self.partial[start..start + offset];
            if !line.is_empty() {
                match serde_json::from_slice::<Event>(line) {
                    Ok(event) => events.push(event),
                    Err(e) => {
                        eprintln!("{}: skipping unparseable line: {e}", self.path.display())
                    }
                }
            }
            start += offset + 1;
        }
        self.partial.drain(..start);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use midi_event::Message;
    use std::fs;
    use std::io::Write;
    use std::path::Path;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("midi_exporter_tail_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn day_path(dir: &Path, date: NaiveDate) -> PathBuf {
        midi_event::capture_path(dir, date)
    }

    fn append(dir: &Path, date: NaiveDate, text: &str) {
        let path = day_path(dir, date);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn line(t_mono_ns: u64) -> String {
        let event = Event {
            t_mono_ns,
            t_wall: "2026-07-11T21:30:00+09:00".into(),
            src: "24:0".into(),
            group: 0,
            msg: Message::NoteOn {
                ch: 0,
                note: 21,
                vel: 55,
            },
        };
        let mut json = serde_json::to_string(&event).unwrap();
        json.push('\n');
        json
    }

    const D1: NaiveDate = NaiveDate::from_ymd_opt(2026, 7, 11).unwrap();
    const D2: NaiveDate = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();

    #[test]
    fn attach_skips_existing_and_sees_appends() {
        let dir = scratch("attach");
        append(&dir, D1, &line(1));
        append(&dir, D1, &line(2));

        let mut tail = Tail::new(dir.clone(), D1).unwrap();
        assert!(tail.poll(D1).unwrap().is_empty());

        append(&dir, D1, &line(3));
        let events = tail.poll(D1).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].t_mono_ns, 3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_created_after_attach_is_read_from_start() {
        let dir = scratch("created_later");
        let mut tail = Tail::new(dir.clone(), D1).unwrap();
        assert!(tail.poll(D1).unwrap().is_empty());

        append(&dir, D1, &line(1));
        append(&dir, D1, &line(2));
        let events = tail.poll(D1).unwrap();
        assert_eq!(events.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_line_is_carried_until_complete() {
        let dir = scratch("partial");
        append(&dir, D1, "");
        let mut tail = Tail::new(dir.clone(), D1).unwrap();

        let full = line(1);
        let (head, rest) = full.split_at(10);
        append(&dir, D1, head);
        assert!(tail.poll(D1).unwrap().is_empty());

        append(&dir, D1, rest);
        let events = tail.poll(D1).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].t_mono_ns, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn trailing_partial_at_attach_still_parses() {
        let dir = scratch("attach_partial");
        let full = line(1);
        let (head, rest) = full.split_at(10);
        append(&dir, D1, &line(7));
        append(&dir, D1, head);

        let mut tail = Tail::new(dir.clone(), D1).unwrap();
        append(&dir, D1, rest);
        let events = tail.poll(D1).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].t_mono_ns, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_line_is_skipped_not_fatal() {
        let dir = scratch("bad_line");
        append(&dir, D1, "");
        let mut tail = Tail::new(dir.clone(), D1).unwrap();

        append(&dir, D1, "not json\n");
        append(&dir, D1, &line(1));
        let events = tail.poll(D1).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].t_mono_ns, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn day_rollover_drains_old_then_reads_new_from_start() {
        let dir = scratch("rollover");
        append(&dir, D1, "");
        let mut tail = Tail::new(dir.clone(), D1).unwrap();

        append(&dir, D1, &line(1));
        // New day, but midilogd has not written the new file yet: the old file
        // stays watched.
        let events = tail.poll(D2).unwrap();
        assert_eq!(events.len(), 1);

        append(&dir, D1, &line(2));
        append(&dir, D2, &line(3));
        let events = tail.poll(D2).unwrap();
        assert_eq!(
            events.iter().map(|e| e.t_mono_ns).collect::<Vec<_>>(),
            vec![2, 3]
        );

        // Old file is no longer watched; only the new one is.
        append(&dir, D2, &line(4));
        let events = tail.poll(D2).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].t_mono_ns, 4);
        let _ = fs::remove_dir_all(&dir);
    }
}
