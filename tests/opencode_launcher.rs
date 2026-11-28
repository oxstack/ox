#![cfg(unix)]

#[path = "common/executable.rs"]
mod executable;

use std::fs;
use std::process::Command;

use serde_json::Value;

#[test]
fn opencode_launches_with_an_isolated_profile_and_only_the_loopback_credential() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("ox");
    let bin = temp.path().join("bin");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&bin).unwrap();
    fs::write(
        home.join("codeox.toml"),
        r#"
[providers.fixture]
base_url = "http://127.0.0.1:1/v1"
wire_api = "anthropic_messages"
env_key = "FIXTURE_VENDOR_KEY"
discovery = false

[[providers.fixture.models]]
id = "tenant/test-model"
name = "Test Model"
context_window = 32000
input_modalities = ["text", "image"]

[providers.missing]
base_url = "http://127.0.0.1:1/v1"
env_key = "FIXTURE_MISSING_KEY"
discovery = false

[[providers.missing.models]]
id = "unavailable"
"#,
    )
    .unwrap();
    executable::write(
        &bin.join("opencode"),
        r#"#!/bin/sh
set -eu
test "$#" = 3
test "$1" = "run"
test "$2" = "--"
test "$3" = "prompt with spaces"
test -z "${FIXTURE_VENDOR_KEY+x}"
test -z "${OX_GATEWAY_KEY+x}"
test -z "${OPENCODE_DB+x}"
test -z "${OPENCODE_CONFIG+x}"
test -z "${OPENCODE_CONFIG_CONTENT+x}"
test "${#CODEOX_TOKEN}" = 64
test "$XDG_CONFIG_HOME" = "$CODEX_HOME/ox-clients/opencode/config"
test "$XDG_DATA_HOME" = "$CODEX_HOME/ox-clients/opencode/data"
test "$XDG_CACHE_HOME" = "$CODEX_HOME/ox-clients/opencode/cache"
test "$XDG_STATE_HOME" = "$CODEX_HOME/ox-clients/opencode/state"
/bin/cat "$OPENCODE_CONFIG_DIR/opencode.json"
exit 17
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_ox"))
        .args(["opencode", "run", "--", "prompt with spaces"])
        .env_clear()
        .env("PATH", &bin)
        .env("CODEX_HOME", &home)
        .env("FIXTURE_VENDOR_KEY", "upstream-only-credential")
        .env("OX_GATEWAY_KEY", "persistent-credential")
        .env("OPENCODE_CONFIG_DIR", "native-config")
        .env("OPENCODE_CONFIG", "native-config.json")
        .env("OPENCODE_CONFIG_CONTENT", "native-inline-config")
        .env("OPENCODE_DB", "native.db")
        .env("XDG_DATA_HOME", "native-data")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(17), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let managed: Value = serde_json::from_str(&stdout).unwrap();
    let provider = &managed["provider"]["ox"];
    assert_eq!(managed["enabled_providers"], serde_json::json!(["ox"]));
    assert_eq!(managed["disabled_providers"], serde_json::json!([]));
    assert_eq!(managed["model"], "ox/fixture/tenant/test-model");
    assert_eq!(provider["options"]["apiKey"], "{env:CODEOX_TOKEN}");
    let url = reqwest::Url::parse(provider["options"]["baseURL"].as_str().unwrap()).unwrap();
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert_eq!(url.path(), "/v1");
    assert_eq!(provider["env"], serde_json::json!(["CODEOX_TOKEN"]));
    assert_eq!(
        provider["whitelist"],
        serde_json::json!(["fixture/tenant/test-model"])
    );
    let models = provider["models"].as_object().unwrap();
    assert_eq!(models.len(), 1);
    let model = &models["fixture/tenant/test-model"];
    assert_eq!(model["id"], "fixture/tenant/test-model");
    assert_eq!(model["name"], "Test Model · fixture");
    assert_eq!(model["tool_call"], true);
    assert_eq!(model["attachment"], true);
    assert_eq!(
        model["modalities"]["input"],
        serde_json::json!(["text", "image"])
    );
    assert_eq!(model["limit"]["context"], 32000);
    assert_eq!(model["limit"]["output"], 8192);
    assert_eq!(
        fs::read_dir(home.join("ox-clients/opencode/config"))
            .unwrap()
            .count(),
        1,
        "the temporary model file should be removed after the client exits"
    );

    let list = Command::new(env!("CARGO_BIN_EXE_ox"))
        .args(["opencode", "models"])
        .env_clear()
        .env("PATH", temp.path())
        .env("CODEX_HOME", &home)
        .env("FIXTURE_VENDOR_KEY", "upstream-only-credential")
        .output()
        .unwrap();
    assert!(list.status.success(), "{list:?}");
    assert_eq!(
        String::from_utf8(list.stdout).unwrap(),
        "ox/fixture/tenant/test-model\n"
    );
}

#[test]
fn opencode_help_and_setup_hint_work_before_provider_configuration() {
    let home = tempfile::tempdir().unwrap();
    let ox = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ox"));
        command.env_clear().env("CODEX_HOME", home.path());
        command
    };
    let help = ox().args(["opencode", "--help"]).output().unwrap();
    assert!(help.status.success());
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("ox opencode models")
    );
    let missing = ox().arg("opencode").output().unwrap();
    assert!(!missing.status.success());
    assert!(
        String::from_utf8(missing.stderr)
            .unwrap()
            .contains("ox setup")
    );
    assert!(!home.path().join("ox-clients").exists());
}
