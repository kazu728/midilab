use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn shipped_map() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/fp30x-address-map.json")
}

fn pianoctl(map: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pianoctl"))
        .arg("--map")
        .arg(map)
        .args(args)
        .output()
        .expect("running pianoctl")
}

fn inconsistent_map() -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("pianoctl-inconsistent-{}.json", std::process::id()));
    let text = fs::read_to_string(shipped_map()).expect("the shipped map");
    assert_eq!(
        text.matches(r#""size": 2,"#).count(),
        1,
        "this mutation only means what it says while one entry is two bytes wide"
    );
    let broken = text.replacen(r#""size": 2,"#, r#""size": 1,"#, 1);
    fs::write(&path, broken).expect("writing the temporary map");
    path
}

const CANNOT_MEASURE: i32 = 2;
const NO_ANSWER: i32 = 1;

#[test]
fn an_inconsistent_map_stops_the_command_that_would_use_it() {
    let path = inconsistent_map();
    let out = pianoctl(&path, &["decode", "f0411000000028120100010f016ef7"]);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    fs::remove_file(&path).expect("cleaning up");

    assert_eq!(out.status.code(), Some(CANNOT_MEASURE), "{stderr}");
    assert!(stderr.contains("does not match u14"), "{stderr}");
}

#[test]
fn a_malformed_frame_fails_the_run_and_says_why() {
    let out = pianoctl(
        &shipped_map(),
        &["decode", "f0411000000028120100010f0100f7"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(NO_ANSWER), "{stdout}");
    assert!(stdout.contains("checksum"), "{stdout}");
}

#[test]
fn one_unreadable_argument_does_not_swallow_the_others() {
    let good = "f0411000000028120100010f016ef7";
    let out = pianoctl(&shipped_map(), &["decode", good, "zz", good]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(NO_ANSWER), "{stdout}");
    assert_eq!(
        stdout.matches("metronome_status = on").count(),
        2,
        "both good frames should still be decoded: {stdout}"
    );
    assert!(stdout.contains("not a hex digit"), "{stdout}");
}

#[test]
fn a_frame_from_another_device_is_understood_not_failed() {
    let out = pianoctl(&shipped_map(), &["decode", "f043104c00007e00f7"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("not this device's dialect"), "{stdout}");
}
