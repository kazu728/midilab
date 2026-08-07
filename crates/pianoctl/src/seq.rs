use std::time::Instant;

use alsa::Direction;
use alsa::poll::{self, Descriptors as _};
use alsa::seq::{
    Addr, ClientIter, Event, EventType, PortCap, PortInfo, PortIter, PortSubscribe, PortType, Seq,
};

use crate::sysex::{Reassembler, Request};

const MAX_OVERRUNS: usize = 8;

const SYSTEM_ANNOUNCE: Addr = Addr { client: 0, port: 1 };

pub struct Session {
    seq: Seq,
    port: Addr,
    source: Addr,
    dest: Addr,
    reassembler: Reassembler,
    overruns: usize,
}

#[derive(Default)]
pub struct FrameBatch {
    pub frames: Vec<Vec<u8>>,
    pub error: Option<String>,
}

impl Session {
    pub fn open(needle: &str) -> Result<Self, String> {
        let seq =
            Seq::open(None, None, true).map_err(|e| format!("opening ALSA sequencer: {e}"))?;
        seq.set_client_name(c"pianoctl")
            .map_err(|e| format!("setting client name: {e}"))?;

        let mut info = PortInfo::empty().map_err(|e| format!("allocating port info: {e}"))?;
        info.set_name(c"control");
        info.set_capability(
            PortCap::READ | PortCap::SUBS_READ | PortCap::WRITE | PortCap::SUBS_WRITE,
        );
        info.set_type(PortType::MIDI_GENERIC | PortType::APPLICATION);
        seq.create_port(&info)
            .map_err(|e| format!("creating control port: {e}"))?;
        let port = info.addr();

        let own = seq
            .client_id()
            .map_err(|e| format!("reading our client id: {e}"))?;
        let (source, dest) = find_device(&seq, needle, own).ok_or_else(|| {
            format!("no ALSA client matching {needle:?} is readable and writable")
        })?;

        subscribe(&seq, source, port)
            .map_err(|e| format!("subscribing to {}:{}: {e}", source.client, source.port))?;
        // Addressing an event is not enough to reach a hardware port: the
        // kernel's rawmidi bridge opens the device's output stream when the
        // port is first subscribed to, and until then it refuses every
        // delivery with ENODEV — direct ones included.
        subscribe(&seq, port, dest)
            .map_err(|e| format!("connecting to {}:{}: {e}", dest.client, dest.port))?;
        // Unplugging the piano mid-measurement must not read as silence: with
        // the announce subscription the client's departure arrives as an event
        // and every wait can fail loudly instead of timing out (SYSEX.md §6.2
        // records an unanswered request as a fact about the firmware).
        subscribe(&seq, SYSTEM_ANNOUNCE, port)
            .map_err(|e| format!("subscribing to System:Announce: {e}"))?;

        Ok(Self {
            seq,
            port,
            source,
            dest,
            reassembler: Reassembler::default(),
            overruns: 0,
        })
    }

    pub fn source(&self) -> Addr {
        self.source
    }

    pub fn dest(&self) -> Addr {
        self.dest
    }

    pub fn send(&self, request: &Request) -> Result<(), String> {
        let mut event = Event::new_ext(EventType::Sysex, request.bytes());
        event.set_source(self.port.port);
        event.set_dest(self.dest);
        event.set_direct();
        self.seq
            .event_output(&mut event)
            .map_err(|e| format!("sending request: {e}"))?;
        self.seq
            .drain_output()
            .map_err(|e| format!("flushing request: {e}"))?;
        Ok(())
    }

