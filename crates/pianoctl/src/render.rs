use crate::map::AddressMap;
use crate::sysex::{Address, Codec, DecodeError, IdentityReply, Incoming, hex, spaced_hex};

pub fn frame(
    map: &AddressMap,
    codec: &Codec,
    raw: &[u8],
    decoded: Result<Incoming, DecodeError>,
) -> String {
    let (kind, detail) = match decoded {
        Ok(Incoming::Dt1 {
            device_id,
            addr,
            data,
        }) => (
            "dt1",
            format!(
                "{addr}  {}{}",
                value_at(map, addr, &data),
                unexpected_device(codec, device_id)
            ),
        ),
        Ok(Incoming::Rq1 {
            device_id,
            addr,
            size,
        }) => (
            "rq1",
            format!(
                "{addr}  {} ({size} byte(s) requested){}",
                name_at(map, addr),
                unexpected_device(codec, device_id)
            ),
        ),
        Ok(Incoming::IdentityReply(reply)) => ("id ", identity(&reply)),
        Ok(Incoming::Foreign) => ("-- ", "not this device's dialect".to_string()),
        Err(e) => ("!! ", e.to_string()),
    };
    format!("{kind} {detail:<54} {}", hex(raw))
}

pub fn identity(reply: &IdentityReply) -> String {
    let maker = if reply.manufacturer == 0x41 {
        " (Roland)"
    } else {
        ""
    };
    format!(
        "manufacturer {:02X}{maker}  family {}  member {}  revision {}",
        reply.manufacturer,
        spaced_hex(&reply.family),
        spaced_hex(&reply.member),
        spaced_hex(&reply.revision),
    )
}

fn value_at(map: &AddressMap, addr: Address, data: &[u8]) -> String {
    let bytes = spaced_hex(data);
    match map.param_at(addr) {
        None => format!("unmapped  [{bytes}]"),
        Some(param) => match param.decode(data) {
            Some(value) => format!("{} = {}  [{bytes}]", param.name, param.show(&value)),
            None => format!(
                "{} = ?  [{bytes}] (the map says {} byte(s))",
                param.name, param.size
            ),
        },
    }
}

fn unexpected_device(codec: &Codec, device_id: u8) -> String {
    if device_id == codec.device_id() {
        String::new()
    } else {
        format!("  (device id {device_id:02X})")
    }
}

fn name_at(map: &AddressMap, addr: Address) -> String {
    map.param_at(addr)
        .map_or_else(|| "unmapped".to_string(), |param| param.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysex::parse_hex;

    const MAP: &str = r#"{
      "schema": "roland-sysex-map/v0",
      "device": {
        "name": "Test", "identity_family": "19 03", "identity_member": "00 00",
        "model_id": "00 00 00 28", "device_id_default": "10", "checksum": "roland7"
      },
      "params": [
        {"name": "metronome_status", "read_address": "01 00 01 0F", "size": 1, "encoding": "u7",
         "enum": {"0": "off", "1": "on"}, "source": "app_re", "source_ref": "test"}
      ]
    }"#;

    fn line(frame_hex: &str) -> String {
        let map: AddressMap = serde_json::from_str(MAP).expect("valid map JSON");
        let codec = map.codec();
        let raw = parse_hex(frame_hex).expect("hex");
        let decoded = codec.decode(&raw);
        frame(&map, &codec, &raw, decoded)
    }

    #[test]
    fn a_known_address_is_named_and_its_value_read() {
        let out = line("f0 41 10 00 00 00 28 12 01 00 01 0F 01 6E F7");
        assert!(
            out.starts_with("dt1 01 00 01 0F  metronome_status = on (1)  [01]"),
            "{out}"
        );
        assert!(out.ends_with("f0411000000028120100010f016ef7"), "{out}");
    }

    #[test]
    fn an_address_the_map_does_not_know_says_so() {
        let out = line("f0 41 10 00 00 00 28 12 01 00 07 07 01 70 F7");
        assert!(out.contains("unmapped  [01]"), "{out}");
    }

    #[test]
    fn a_reply_the_map_cannot_decode_shows_the_bytes_and_the_disagreement() {
        let out = line("f0 41 10 00 00 00 28 12 01 00 01 0F 00 01 6E F7");
        assert!(
            out.contains("metronome_status = ?  [00 01] (the map says 1 byte(s))"),
            "{out}"
        );
    }

    #[test]
    fn an_unexpected_device_id_is_called_out() {
        let out = line("f0 41 00 00 00 00 28 12 01 00 01 0F 01 6E F7");
        assert!(out.contains("(device id 00)"), "{out}");
    }

    #[test]
    fn the_expectation_follows_the_device_that_was_addressed() {
        let map: AddressMap = serde_json::from_str(MAP).expect("valid map JSON");
        let codec = map.codec().with_device_id(0x00);
        let broadcast = parse_hex("f0 41 00 00 00 00 28 12 01 00 01 0F 01 6E F7").unwrap();
        let addressed = parse_hex("f0 41 10 00 00 00 28 12 01 00 01 0F 01 6E F7").unwrap();
        let decoded = codec.decode(&broadcast);
        assert!(!frame(&map, &codec, &broadcast, decoded).contains("device id"));
        let decoded = codec.decode(&addressed);
        assert!(frame(&map, &codec, &addressed, decoded).contains("(device id 10)"));
    }

    #[test]
    fn a_broken_frame_reports_why_instead_of_a_value() {
        let out = line("f0 41 10 00 00 00 28 12 01 00 01 0F 01 00 F7");
        assert!(out.starts_with("!!  checksum 0x00, computed 0x6e"), "{out}");
    }

    #[test]
    fn a_request_shows_what_it_asks_for() {
        let out = line("f0 41 10 00 00 00 28 11 01 00 01 0F 00 00 00 01 6E F7");
        assert!(
            out.starts_with("rq1 01 00 01 0F  metronome_status (1 byte(s) requested)"),
            "{out}"
        );
    }

    #[test]
    fn an_identity_reply_keeps_the_revision_whole() {
        let out = line("f0 7e 10 06 02 41 19 03 00 00 1c 01 00 00 f7");
        assert!(
            out.contains("manufacturer 41 (Roland)")
                && out.contains("family 19 03")
                && out.contains("revision 1C 01 00 00"),
            "{out}"
        );
    }

    #[test]
    fn another_manufacturer_is_not_dressed_up_as_this_device() {
        let out = line("f0 43 10 4c 00 00 7e 00 f7");
        assert!(out.contains("not this device's dialect"), "{out}");
    }
}
