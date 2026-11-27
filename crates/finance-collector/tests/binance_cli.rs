//! End-to-end CLI tests. The child replays a synthetic response without sockets.

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

use serde_json::{Value, json};

const PROFILES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config");

fn cli(root: &Path, helper: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_finance-binance"));
    cmd.env_clear()
        .env("FINANCE_PYTHON", helper)
        .env("FINANCE_CONFIG_DIR", PROFILES)
        .env("FERTILE_DATA_ROOT", root.join("absent-data"))
        .env("F4_TEST_CALLS", root.join("calls"));
    cmd
}

#[test]
fn bundled_profiles_exit_disabled_without_starting_transport() {
    let tmp = tempfile::tempdir().unwrap();
    for profile in ["cloudbox", "stable"] {
        let output = cli(tmp.path(), &tmp.path().join("nonexistent-python"))
            .env("FERTILE_PROFILE", profile)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stderr).unwrap(),
            json!({"state":"disabled_by_profile"})
        );
    }
    assert!(!tmp.path().join("absent-data").exists());
}

#[test]
fn cli_replays_region_restriction_through_child_pipe_and_exits_successfully() {
    let tmp = tempfile::tempdir().unwrap();
    let helper = tmp.path().join("fixture.sh");
    fs::write(
        &helper,
        concat!(
            "#!/bin/sh\n",
            "printf '%s\\n' \"$*\" >> \"$F4_TEST_CALLS\"\n",
            "printf '%s\\n' '{\"type\":\"failure\",\"status\":451,\"retry_after\":null}'\n",
        ),
    )
    .unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    let profile = tmp.path().join("enabled.yaml");
    fs::write(
        &profile,
        "enabled: true\nspot:\n  license: fixture.synthetic\n  display_live: true\n  record_local: false\n  derive_non_display: false\n  backup_personal_drive: false\n",
    )
    .unwrap();
    let output = cli(tmp.path(), &helper)
        .env("FINANCE_BINANCE_CONFIG", profile)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let statuses: Vec<Value> = std::str::from_utf8(&output.stderr)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        statuses,
        [
            json!({"state":"connecting"}),
            json!({"state":"region_restricted","http_status":451})
        ]
    );
    assert_eq!(
        fs::read_to_string(tmp.path().join("calls")).unwrap(),
        "-m fertile_finance.binance_transport --spot\n"
    );
    assert!(!tmp.path().join("absent-data").exists());
}
