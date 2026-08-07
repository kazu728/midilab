//! `System:Announce` handles USB replugging; queue timestamps are anchored to
//! CLOCK_MONOTONIC so daemon latency does not enter the logged time.
//!
use std::path::Path;
use std::time::Duration;

use alsa::seq::{
    Addr, ClientIter, EventType, MidiEvent, PortCap, PortInfo, PortIter, PortSubscribe, PortType,
    Seq,
};
use chrono::{Local, SecondsFormat};
use midi_event::Event;

use crate::event::{source_of, to_message};
use crate::sink::JsonlSink;

const SYSTEM_ANNOUNCE: Addr = Addr { client: 0, port: 1 };

pub fn run(capture_dir: &Path, source_match: &str) -> Result<(), String> {
    // Duplex: capture for the events themselves, output for queue control.
    let seq =
        Seq::open(None, None, false).map_err(|e| format!("opening ALSA sequencer: {e}"))?;
    seq.set_client_name(c"midilogd")
        .map_err(|e| format!("setting client name: {e}"))?;

    let queue = seq
        .alloc_queue()
        .map_err(|e| format!("allocating timestamp queue: {e}"))?;

    let mut port = PortInfo::empty().map_err(|e| format!("allocating port info: {e}"))?;
    port.set_name(c"capture");
    port.set_capability(PortCap::WRITE | PortCap::SUBS_WRITE);
    port.set_type(PortType::MIDI_GENERIC | PortType::APPLICATION);
    // The kernel stamps every event on delivery to this port with the queue's
    // clock — before the daemon dequeues it, unaffected by write-side stalls.
    port.set_timestamping(true);
    port.set_timestamp_real(true);
    port.set_timestamp_queue(queue);
    seq.create_port(&port)
        .map_err(|e| format!("creating capture port: {e}"))?;
    let dest = port.addr();

    seq.control_queue(queue, EventType::Start, 0, None)
        .map_err(|e| format!("starting timestamp queue: {e}"))?;
    seq.drain_output()
        .map_err(|e| format!("starting timestamp queue: {e}"))?;
    // Anchor queue time to CLOCK_MONOTONIC so `t_mono_ns` stays boot-relative
    // and comparable across daemon restarts. Both clocks tick from the same
    // kernel timebase; the offset picked up here is a constant sub-ms shift.
    let base_ns = monotonic_ns()
        - duration_ns(
            seq.get_queue_status(queue)
                .map_err(|e| format!("reading queue status: {e}"))?
                .get_real_time(),
        );

    // Without the announce subscription the daemon can never (re)attach to the
    // piano, so failing here is fatal: exit and let systemd restart us.
    subscribe(&seq, SYSTEM_ANNOUNCE, dest)
        .map_err(|e| format!("subscribing to System:Announce: {e}"))?;

    let mut current = try_subscribe(&seq, source_match, dest);
    match current {
        Some(addr) => eprintln!("midilogd: subscribed to {}:{}", addr.client, addr.port),
        None => eprintln!("midilogd: waiting for a source matching {source_match:?}"),
    }

    let codec = MidiEvent::new(0).map_err(|e| format!("allocating MIDI codec: {e}"))?;
    // Every `Other` line must be self-contained hex, so no running status.
    codec.enable_running_status(false);

    let mut sink = JsonlSink::new(capture_dir);
    let mut input = seq.input();
    loop {
        let mut ev = match input.event_input() {
            Ok(ev) => ev,
            // Kernel-side input buffer overflow: the overflowed events are
            // already lost, but the subscription is intact — keep running
            // rather than losing the restart window on top.
            Err(e) if e.errno() == libc::ENOSPC => {
                eprintln!("midilogd: input buffer overrun; some events were lost");
                continue;
            }
            Err(e) => return Err(format!("reading event: {e}")),
        };
        match ev.get_type() {
            EventType::PortStart | EventType::ClientStart => {
                // Unconditional on purpose: if we are already subscribed this
                // fails (EBUSY) and changes nothing, and if a replug re-used
                // the old client:port before we processed the exit, it is the
                // only path that re-attaches us.
                if let Some(addr) = try_subscribe(&seq, source_match, dest) {
                    eprintln!("midilogd: (re)subscribed to {}:{}", addr.client, addr.port);
                    current = Some(addr);
                }
            }
            EventType::PortExit | EventType::ClientExit => {
                if let Some(addr) = current
                    && !port_alive(&seq, addr)
                {
                    eprintln!(
                        "midilogd: source {}:{} left; awaiting return",
                        addr.client, addr.port
                    );
                    current = None;
                }
            }
            _ => {
                if let Some(msg) = to_message(&codec, &mut ev) {
                    let now = Local::now();
                    let event = Event {
                        t_mono_ns: ev
                            .get_time()
                            .map_or_else(monotonic_ns, |t| base_ns + duration_ns(t)),
                        t_wall: now.to_rfc3339_opts(SecondsFormat::Nanos, true),
                        src: source_of(&ev),
                        group: 0,
                        msg,
                    };
                    sink.append(&event, now.date_naive())
                        .map_err(|e| format!("writing event: {e}"))?;
                }
            }
        }
    }
}

fn subscribe(seq: &Seq, sender: Addr, dest: Addr) -> Result<(), alsa::Error> {
    let sub = PortSubscribe::empty()?;
    sub.set_sender(sender);
    sub.set_dest(dest);
    seq.subscribe_port(&sub)
}

fn try_subscribe(seq: &Seq, needle: &str, dest: Addr) -> Option<Addr> {
    let src = find_source(seq, needle)?;
    subscribe(seq, src, dest).ok()?;
    Some(src)
}

fn find_source(seq: &Seq, needle: &str) -> Option<Addr> {
    let needle = needle.to_lowercase();
    let readable = PortCap::READ | PortCap::SUBS_READ;
    for client in ClientIter::new(seq) {
        let client_name = client.get_name().unwrap_or("").to_lowercase();
        for port in PortIter::new(seq, client.get_client()) {
            if !port.get_capability().contains(readable) {
                continue;
            }
            let port_name = port.get_name().unwrap_or("").to_lowercase();
            if client_name.contains(&needle) || port_name.contains(&needle) {
                return Some(Addr {
                    client: client.get_client(),
                    port: port.get_port(),
                });
            }
        }
    }
    None
}

fn port_alive(seq: &Seq, addr: Addr) -> bool {
    ClientIter::new(seq).any(|client| {
        client.get_client() == addr.client
            && PortIter::new(seq, client.get_client()).any(|p| p.get_port() == addr.port)
    })
}

/// CLOCK_MONOTONIC in nanoseconds. Unlike a per-run `Instant`, it survives
/// process restarts and resets only on reboot.
fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec for the duration of the call.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn duration_ns(d: Duration) -> u64 {
    d.as_nanos() as u64
}
