use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::map::{AddressMap, Encoding, Param, Value};
use crate::sysex::parse_hex;

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Written {
    Num(i64),
    Text(String),
}

#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct Profile(BTreeMap<String, Written>);

#[derive(Debug)]
pub struct Step<'m> {
    pub param: &'m Param,
    pub desired: Value,
    pub current_from: Option<&'m Param>,
    frame: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    InSync,
    Write(Reason),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reason {
    Differs,
    Unknown,
    /// The read-back partner has no name for the value we want, so agreement
    /// cannot be established either way — `on_request_next_start` has no
    /// counterpart in `metronome_status`.
    NotComparable,
}

impl Profile {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text =
            fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    pub fn plan<'m>(&self, map: &'m AddressMap) -> Result<Vec<Step<'m>>, String> {
        let codec = map.codec();
        let mut steps = Vec::new();
        let mut problems = Vec::new();
        for (name, written) in &self.0 {
            let Some(param) = map.param(name) else {
                problems.push(format!("{name}: no such parameter in the map"));
                continue;
            };
            let Some(address) = param.write_address else {
                problems.push(format!("{name}: is read-only"));
                continue;
            };
            let desired = match resolve(param, written) {
                Ok(value) => value,
                Err(e) => {
                    problems.push(format!("{name}: {e}"));
                    continue;
                }
            };
            let bytes = param
                .encode(&desired)
                .expect("accepts() rejects anything the encoding cannot carry");
            let current_from = match param.read_address {
                Some(_) => Some(param),
                None => param
                    .verify_with
                    .as_deref()
                    .and_then(|partner| map.param(partner)),
            };
            steps.push(Step {
                param,
                desired,
                current_from,
                frame: codec.dt1(address, &bytes),
            });
        }
        if problems.is_empty() {
            Ok(steps)
        } else {
            Err(problems.join("\n"))
        }
    }
}

impl Step<'_> {
    pub fn frame(&self) -> &[u8] {
        &self.frame
    }

    pub fn decide(&self, current: Option<&Value>) -> Decision {
        let (Some(current), Some(from)) = (current, self.current_from) else {
            return Decision::Write(Reason::Unknown);
        };
        if from.name == self.param.name {
            return if *current == self.desired {
                Decision::InSync
            } else {
                Decision::Write(Reason::Differs)
            };
        }
        let (Some(want), Some(now)) = (self.param.label(&self.desired), from.label(current)) else {
            return Decision::Write(Reason::NotComparable);
        };
        if !from.labels.values().any(|label| label == want) {
            return Decision::Write(Reason::NotComparable);
        }
        if want == now {
            Decision::InSync
        } else {
            Decision::Write(Reason::Differs)
        }
    }
}

