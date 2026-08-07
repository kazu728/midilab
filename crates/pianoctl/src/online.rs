use std::thread;
use std::time::{Duration, Instant};

use alsa::seq::Addr;
use chrono::Local;

use crate::map::{AddressMap, Param, Value};
use crate::profile::{Decision, Reason, Step};
use crate::render;
use crate::seq::Session;
use crate::sysex::{Codec, Incoming, Request, hex, spaced_hex};

const TIMEOUT: Duration = Duration::from_millis(500);
const ATTEMPTS: usize = 2;

pub fn identity(map: &AddressMap, codec: &Codec, port: &str) -> Result<bool, String> {
    let mut session = open(port)?;
    let request = Request::identity();
    let reply = ask(&mut session, map, codec, &request, |incoming| {
        matches!(incoming, Incoming::IdentityReply(_))
    })?;
    let Some(Incoming::IdentityReply(reply)) = reply else {
        println!("no identity reply after {ATTEMPTS} requests");
        return Ok(false);
    };
    println!("{}", render::identity(&reply));
    let device = &map.device;
    if reply.family.as_slice() != device.identity_family.as_slice()
        || reply.member.as_slice() != device.identity_member.as_slice()
    {
        return Err(format!(
            "this is not a {}: the map expects family {}, member {}",
            device.name,
            spaced_hex(&device.identity_family),
            spaced_hex(&device.identity_member),
        ));
    }
    println!("matches the map's {}", device.name);
    Ok(true)
}

pub fn read(
    map: &AddressMap,
    codec: &Codec,
    port: &str,
    params: &[&Param],
) -> Result<bool, String> {
    let mut session = open(port)?;
    let mut answered = true;
    for param in params {
        let reading = read_param(&mut session, map, codec, param)?;
        println!("{:<18} {}", param.name, show(param, &reading));
        answered &= matches!(reading, Reading::Got(_));
    }
    Ok(answered)
}

pub fn watch(map: &AddressMap, codec: &Codec, port: &str) -> Result<bool, String> {
    let mut session = open(port)?;
    eprintln!("pianoctl: watching — operate the panel; Ctrl-C to stop");
    loop {
        let deadline = Instant::now() + Duration::from_secs(3600);
        let batch = session.frames(deadline)?;
        for raw in batch.frames {
            let decoded = codec.decode(&raw);
            println!(
                "{}  {}",
                Local::now().format("%H:%M:%S%.3f"),
                render::frame(map, codec, &raw, decoded)
            );
        }
        if let Some(error) = batch.error {
            return Err(error);
        }
    }
}

