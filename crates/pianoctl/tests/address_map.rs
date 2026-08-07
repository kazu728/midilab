use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../data")
        .join(name)
}

fn json(name: &str) -> serde_json::Value {
    let text = fs::read_to_string(data(name)).expect("the data file is part of the repository");
    serde_json::from_str(&text).expect("valid JSON")
}

#[test]
fn check_map_passes_on_the_shipped_map() {
    let out = Command::new(env!("CARGO_BIN_EXE_pianoctl"))
        .arg("--map")
        .arg(data("fp30x-address-map.json"))
        .arg("check-map")
        .output()
        .expect("running pianoctl");
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn the_shipped_map_satisfies_the_published_schema() {
    let schema = json("fp30x-address-map.schema.json");
    let map = json("fp30x-address-map.json");
    let validator = jsonschema::validator_for(&schema).expect("the schema itself is valid");
    let problems: Vec<String> = validator
        .iter_errors(&map)
        .map(|e| format!("{}: {e}", e.instance_path))
        .collect();
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn the_rules_stated_twice_are_enforced_twice() {
    let schema = json("fp30x-address-map.schema.json");
    let validator = jsonschema::validator_for(&schema).expect("the schema itself is valid");
    let shipped = fs::read_to_string(data("fp30x-address-map.json")).expect("the shipped map");
    let notes_field = {
        const KEY: &str = "\"notes\": \"";
        let start = shipped.find(KEY).expect("the map has a notes field");
        let value = start + KEY.len();
        let end = value + shipped[value..].find('"').expect("the value is closed");
        shipped[start..=end].to_string()
    };

    let broken = [
        (
            "a name that is not an identifier",
            "\"name\": \"metronome_beat\"",
            "\"name\": \"Metronome Beat!!\"",
        ),
        (
            "a size beyond the declared maximum",
            "\"size\": 3,",
            "\"size\": 200,",
        ),
        (
            "a verification without a real date",
            "\"verified\": [],",
            "\"verified\": [{\"firmware\": \"1C 01 00 00\", \"date\": \"yesterday\", \"method\": \"rq1_roundtrip\"}],",
        ),
        (
            "a date with a signed month",
            "\"verified\": [],",
            "\"verified\": [{\"firmware\": \"1C 01 00 00\", \"date\": \"2026-+1-01\", \"method\": \"rq1_roundtrip\"}],",
        ),
        (
            "an address byte that is not 7-bit",
            "\"01 00 01 0F\"",
            "\"01 00 01 8F\"",
        ),
        (
            "an example frame that never ends",
            "\"F0 41 10 00 00 00 28 11 01 00 01 0F 00 00 00 01 6E F7\"",
            "\"F0 41 10 00 00\"",
        ),
        (
            "an address with a space inside a byte",
            "\"read_address\": \"01 00 01 0F\"",
            "\"read_address\": \"0 100 010 F\"",
        ),
        (
            "an address with tabs",
            "\"read_address\": \"01 00 01 0F\"",
            "\"read_address\": \"01\\t\\t00 01 0F\"",
        ),
        (
            "a device id with a space inside it",
            "\"device_id_default\": \"10\"",
            "\"device_id_default\": \"1 0\"",
        ),
        (
            "a one-byte identity family",
            "\"identity_family\": \"19 03\"",
            "\"identity_family\": \"19\"",
        ),
        (
            "an explicitly null optional field",
            notes_field.as_str(),
            "\"notes\": null",
        ),
    ];

    for (what, from, to) in broken {
        let text = shipped.replacen(from, to, 1);
        assert_ne!(text, shipped, "{what}: the map no longer contains {from}");
        let path = std::env::temp_dir().join(format!(
            "pianoctl-broken-{}-{}.json",
            std::process::id(),
            what.replace(' ', "-")
        ));
        fs::write(&path, &text).expect("writing the broken map");
        let out = Command::new(env!("CARGO_BIN_EXE_pianoctl"))
            .arg("--map")
            .arg(&path)
            .arg("check-map")
            .output()
            .expect("running pianoctl");
        fs::remove_file(&path).expect("cleaning up");

        let instance: serde_json::Value = serde_json::from_str(&text).expect("still JSON");
        assert!(
            !validator.is_valid(&instance),
            "{what}: the published schema accepted it"
        );
        assert!(
            !out.status.success(),
            "{what}: pianoctl accepted it\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
