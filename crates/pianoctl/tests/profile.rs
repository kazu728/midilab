use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn the_shipped_example_resolves_against_the_shipped_map() {
    let root = root();
    let profile: toml::Table = toml::from_str(
        &std::fs::read_to_string(root.join("profiles/practice.toml")).expect("shipped profile"),
    )
    .expect("valid profile TOML");
    let map: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("data/fp30x-address-map.json")).expect("shipped map"),
    )
    .expect("valid map JSON");
    let params = map["params"].as_array().expect("parameter array");

    assert!(
        !profile.is_empty(),
        "the example should exercise the planner"
    );
    for name in profile.keys() {
        let param = params
            .iter()
            .find(|param| param["name"] == name.as_str())
            .unwrap_or_else(|| panic!("{name}: no such parameter in the shipped map"));
        let readable = param.get("read_address").is_some()
            || param
                .get("verify_with")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|partner| {
                    params.iter().any(|candidate| {
                        candidate["name"] == partner && candidate.get("read_address").is_some()
                    })
                });
        assert!(readable, "{name}: no readable current state is mapped");
    }

    let out = Command::new(env!("CARGO_BIN_EXE_pianoctl"))
        .arg("--map")
        .arg(root.join("data/fp30x-address-map.json"))
        .arg("--port")
        .arg("pianoctl-integration-test-no-such-port")
        .arg("diff")
        .arg(root.join("profiles/practice.toml"))
        .output()
        .expect("running pianoctl");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(out.status.code(), Some(2), "{stderr}");
    for name in profile.keys() {
        assert!(
            !stderr.contains(&format!("{name}:")),
            "the profile should resolve before the expected hardware error: {stderr}"
        );
    }
}