fn resolve(param: &Param, written: &Written) -> Result<Value, String> {
    let value = match written {
        Written::Num(n) if *n < 0 => {
            return Err(format!("{n} is negative; values start at 0"));
        }
        Written::Num(n) => Value::Num(
            u32::try_from(*n).map_err(|_| format!("{n} is too large; maximum is {}", u32::MAX))?,
        ),
        Written::Text(text) => {
            if let Some((v, _)) = param.labels.iter().find(|(_, label)| *label == text) {
                Value::Num(*v)
            } else if param.encoding == Encoding::Raw {
                Value::Raw(parse_hex(text)?)
            } else if param.labels.is_empty() {
                return Err(format!("{text:?} is not a number"));
            } else {
                return Err(format!("{text:?} is not one of {}", param.label_list()));
            }
        }
    };
    param
        .accepts(&value)
        .map_err(|e| format!("{} {e}", param.show(&value)))?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysex::hex;

    const MAP: &str = r#"{
      "schema": "roland-sysex-map/v0",
      "device": {
        "name": "Test", "identity_family": "19 03", "identity_member": "00 00",
        "model_id": "00 00 00 28", "device_id_default": "10", "checksum": "roland7"
      },
      "params": [
        {"name": "metronome_status", "read_address": "01 00 01 0F", "size": 1, "encoding": "u7",
         "enum": {"0": "off", "1": "on"}, "source": "app_re", "source_ref": "test"},
        {"name": "metronome_switch", "write_address": "01 00 03 1A", "size": 1, "encoding": "u7",
         "enum": {"0": "off", "1": "on", "2": "on_request_next_start"},
         "verify_with": "metronome_status", "source": "app_re", "source_ref": "test"},
        {"name": "master_volume", "read_address": "01 00 02 13", "write_address": "01 00 02 13",
         "size": 1, "encoding": "u7", "range": {"min": 0, "max": 100},
         "source": "app_re", "source_ref": "test"},
        {"name": "tone_for_single", "read_address": "01 00 02 07", "write_address": "01 00 02 07",
         "size": 3, "encoding": "raw", "source": "app_re", "source_ref": "test"}
      ]
    }"#;

    fn map() -> AddressMap {
        serde_json::from_str(MAP).expect("valid map JSON")
    }

    fn plan<'m>(map: &'m AddressMap, text: &str) -> Result<Vec<Step<'m>>, String> {
        Profile::parse(text).expect("valid TOML").plan(map)
    }

    #[test]
    fn resolves_labels_numbers_and_raw_bytes() {
        let map = map();
        let steps = plan(
            &map,
            r#"
            metronome_switch = "on"
            master_volume = 80
            tone_for_single = "01 00 44"
            "#,
        )
        .expect("a resolvable profile");
        let by_name: BTreeMap<_, _> = steps.iter().map(|s| (s.param.name.as_str(), s)).collect();
        assert_eq!(by_name["metronome_switch"].desired, Value::Num(1));
        assert_eq!(by_name["master_volume"].desired, Value::Num(80));
        assert_eq!(
            by_name["tone_for_single"].desired,
            Value::Raw(vec![0x01, 0x00, 0x44])
        );
        assert_eq!(
            hex(by_name["master_volume"].frame()),
            "f04110000000281201000213501af7"
        );
    }

    #[test]
    fn every_unusable_key_is_reported_at_once() {
        let map = map();
        let e = plan(
            &map,
            r#"
            metronome_status = "on"
            master_volume = 200
            nonesuch = 1
            "#,
        )
        .expect_err("three unusable keys");
        assert!(e.contains("metronome_status: is read-only"), "{e}");
        assert!(e.contains("master_volume: 200 outside 0..=100"), "{e}");
        assert!(e.contains("nonesuch: no such parameter"), "{e}");
    }

    #[test]
    fn a_value_that_is_not_a_value_is_explained() {
        let map = map();
        let negative = plan(&map, "master_volume = -3").expect_err("no negative values");
        assert!(
            negative.contains("master_volume: -3 is negative"),
            "{negative}"
        );

        let too_large = plan(&map, "master_volume = 99999999999").expect_err("no values above u32");
        assert!(
            too_large.contains("99999999999 is too large; maximum is 4294967295"),
            "{too_large}"
        );

        let wrong_type = Profile::parse("master_volume = true").expect_err("not a value");
        assert!(wrong_type.contains("master_volume"), "{wrong_type}");
    }

    #[test]
    fn an_unknown_label_names_the_ones_that_exist() {
        let map = map();
        let e = plan(&map, r#"metronome_switch = "maybe""#).expect_err("no such label");
        assert!(e.contains("off(0), on(1), on_request_next_start(2)"), "{e}");
    }

    #[test]
    fn a_readable_parameter_is_compared_directly() {
        let map = map();
        let steps = plan(&map, "master_volume = 80").unwrap();
        let step = &steps[0];
        assert_eq!(
            step.current_from.map(|p| p.name.as_str()),
            Some("master_volume")
        );
        assert_eq!(step.decide(Some(&Value::Num(80))), Decision::InSync);
        assert_eq!(
            step.decide(Some(&Value::Num(50))),
            Decision::Write(Reason::Differs)
        );
        assert_eq!(step.decide(None), Decision::Write(Reason::Unknown));
    }

    #[test]
    fn a_write_only_parameter_is_compared_through_its_partner() {
        let map = map();
        let steps = plan(&map, r#"metronome_switch = "on""#).unwrap();
        let step = &steps[0];
        assert_eq!(
            step.current_from.map(|p| p.name.as_str()),
            Some("metronome_status")
        );
        assert_eq!(step.decide(Some(&Value::Num(1))), Decision::InSync);
        assert_eq!(
            step.decide(Some(&Value::Num(0))),
            Decision::Write(Reason::Differs)
        );
    }

    #[test]
    fn a_value_the_partner_cannot_name_is_not_called_in_sync() {
        let map = map();
        let steps = plan(&map, r#"metronome_switch = "on_request_next_start""#).unwrap();
        assert_eq!(
            steps[0].decide(Some(&Value::Num(1))),
            Decision::Write(Reason::NotComparable)
        );
    }
}