    pub fn frames(&mut self, deadline: Instant) -> Result<FrameBatch, String> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(FrameBatch::default());
            }
            let descriptors = (&self.seq, Some(Direction::Capture));
            let mut fds = descriptors
                .get()
                .map_err(|e| format!("collecting poll descriptors: {e}"))?;
            poll::poll(
                &mut fds,
                remaining.as_millis().try_into().unwrap_or(i32::MAX),
            )
            .map_err(|e| format!("waiting for a reply: {e}"))?;
            // A descriptor in error stays readable forever as far as poll is
            // concerned; without this the loop would spin until the deadline.
            let revents = descriptors
                .revents(&fds)
                .map_err(|e| format!("reading poll result: {e}"))?;
            let mut batch = self.drain();
            if batch.error.is_none()
                && revents.intersects(poll::Flags::ERR | poll::Flags::HUP | poll::Flags::NVAL)
            {
                batch.error = Some(format!("the sequencer connection failed ({revents:?})"));
            }
            if !batch.frames.is_empty() || batch.error.is_some() {
                return Ok(batch);
            }
        }
    }

    fn drain(&mut self) -> FrameBatch {
        let mut done = Vec::new();
        let mut error = None;
        let abandoned_before = self.reassembler.abandoned();
        let mut input = self.seq.input();
        loop {
            match input.event_input() {
                Ok(event) => {
                    self.overruns = 0;
                    match event.get_type() {
                        EventType::Sysex => {
                            if let Some(chunk) = event.get_ext() {
                                done.extend(self.reassembler.push(chunk));
                            }
                        }
                        EventType::PortExit | EventType::ClientExit
                            if !port_alive(&self.seq, self.source) =>
                        {
                            error = Some(format!(
                                "{}:{} left the sequencer mid-measurement",
                                self.source.client, self.source.port
                            ));
                            break;
                        }
                        _ => {}
                    }
                }
                Err(e) if e.errno() == libc::EAGAIN => break,
                // An event type this build of the alsa crate does not model.
                // Skipping it costs nothing; dying would end a `watch` that
                // someone is standing at the piano to run.
                Err(e) if e.errno() == libc::ENOTSUP => {}
                // The overflowed events are already gone; whatever frame was in
                // flight is abandoned when the next F0 arrives. Bounded because
                // an overrun that never clears would otherwise spin here
                // forever, printing as it goes.
                Err(e) if e.errno() == libc::ENOSPC => {
                    eprintln!("pianoctl: input buffer overrun; some frames were lost");
                    // Bytes vanished mid-stream, so whatever frame was open can
                    // only be completed by an unrelated tail — and a checksum
                    // agrees by accident once every 128 tries.
                    self.reassembler.abandon_open();
                    self.overruns += 1;
                    if self.overruns >= MAX_OVERRUNS {
                        error = Some("the sequencer input buffer keeps overrunning".to_string());
                        break;
                    }
                }
                Err(e) => {
                    error = Some(format!("reading event: {e}"));
                    break;
                }
            }
        }
        let abandoned = self.reassembler.abandoned() - abandoned_before;
        if abandoned > 0 {
            eprintln!("pianoctl: {abandoned} partial frame(s) discarded");
        }
        FrameBatch {
            frames: done,
            error,
        }
    }
}

fn subscribe(seq: &Seq, sender: Addr, dest: Addr) -> Result<(), alsa::Error> {
    let subscription = PortSubscribe::empty()?;
    subscription.set_sender(sender);
    subscription.set_dest(dest);
    seq.subscribe_port(&subscription)
}

fn port_alive(seq: &Seq, addr: Addr) -> bool {
    ClientIter::new(seq).any(|client| {
        client.get_client() == addr.client
            && PortIter::new(seq, client.get_client()).any(|port| port.get_port() == addr.port)
    })
}

/// Both sides must come from the *same* client: taken separately, a needle
/// broad enough to match two devices could have us send to one and listen to
/// the other, and the resulting silence would be filed as "this firmware does
/// not answer" — exactly the measurement this tool exists to make.
fn find_device(seq: &Seq, needle: &str, own_client: i32) -> Option<(Addr, Addr)> {
    let needle = needle.to_lowercase();
    let readable = PortCap::READ | PortCap::SUBS_READ;
    let writable = PortCap::WRITE | PortCap::SUBS_WRITE;
    for client in ClientIter::new(seq) {
        if client.get_client() == own_client {
            continue;
        }
        let client_name = client.get_name().unwrap_or("").to_lowercase();
        let mut source = None;
        let mut dest = None;
        for port in PortIter::new(seq, client.get_client()) {
            let port_name = port.get_name().unwrap_or("").to_lowercase();
            if !client_name.contains(&needle) && !port_name.contains(&needle) {
                continue;
            }
            let addr = Addr {
                client: client.get_client(),
                port: port.get_port(),
            };
            let caps = port.get_capability();
            if source.is_none() && caps.contains(readable) {
                source = Some(addr);
            }
            if dest.is_none() && caps.contains(writable) {
                dest = Some(addr);
            }
        }
        if let (Some(source), Some(dest)) = (source, dest) {
            return Some((source, dest));
        }
    }
    None
}
