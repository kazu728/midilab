use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Deserializer};

use crate::sysex::{Address, Codec, Incoming, parse_hex_field, spaced_hex};

pub const SCHEMA: &str = "roland-sysex-map/v0";

const MAX_SIZE: u8 = 16;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddressMap {
    pub schema: String,
    #[serde(default, deserialize_with = "present")]
    pub notice: Option<String>,
    pub device: Device,
    pub params: Vec<Param>,
    #[serde(default)]
    pub examples: Vec<Example>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    pub name: String,
    #[serde(deserialize_with = "hex_bytes")]
    pub identity_family: Vec<u8>,
    #[serde(deserialize_with = "hex_bytes")]
    pub identity_member: Vec<u8>,
    #[serde(deserialize_with = "hex_bytes")]
    pub model_id: Vec<u8>,
    #[serde(deserialize_with = "hex_byte")]
    pub device_id_default: u8,
    pub checksum: ChecksumKind,
    #[serde(default)]
    pub corroborations: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumKind {
    Roland7,
}

impl ChecksumKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Roland7 => "roland7",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Param {
    pub name: String,
    #[serde(default, deserialize_with = "present")]
    pub read_address: Option<Address>,
    #[serde(default, deserialize_with = "present")]
    pub write_address: Option<Address>,
    pub size: u8,
    pub encoding: Encoding,
    #[serde(default, rename = "enum")]
    pub labels: BTreeMap<u32, String>,
    #[serde(default, deserialize_with = "present")]
    pub range: Option<Range>,
    #[serde(default, deserialize_with = "present")]
    pub verify_with: Option<String>,
    pub source: Source,
    pub source_ref: String,
    #[serde(default)]
    pub corroborations: Vec<String>,
    #[serde(default)]
    pub verified: Vec<Verification>,
    #[serde(default, deserialize_with = "present")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Range {
    pub min: u32,
    pub max: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    OfficialDoc,
    AppRe,
    PanelCapture,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OfficialDoc => "official_doc",
            Self::AppRe => "app_re",
            Self::PanelCapture => "panel_capture",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verification {
    pub firmware: String,
    pub date: String,
    pub method: Method,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    PanelCapture,
    Rq1Roundtrip,
    WriteReadback,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PanelCapture => "panel_capture",
            Self::Rq1Roundtrip => "rq1_roundtrip",
            Self::WriteReadback => "write_readback",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Example {
    pub frame: String,
    pub note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    U7,
    U14,
    Raw,
}

impl std::fmt::Display for Encoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::U7 => "u7",
            Self::U14 => "u14",
            Self::Raw => "raw",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Num(u32),
    Raw(Vec<u8>),
}

impl Encoding {
    pub fn size(self) -> Option<u8> {
        match self {
            Self::U7 => Some(1),
            Self::U14 => Some(2),
            Self::Raw => None,
        }
    }

    pub fn ceiling(self) -> Option<u32> {
        match self {
            Self::U7 => Some(127),
            Self::U14 => Some(16_383),
            Self::Raw => None,
        }
    }

    pub fn decode(self, bytes: &[u8]) -> Option<Value> {
        match (self, bytes) {
            (Self::U7, [b]) => Some(Value::Num(u32::from(*b))),
            (Self::U14, [hi, lo]) => Some(Value::Num(u32::from(*hi) * 128 + u32::from(*lo))),
            (Self::Raw, bytes) => Some(Value::Raw(bytes.to_vec())),
            _ => None,
        }
    }

    pub fn encode(self, value: &Value) -> Option<Vec<u8>> {
        match (self, value) {
            (Self::U7, Value::Num(v)) if *v <= 127 => Some(vec![*v as u8]),
            (Self::U14, Value::Num(v)) if *v <= 16_383 => {
                Some(vec![(v / 128) as u8, (v % 128) as u8])
            }
            (Self::Raw, Value::Raw(bytes)) if bytes.iter().all(|b| *b <= 0x7F) => {
                Some(bytes.clone())
            }
            _ => None,
        }
    }
}

impl Param {
    pub fn decode(&self, bytes: &[u8]) -> Option<Value> {
        (bytes.len() == usize::from(self.size))
            .then(|| self.encoding.decode(bytes))
            .flatten()
    }

    pub fn encode(&self, value: &Value) -> Option<Vec<u8>> {
        self.encoding
            .encode(value)
            .filter(|bytes| bytes.len() == usize::from(self.size))
    }

    pub fn label(&self, value: &Value) -> Option<&str> {
        match value {
            Value::Num(n) => self.labels.get(n).map(String::as_str),
            Value::Raw(_) => None,
        }
    }

    pub fn show(&self, value: &Value) -> String {
        match value {
            Value::Num(n) => match self.label(value) {
                Some(label) => format!("{label} ({n})"),
                None => n.to_string(),
            },
            Value::Raw(bytes) => spaced_hex(bytes),
        }
    }

    pub fn accepts(&self, value: &Value) -> Result<(), String> {
        if let Value::Raw(bytes) = value
            && let Some(b) = bytes.iter().find(|b| **b > 0x7F)
        {
            return Err(format!("byte {b:#04x} is not 7-bit"));
        }
        if let Value::Num(n) = value {
            if let Some(range) = self.range {
                if *n < range.min || *n > range.max {
                    return Err(format!("outside {}..={}", range.min, range.max));
                }
            } else if !self.labels.is_empty() && !self.labels.contains_key(n) {
                return Err(format!("not one of {}", self.label_list()));
            }
        }
        if self.encode(value).is_none() {
            return Err(format!(
                "does not fit {} in {} byte(s)",
                self.encoding, self.size
            ));
        }
        Ok(())
    }

    pub fn label_list(&self) -> String {
        self.labels
            .iter()
            .map(|(v, label)| format!("{label}({v})"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl AddressMap {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text =
            fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        let map: Self =
            serde_json::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))?;
        let problems = map.validate();
        if problems.is_empty() {
            Ok(map)
        } else {
            Err(format!(
                "{} is inconsistent:\n{}",
                path.display(),
                problems.join("\n")
            ))
        }
    }

    pub fn codec(&self) -> Codec {
        Codec::new(self.device.model_id.clone(), self.device.device_id_default)
    }

    pub fn param(&self, name: &str) -> Option<&Param> {
        self.params.iter().find(|p| p.name == name)
    }

    pub fn readable(&self, name: &str) -> Result<&Param, String> {
        let param = self
            .param(name)
            .ok_or_else(|| format!("no parameter named {name:?} in the map"))?;
        if param.read_address.is_none() {
            return Err(format!(
                "{name} has no read address{}",
                param
                    .verify_with
                    .as_deref()
                    .map_or(String::new(), |partner| format!("; try {partner}"))
            ));
        }
        Ok(param)
    }

    pub fn param_at(&self, addr: Address) -> Option<&Param> {
        self.params
            .iter()
            .find(|p| p.read_address == Some(addr))
            .or_else(|| self.params.iter().find(|p| p.write_address == Some(addr)))
    }

    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.schema != SCHEMA {
            problems.push(format!("schema is {:?}, expected {SCHEMA:?}", self.schema));
        }
        for (field, bytes) in [
            ("model_id", &self.device.model_id),
            ("identity_family", &self.device.identity_family),
            ("identity_member", &self.device.identity_member),
        ] {
            if bytes.is_empty() {
                problems.push(format!("device.{field} is empty"));
            }
            if let Some(b) = bytes.iter().find(|b| **b > 0x7F) {
                problems.push(format!("device.{field} byte {b:#04x} is not 7-bit"));
            }
        }
        for (field, bytes) in [
            ("identity_family", &self.device.identity_family),
            ("identity_member", &self.device.identity_member),
        ] {
            if bytes.len() != 2 {
                problems.push(format!(
                    "device.{field} must contain exactly 2 bytes, got {}",
                    bytes.len()
                ));
            }
        }
        if self.device.device_id_default > 0x7F {
            problems.push("device.device_id_default is not 7-bit".into());
        }
        if self.params.is_empty() {
            problems.push("no parameters".into());
        }

        let mut names = BTreeSet::new();
        let mut claims: BTreeMap<Address, &str> = BTreeMap::new();
        for param in &self.params {
            let mut problem = |msg: String| problems.push(format!("{}: {msg}", param.name));
            if !names.insert(param.name.as_str()) {
                problem("duplicate parameter name".into());
            }
            if !is_identifier(&param.name) {
                problem("name is not lowercase snake_case".into());
            }
            if param.read_address.is_none() && param.write_address.is_none() {
                problem("has neither a read nor a write address".into());
            }
            for addr in [param.read_address, param.write_address]
                .into_iter()
                .flatten()
            {
                match claims.insert(addr, param.name.as_str()) {
                    Some(owner) if owner != param.name => {
                        problem(format!("address {addr} is also claimed by {owner}"));
                    }
                    _ => {}
                }
            }
            if param.size == 0 || param.size > MAX_SIZE {
                problem(format!("size must be 1..={MAX_SIZE}"));
            }
            if let Some(fixed) = param.encoding.size()
                && fixed != param.size
            {
                problem(format!(
                    "size {} does not match {} ({fixed} byte(s))",
                    param.size, param.encoding
                ));
            }
            match param.encoding.ceiling() {
                None => {
                    if !param.labels.is_empty() {
                        problem("raw values cannot be labelled".into());
                    }
                    if param.range.is_some() {
                        problem("raw values cannot have a range".into());
                    }
                }
                Some(ceiling) => {
                    if let Some(v) = param.labels.keys().find(|v| **v > ceiling) {
                        problem(format!("label key {v} exceeds the encoding's {ceiling}"));
                    }
                    let mut seen = BTreeSet::new();
                    if let Some(label) = param.labels.values().find(|l| !seen.insert(l.as_str())) {
                        problem(format!("label {label:?} names more than one value"));
                    }
                    if let Some(range) = param.range {
                        if range.min > range.max {
                            problem(format!("range {}..={} is inverted", range.min, range.max));
                        }
                        if range.max > ceiling {
                            problem(format!("range max {} exceeds {ceiling}", range.max));
                        }
                        if let Some(value) = param
                            .labels
                            .keys()
                            .find(|value| **value < range.min || **value > range.max)
                        {
                            problem(format!(
                                "label key {value} is outside range {}..={}",
                                range.min, range.max
                            ));
                        }
                    }
                }
            }
            if let Some(partner) = &param.verify_with {
                if param.read_address.is_some() {
                    problem("has its own read address, so verify_with is redundant".into());
                }
                match self.param(partner) {
                    None => problem(format!("verify_with names unknown {partner:?}")),
                    Some(target) if target.read_address.is_none() => {
                        problem(format!("verify_with {partner:?} cannot be read"));
                    }
                    Some(target) if target.labels.is_empty() || param.labels.is_empty() => {
                        problem(format!(
                            "verify_with {partner:?} needs labels on both parameters"
                        ));
                    }
                    Some(_) => {}
                }
            }
            for confirmation in &param.verified {
                if !is_iso_date(&confirmation.date) {
                    problem(format!(
                        "verified on {:?}, which is not a real YYYY-MM-DD date",
                        confirmation.date
                    ));
                }
                if confirmation.firmware.trim().is_empty() {
                    problem("a verification without a firmware revision has no scope".into());
                }
            }
        }

        let codec = self.codec();
        for example in &self.examples {
            let mut problem =
                |msg: String| problems.push(format!("example {:?}: {msg}", example.note));
            let bytes = match parse_hex_field(&example.frame) {
                Ok(bytes) => bytes,
                Err(e) => {
                    problem(e);
                    continue;
                }
            };
            match codec.decode(&bytes) {
                Err(e) => problem(e.to_string()),
                Ok(Incoming::Dt1 { addr, data, .. }) => match self.param_at(addr) {
                    None => problem(format!("writes {addr}, which no parameter claims")),
                    Some(param) if data.len() != usize::from(param.size) => problem(format!(
                        "writes {} byte(s) to {}, which is {} byte(s)",
                        data.len(),
                        param.name,
                        param.size
                    )),
                    Some(_) => {}
                },
                Ok(Incoming::Rq1 { addr, size, .. }) => match self.param_at(addr) {
                    None => problem(format!("reads {addr}, which no parameter claims")),
                    Some(param) if size != u32::from(param.size) => problem(format!(
                        "reads {size} byte(s) from {}, which is {} byte(s)",
                        param.name, param.size
                    )),
                    Some(_) => {}
                },
                Ok(_) => problem("is not a DT1 or RQ1 for this device".into()),
            }
        }
        problems
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn is_iso_date(date: &str) -> bool {
    let bytes = date.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[..4].iter().any(|b| !b.is_ascii_digit())
        || bytes[5..7].iter().any(|b| !b.is_ascii_digit())
        || bytes[8..].iter().any(|b| !b.is_ascii_digit())
    {
        return false;
    }
    let parts: Vec<&str> = date.split('-').collect();
    let [year, month, day] = parts[..] else {
        return false;
    };
    if (year.len(), month.len(), day.len()) != (4, 2, 2) {
        return false;
    }
    let number = |part: &str| part.parse::<u32>().ok();
    let (Some(year), Some(month), Some(day)) = (number(year), number(month), number(day)) else {
        return false;
    };
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let last = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=last).contains(&day)
}

fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

fn hex_bytes<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let text = String::deserialize(d)?;
    parse_hex_field(&text).map_err(serde::de::Error::custom)
}

fn hex_byte<'de, D: Deserializer<'de>>(d: D) -> Result<u8, D::Error> {
    match hex_bytes(d)?[..] {
        [byte] => Ok(byte),
        ref other => Err(serde::de::Error::custom(format!(
            "expected one hex byte, got {}",
            other.len()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_json(params: &str, examples: &str) -> String {
        format!(
            r#"{{
              "schema": "roland-sysex-map/v0",
              "device": {{
                "name": "Test", "identity_family": "19 03", "identity_member": "00 00",
                "model_id": "00 00 00 28", "device_id_default": "10", "checksum": "roland7"
              }},
              "params": [{params}],
              "examples": [{examples}]
            }}"#
        )
    }

    fn param_json(fields: &str) -> String {
        format!(r#"{{"source": "app_re", "source_ref": "test", {fields}}}"#)
    }

    fn parse(params: &str, examples: &str) -> AddressMap {
        serde_json::from_str(&map_json(params, examples)).expect("valid map JSON")
    }

    fn problems(params: &str, examples: &str) -> Vec<String> {
        parse(params, examples).validate()
    }

    const STATUS: &str = r#""name": "metronome_status", "read_address": "01 00 01 0F",
                            "size": 1, "encoding": "u7", "enum": {"0": "off", "1": "on"}"#;
    const SWITCH: &str = r#""name": "metronome_switch", "write_address": "01 00 03 1A",
                            "size": 1, "encoding": "u7", "enum": {"0": "off", "1": "on"},
                            "verify_with": "metronome_status""#;

    #[test]
    fn a_consistent_map_has_no_problems() {
        let params = format!("{}, {}", param_json(STATUS), param_json(SWITCH));
        let example = r#"{"frame": "f0 41 10 00 00 00 28 11 01 00 01 0f 00 00 00 01 6e f7",
                          "note": "metronome status request"}"#;
        assert_eq!(problems(&params, example), Vec::<String>::new());
    }

    #[test]
    fn duplicate_names_are_caught() {
        let params = format!("{}, {}", param_json(STATUS), param_json(STATUS));
        assert!(
            problems(&params, "")
                .iter()
                .any(|p| p.contains("duplicate parameter name")),
            "expected a duplicate-name problem"
        );
    }

    #[test]
    fn two_parameters_cannot_claim_one_address() {
        let twin = r#""name": "metronome_state", "read_address": "01 00 01 0F",
                       "size": 1, "encoding": "u7""#;
        let params = format!("{}, {}", param_json(STATUS), param_json(twin));
        assert!(
            problems(&params, "")
                .iter()
                .any(|p| p.contains("is also claimed by")),
            "expected a same-address problem"
        );

        let writer = r#""name": "metronome_poke", "write_address": "01 00 01 0F",
                        "size": 1, "encoding": "u7""#;
        let crossed = format!("{}, {}", param_json(STATUS), param_json(writer));
        assert!(
            problems(&crossed, "")
                .iter()
                .any(|p| p.contains("is also claimed by")),
            "expected a read/write cross-claim problem"
        );
    }

    #[test]
    fn one_parameter_may_read_and_write_the_same_address() {
        let params = param_json(
            r#""name": "master_volume", "read_address": "01 00 02 13",
               "write_address": "01 00 02 13", "size": 1, "encoding": "u7""#,
        );
        assert_eq!(problems(&params, ""), Vec::<String>::new());
    }

    #[test]
    fn a_label_naming_two_values_is_caught() {
        let params = param_json(
            r#""name": "metronome_status", "read_address": "01 00 01 0F", "size": 1,
               "encoding": "u7", "enum": {"0": "off", "1": "on", "2": "on"}"#,
        );
        assert!(
            problems(&params, "")
                .iter()
                .any(|p| p.contains("names more than one value")),
            "expected a duplicate-label problem"
        );
    }

    #[test]
    fn the_rules_the_published_schema_states_are_enforced_here_too() {
        let bad_name = param_json(
            r#""name": "Tone For Single!!", "read_address": "01 00 02 07", "size": 3,
               "encoding": "raw""#,
        );
        assert!(
            problems(&bad_name, "")
                .iter()
                .any(|p| p.contains("snake_case")),
            "expected a name problem"
        );

        let huge = param_json(
            r#""name": "tone", "read_address": "01 00 02 07", "size": 200, "encoding": "raw""#,
        );
        assert!(
            problems(&huge, "")
                .iter()
                .any(|p| p.contains("size must be")),
            "expected a size problem"
        );

        let vague = param_json(&format!(
            r#"{STATUS}, "verified": [{{"firmware": "1C 01 00 00", "date": "yesterday",
               "method": "rq1_roundtrip"}}]"#
        ));
        assert!(
            problems(&vague, "")
                .iter()
                .any(|p| p.contains("not a real YYYY-MM-DD date")),
            "expected a date problem"
        );
    }

    #[test]
    fn size_must_match_the_encoding() {
        let params = param_json(
            r#""name": "tempo", "read_address": "01 00 01 08", "size": 1, "encoding": "u14""#,
        );
        assert!(
            problems(&params, "")
                .iter()
                .any(|p| p.contains("does not match")),
            "expected a size/encoding problem"
        );
    }

    #[test]
    fn an_entry_without_any_address_is_useless() {
        let params = param_json(r#""name": "nowhere", "size": 1, "encoding": "u7""#);
        assert!(
            problems(&params, "")
                .iter()
                .any(|p| p.contains("neither a read nor a write")),
            "expected a missing-address problem"
        );
    }

    #[test]
    fn a_range_beyond_the_encoding_is_caught() {
        let params = param_json(
            r#""name": "loud", "read_address": "01 00 02 13", "size": 1, "encoding": "u7",
               "range": {"min": 0, "max": 500}"#,
        );
        assert!(
            problems(&params, "")
                .iter()
                .any(|p| p.contains("range max")),
            "expected a range problem"
        );
    }

    #[test]
    fn a_label_outside_its_range_is_caught() {
        let params = param_json(
            r#""name": "loud", "read_address": "01 00 02 13", "size": 1, "encoding": "u7",
               "enum": {"0": "silent", "101": "too_loud"}, "range": {"min": 0, "max": 100}"#,
        );
        let found = problems(&params, "");
        assert!(
            found.iter().any(|p| p.contains("outside range")),
            "{found:?}"
        );
    }

    #[test]
    fn identity_family_and_member_are_two_bytes() {
        for (from, to) in [
            (
                "\"identity_family\": \"19 03\"",
                "\"identity_family\": \"19\"",
            ),
            (
                "\"identity_member\": \"00 00\"",
                "\"identity_member\": \"00 00 00\"",
            ),
        ] {
            let json = map_json(&param_json(STATUS), "").replacen(from, to, 1);
            let map: AddressMap = serde_json::from_str(&json).expect("valid JSON");
            let found = map.validate();
            assert!(
                found.iter().any(|p| p.contains("exactly 2 bytes")),
                "{found:?}"
            );
        }
    }

    #[test]
    fn explicit_null_is_not_treated_as_an_omitted_optional_field() {
        let status = param_json(STATUS);
        let switch = param_json(SWITCH);
        let base = map_json(&status, "");
        let cases = [
            base.replacen(
                "\"schema\": \"roland-sysex-map/v0\",",
                "\"schema\": \"roland-sysex-map/v0\", \"notice\": null,",
                1,
            ),
            base.replacen(
                "\"read_address\": \"01 00 01 0F\"",
                "\"read_address\": null",
                1,
            ),
            map_json(&switch, "").replacen(
                "\"write_address\": \"01 00 03 1A\"",
                "\"write_address\": null",
                1,
            ),
            base.replacen("\"size\": 1,", "\"range\": null, \"size\": 1,", 1),
            map_json(&switch, "").replacen(
                "\"verify_with\": \"metronome_status\"",
                "\"verify_with\": null",
                1,
            ),
            base.replacen("\"size\": 1,", "\"notes\": null, \"size\": 1,", 1),
        ];

        for json in cases {
            assert!(
                serde_json::from_str::<AddressMap>(&json).is_err(),
                "explicit null was accepted: {json}"
            );
        }
    }

    #[test]
    fn dates_require_ascii_digits_in_every_numeric_field() {
        assert!(is_iso_date("2024-02-29"));
        assert!(!is_iso_date("2026-+1-01"));
        assert!(!is_iso_date("2026-01-+1"));
        assert!(!is_iso_date("２０２６-01-01"));
    }

    #[test]
    fn a_read_back_partner_must_exist_and_be_readable() {
        let missing = param_json(
            r#""name": "metronome_switch", "write_address": "01 00 03 1A", "size": 1,
               "encoding": "u7", "enum": {"1": "on"}, "verify_with": "nope""#,
        );
        assert!(
            problems(&missing, "")
                .iter()
                .any(|p| p.contains("unknown \"nope\"")),
            "expected a missing-partner problem"
        );

        let write_only_partner = format!(
            "{}, {}",
            param_json(SWITCH),
            param_json(
                r#""name": "metronome_status", "write_address": "01 00 01 0F", "size": 1,
                   "encoding": "u7", "enum": {"0": "off", "1": "on"}"#
            )
        );
        assert!(
            problems(&write_only_partner, "")
                .iter()
                .any(|p| p.contains("cannot be read")),
            "expected an unreadable-partner problem"
        );

        let unnamed_partner = format!(
            "{}, {}",
            param_json(SWITCH),
            param_json(
                r#""name": "metronome_status", "read_address": "01 00 01 0F", "size": 1,
                   "encoding": "u7""#
            )
        );
        assert!(
            problems(&unnamed_partner, "")
                .iter()
                .any(|p| p.contains("needs labels on both")),
            "expected a missing-labels problem"
        );

        let redundant = param_json(
            r#""name": "master_volume", "read_address": "01 00 02 13",
               "write_address": "01 00 02 13", "size": 1, "encoding": "u7",
               "enum": {"0": "silent"}, "verify_with": "master_volume""#,
        );
        assert!(
            problems(&redundant, "")
                .iter()
                .any(|p| p.contains("redundant")),
            "expected a redundant-verify_with problem"
        );
    }

    #[test]
    fn a_date_that_could_not_happen_is_not_a_date() {
        let params = param_json(&format!(
            r#"{STATUS}, "verified": [{{"firmware": "1C 01 00 00", "date": "2026-13-45",
               "method": "rq1_roundtrip"}}]"#
        ));
        assert!(
            problems(&params, "")
                .iter()
                .any(|p| p.contains("not a real YYYY-MM-DD date")),
            "expected a date problem"
        );
    }

    #[test]
    fn the_names_read_from_data_are_the_names_printed_back() {
        for name in ["official_doc", "app_re", "panel_capture"] {
            let source: Source = serde_json::from_str(&format!("\"{name}\"")).unwrap();
            assert_eq!(source.as_str(), name);
        }
        for name in ["panel_capture", "rq1_roundtrip", "write_readback"] {
            let method: Method = serde_json::from_str(&format!("\"{name}\"")).unwrap();
            assert_eq!(method.as_str(), name);
        }
        for name in ["u7", "u14", "raw"] {
            let encoding: Encoding = serde_json::from_str(&format!("\"{name}\"")).unwrap();
            assert_eq!(encoding.to_string(), name);
        }
        let checksum: ChecksumKind = serde_json::from_str("\"roland7\"").unwrap();
        assert_eq!(checksum.as_str(), "roland7");
    }

    #[test]
    fn examples_are_re_checksummed_not_trusted() {
        let example = r#"{"frame": "f0 41 10 00 00 00 28 11 01 00 01 0f 00 00 00 01 60 f7",
                          "note": "bad checksum"}"#;
        let found = problems(&param_json(STATUS), example);
        assert!(found.iter().any(|p| p.contains("checksum")), "{found:?}");
    }

    #[test]
    fn examples_must_address_a_known_parameter() {
        let example = r#"{"frame": "f0 41 10 00 00 00 28 12 01 00 07 07 01 70 f7",
                          "note": "unmapped address"}"#;
        let found = problems(&param_json(STATUS), example);
        assert!(
            found.iter().any(|p| p.contains("no parameter claims")),
            "{found:?}"
        );
    }

    #[test]
    fn values_round_trip_through_their_encoding() {
        assert_eq!(Encoding::U7.decode(&[100]), Some(Value::Num(100)));
        assert_eq!(Encoding::U14.decode(&[0, 96]), Some(Value::Num(96)));
        assert_eq!(Encoding::U14.decode(&[3, 116]), Some(Value::Num(500)));
        assert_eq!(Encoding::U14.encode(&Value::Num(500)), Some(vec![3, 116]));
        assert_eq!(Encoding::U7.encode(&Value::Num(128)), None);
        assert_eq!(Encoding::U7.decode(&[1, 2]), None);
    }

    #[test]
    fn labelled_parameters_accept_only_their_labels() {
        let map = parse(&param_json(STATUS), "");
        let status = map.param("metronome_status").unwrap();
        assert!(status.accepts(&Value::Num(1)).is_ok());
        assert!(status.accepts(&Value::Num(2)).is_err());
        assert_eq!(status.show(&Value::Num(1)), "on (1)");
        assert_eq!(status.show(&Value::Num(9)), "9");
    }

    #[test]
    fn a_ranged_parameter_accepts_its_whole_range() {
        let params = param_json(
            r#""name": "master_volume", "read_address": "01 00 02 13", "write_address": "01 00 02 13",
               "size": 1, "encoding": "u7", "range": {"min": 0, "max": 100}"#,
        );
        let map = parse(&params, "");
        let volume = map.param("master_volume").unwrap();
        assert!(volume.accepts(&Value::Num(100)).is_ok());
        assert!(volume.accepts(&Value::Num(101)).is_err());
    }
}