pub fn diff(
    map: &AddressMap,
    codec: &Codec,
    port: &str,
    steps: &[Step<'_>],
) -> Result<bool, String> {
    let mut session = open(port)?;
    let mut answered = true;
    let mut changes = 0;
    let mut unknown = 0;
    for step in steps {
        let reading = current(&mut session, map, codec, step)?;
        answered &= !matches!(reading, Reading::Silent | Reading::Undecodable(_));
        let verdict = match step.decide(reading.value()) {
            Decision::InSync => "in sync".to_string(),
            Decision::Write(reason) => {
                match reason {
                    Reason::Differs => changes += 1,
                    Reason::Unknown | Reason::NotComparable => unknown += 1,
                }
                format!("would send {}{}", hex(step.frame()), note(&reason))
            }
        };
        println!(
            "{:<18} want {:<22} current {:<34} {verdict}",
            step.param.name,
            step.param.show(&step.desired),
            current_text(step, &reading)
        );
    }
    println!(
        "{} parameter(s): {changes} differ, {unknown} could not be compared, {} in sync; \
         no write was sent (only the reads above went out)",
        steps.len(),
        steps.len() - changes - unknown,
    );
    Ok(answered && unknown == 0)
}

pub fn apply(
    map: &AddressMap,
    codec: &Codec,
    port: &str,
    steps: &[Step<'_>],
) -> Result<bool, String> {
    let mut session = open(port)?;
    let mut sent = 0;
    for (done, step) in steps.iter().enumerate() {
        let before = current(&mut session, map, codec, step)?;
        if step.decide(before.value()) == Decision::InSync {
            println!(
                "{:<18} already {}",
                step.param.name,
                step.param.show(&step.desired)
            );
            continue;
        }
        eprintln!("--> {}", hex(step.frame()));
        session.send(&Request::write(step.frame()))?;
        sent += 1;
        let after = settled(&mut session, map, codec, step)?;
        let confirmed = step.decide(after.value()) == Decision::InSync;
        println!(
            "{:<18} want {:<22} now {:<34} {}",
            step.param.name,
            step.param.show(&step.desired),
            current_text(step, &after),
            if confirmed {
                "confirmed"
            } else {
                "sent, not confirmed"
            },
        );
        if !confirmed {
            let left = steps.len() - done - 1;
            println!("stopping after an unconfirmed write; {left} step(s) not attempted");
            return Ok(false);
        }
    }
    println!(
        "{} parameter(s): {} already in sync, {sent} written and confirmed",
        steps.len(),
        steps.len() - sent,
    );
    Ok(true)
}

/// The piano acts on a DT1 before it updates the address RQ1 answers from —
/// about 90 ms on firmware 1C 01 00 00 (measured 2026-08-08 over three values:
/// 88, 94 and 90 ms). A single read straight after the write therefore returns
/// the old value and would file every successful write as a failure, so the
/// read-back is repeated until the piano agrees or the allowance is spent.
const SETTLE: Duration = Duration::from_millis(100);
const SETTLE_ATTEMPTS: usize = 10;

fn settled(
    session: &mut Session,
    map: &AddressMap,
    codec: &Codec,
    step: &Step<'_>,
) -> Result<Reading, String> {
    let mut reading = current(session, map, codec, step)?;
    for _ in 1..SETTLE_ATTEMPTS {
        if step.decide(reading.value()) == Decision::InSync {
            break;
        }
        thread::sleep(SETTLE);
        reading = current(session, map, codec, step)?;
    }
    Ok(reading)
}

fn current(
    session: &mut Session,
    map: &AddressMap,
    codec: &Codec,
    step: &Step<'_>,
) -> Result<Reading, String> {
    match step.current_from {
        Some(param) => read_param(session, map, codec, param),
        None => Ok(Reading::Unreadable),
    }
}

fn current_text(step: &Step<'_>, reading: &Reading) -> String {
    match (reading, step.current_from) {
        (Reading::Got(value), Some(from)) if from.name == step.param.name => from.show(value),
        (Reading::Got(value), Some(from)) => format!("{} via {}", from.show(value), from.name),
        (_, Some(from)) => format!("unknown ({})", show(from, reading)),
        (_, None) => "unknown (nothing readable is mapped)".to_string(),
    }
}

fn note(reason: &Reason) -> &'static str {
    match reason {
        Reason::Differs => "",
        Reason::Unknown => "  (current state unknown)",
        Reason::NotComparable => "  (no read-back name for this value)",
    }
}

enum Reading {
    Got(Value),
    Undecodable(Vec<u8>),
    Silent,
    Unreadable,
}

impl Reading {
    fn value(&self) -> Option<&Value> {
        match self {
            Reading::Got(value) => Some(value),
            _ => None,
        }
    }
}

fn show(param: &Param, reading: &Reading) -> String {
    match reading {
        Reading::Got(value) => param.show(value),
        Reading::Undecodable(data) => format!(
            "answered [{}], which the map cannot decode as {} byte(s) of {}",
            spaced_hex(data),
            param.size,
            param.encoding
        ),
        Reading::Silent => format!("no response after {ATTEMPTS} requests"),
        Reading::Unreadable => "not readable".to_string(),
    }
}

fn read_param(
    session: &mut Session,
    map: &AddressMap,
    codec: &Codec,
    param: &Param,
) -> Result<Reading, String> {
    let Some(address) = param.read_address else {
        return Ok(Reading::Unreadable);
    };
    let request = Request::read(codec, address, u32::from(param.size));
    let reply = ask(
        session,
        map,
        codec,
        &request,
        |incoming| matches!(incoming, Incoming::Dt1 { addr, .. } if *addr == address),
    )?;
    Ok(match reply {
        Some(Incoming::Dt1 { data, .. }) => match param.decode(&data) {
            Some(value) => Reading::Got(value),
            None => Reading::Undecodable(data),
        },
        _ => Reading::Silent,
    })
}

fn ask(
    session: &mut Session,
    map: &AddressMap,
    codec: &Codec,
    request: &Request,
    want: impl Fn(&Incoming) -> bool,
) -> Result<Option<Incoming>, String> {
    for _ in 0..ATTEMPTS {
        eprintln!("--> {}", hex(request.bytes()));
        session.send(request)?;
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let batch = session.frames(deadline)?;
            if batch.frames.is_empty() && batch.error.is_none() {
                break;
            }
            let mut matched = None;
            for raw in batch.frames {
                let decoded = codec.decode(&raw);
                if matched.is_none() && decoded.as_ref().is_ok_and(&want) {
                    eprintln!("<-- {}", hex(&raw));
                    matched = decoded.ok();
                } else {
                    eprintln!("{}", render::frame(map, codec, &raw, decoded));
                }
            }
            if let Some(error) = batch.error {
                return Err(error);
            }
            if matched.is_some() {
                return Ok(matched);
            }
        }
    }
    Ok(None)
}

fn open(needle: &str) -> Result<Session, String> {
    let session = Session::open(needle)?;
    eprintln!(
        "pianoctl: listening to {}, sending to {}",
        addr(session.source()),
        addr(session.dest())
    );
    Ok(session)
}

fn addr(addr: Addr) -> String {
    format!("{}:{}", addr.client, addr.port)
}
