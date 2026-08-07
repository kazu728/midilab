//! Attach at today's EOF and advance across local-date files without replaying
//! history, so exporter restarts behave as ordinary counter resets.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use chrono::NaiveDate;
use midi_event::Event;

pub struct Tail {
    dir: PathBuf,
    /// Date of the file currently followed.
    date: NaiveDate,
    open: Option<OpenFile>,
}

struct OpenFile {
    path: PathBuf,
    file: File,
    /// Inode of `file`, to notice the path being replaced under us.
    ino: u64,
    /// Bytes consumed from `file`, to notice it being truncated under us.
    read: u64,
    /// Bytes after the last newline seen; a line is parsed only once complete.
    partial: Vec<u8>,
}

impl Tail {
    /// Attach to the log, skipping whatever today's file already contains. A
    /// trailing unterminated line is kept pending so it still parses once
    /// midilogd finishes writing it.
    pub fn new(dir: PathBuf, today: NaiveDate) -> io::Result<Self> {
        let mut tail = Self {
            dir,
            date: today,
            open: None,
        };
        if let Some(mut open) = tail.try_open(today)? {
            open.skip_existing()?;
            tail.open = Some(open);
        }
        Ok(tail)
    }

    /// The tracked date only moves forward, and only once a later day's file
    /// exists — the reliable sign that midilogd has rotated, since its own
    /// clock (not the exporter's) picks the file. The old file is drained one
    /// last time at that point, so an event written just before midnight is
    /// never lost; a backward clock step is ignored rather than replaying an
    /// already-counted file.
    pub fn poll(&mut self, today: NaiveDate) -> io::Result<Vec<Event>> {
        let mut events = Vec::new();
        self.read_current(&mut events)?;
        while today > self.date && self.later_file_exists(today) {
            self.read_current(&mut events)?;
            self.date = self.date.succ_opt().unwrap_or(today);
            self.open = None;
            self.read_current(&mut events)?;
        }
        Ok(events)
    }

    fn read_current(&mut self, events: &mut Vec<Event>) -> io::Result<()> {
        let stale = match &self.open {
            Some(open) => open.stale()?,
            None => false,
        };
        if stale {
            self.open = None;
        }
        if self.open.is_none() {
            self.open = self.try_open(self.date)?;
        }
        if let Some(open) = &mut self.open {
            open.read_appended(events)?;
        }
        Ok(())
    }

    /// The signal that midilogd has moved on and the current file is final:
    /// some later day up to `today` already has a file.
    fn later_file_exists(&self, today: NaiveDate) -> bool {
        let mut date = self.date;
        while let Some(next) = date.succ_opt() {
            if next > today {
                return false;
            }
            if midi_event::capture_path(&self.dir, next).exists() {
                return true;
            }
            date = next;
        }
        false
    }

    fn try_open(&self, date: NaiveDate) -> io::Result<Option<OpenFile>> {
        let path = midi_event::capture_path(&self.dir, date);
        match File::open(&path) {
            Ok(file) => {
                let ino = file.metadata()?.ino();
                Ok(Some(OpenFile {
                    path,
                    file,
                    ino,
                    read: 0,
                    partial: Vec::new(),
                }))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl OpenFile {
    /// Leave the cursor at EOF and any trailing partial line pending.
    fn skip_existing(&mut self) -> io::Result<()> {
        let mut buf = Vec::new();
        self.read += self.file.read_to_end(&mut buf)? as u64;
        let after_last_newline = buf
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |pos| pos + 1);
        self.partial = buf.split_off(after_last_newline);
        Ok(())
    }

    /// Whether the file we hold is no longer the live one: the path now
    /// resolves to a different inode (replaced), or it has shrunk below what we
    /// have already read (truncated). A truncation that regrows past the old
    /// offset before the next poll would be indistinguishable from ordinary
    /// growth, but this log is appended a few KB/s at most, so the shrink is
    /// always observed first.
    fn stale(&self) -> io::Result<bool> {
        match fs::metadata(&self.path) {
            Ok(m) => Ok(m.ino() != self.ino || self.file.metadata()?.len() < self.read),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(e) => Err(e),
        }
    }

    /// Bad lines are reported and skipped: this is a derived view, so losing
    /// one line is acceptable where killing the exporter is not.
    fn read_appended(&mut self, events: &mut Vec<Event>) -> io::Result<()> {
        self.read += self.file.read_to_end(&mut self.partial)? as u64;
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

    fn append(dir: &Path, date: NaiveDate, text: &str) {
        let path = midi_event::capture_path(dir, date);
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
    const D3: NaiveDate = NaiveDate::from_ymd_opt(2026, 7, 13).unwrap();

    fn mono(events: &[Event]) -> Vec<u64> {
        events.iter().map(|e| e.t_mono_ns).collect()
    }

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

    #[test]
    fn file_first_written_just_before_midnight_is_not_lost() {
        // Attached on D1 before its file exists; the very first event lands on
        // D1 in the window between the last D1 poll and the clock reaching D2.
        let dir = scratch("pre_midnight");
        let mut tail = Tail::new(dir.clone(), D1).unwrap();
        assert!(tail.poll(D1).unwrap().is_empty());

        append(&dir, D1, &line(1));
        assert_eq!(mono(&tail.poll(D2).unwrap()), vec![1]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn backward_clock_step_does_not_replay() {
        let dir = scratch("clock_back");
        append(&dir, D1, "");
        let mut tail = Tail::new(dir.clone(), D1).unwrap();

        append(&dir, D1, &line(1));
        append(&dir, D2, &line(2));
        assert_eq!(mono(&tail.poll(D2).unwrap()), vec![1, 2]);

        // Clock steps back to D1: already-counted files must not be replayed.
        assert!(tail.poll(D1).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn catches_up_across_a_silent_day() {
        // D2 has no file (nobody played); the exporter wakes with today at D3
        // and must still read D1's tail and D3 without stalling on the gap.
        let dir = scratch("silent_day");
        append(&dir, D1, "");
        let mut tail = Tail::new(dir.clone(), D1).unwrap();

        append(&dir, D1, &line(1));
        append(&dir, D3, &line(3));
        assert_eq!(mono(&tail.poll(D3).unwrap()), vec![1, 3]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncated_file_is_reread_from_its_new_start() {
        let dir = scratch("truncate");
        append(&dir, D1, &line(1));
        let mut tail = Tail::new(dir.clone(), D1).unwrap();
        append(&dir, D1, &line(2));
        assert_eq!(mono(&tail.poll(D1).unwrap()), vec![2]);

        fs::write(midi_event::capture_path(&dir, D1), "").unwrap();
        append(&dir, D1, &line(3));
        assert_eq!(mono(&tail.poll(D1).unwrap()), vec![3]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn replaced_file_is_reopened() {
        let dir = scratch("replaced");
        append(&dir, D1, &line(1));
        let mut tail = Tail::new(dir.clone(), D1).unwrap();

        // The file is removed and recreated as a different inode. The open
        // handle keeps the old inode alive, so the new file is genuinely other.
        fs::remove_file(midi_event::capture_path(&dir, D1)).unwrap();
        append(&dir, D1, &line(2));
        assert_eq!(mono(&tail.poll(D1).unwrap()), vec![2]);
        let _ = fs::remove_dir_all(&dir);
    }
}
