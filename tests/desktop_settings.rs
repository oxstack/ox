//! Exercise the exact subprocess protocol used by the desktop with synthetic credentials.
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Stdio},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).unwrap();
        let p = std::env::temp_dir().join(format!("ox-desktop-settings-{}", hex::encode(nonce)));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
    fn request(&self, request: Value) -> Result<Value, String> {
        self.request_using(request, &[])
    }
    fn request_using(&self, request: Value, env: &[(&str, &str)]) -> Result<Value, String> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ox"))
            .arg("--ox-desktop-settings-v1")
            .env_clear()
            .envs(env.iter().copied())
            .env("HOME", &self.0)
            .env("CODEX_HOME", &self.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.to_string().as_bytes())
            .unwrap();
        let result = child.wait_with_output().unwrap();
        if result.status.success() {
            Ok(serde_json::from_slice(&result.stdout).unwrap())
        } else {
            Err(String::from_utf8(result.stderr).unwrap())
        }
    }
    fn read(&self) -> Value {
        self.request(json!({"action":"read"})).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn provider_models_use_route_ownership_with_custom_aliases() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.join("codeox.toml"),
        r#"[providers.aardvark]
base_url = "http://127.0.0.1:9/v1"
env_key = "OLD_KEY"
discovery = false
[[providers.aardvark.models]]
id = "old"

[providers.verified]
base_url = "http://127.0.0.1:9/v1"
env_key = "VERIFIED_KEY"
discovery = false
[[providers.verified.models]]
id = "new"
alias = "preferred-review"
"#,
    )
    .unwrap();
    let env = [("OLD_KEY", "fixture-old"), ("VERIFIED_KEY", "fixture-new")];
    assert_eq!(
        fixture
            .request_using(json!({"action":"provider_models","id":"verified"}), &env)
            .unwrap(),
        json!(["preferred-review"])
    );
    assert_eq!(
        fixture
            .request_using(json!({"action":"provider_models","id":"aardvark"}), &env)
            .unwrap(),
        json!(["aardvark/old"])
    );
}

#[test]
fn first_run_and_connected_accounts_report_only_native_status() {
    let fixture = Fixture::new();
    let fresh = fixture.read();
    assert_eq!(fresh["chatgpt_account"], Value::Null);
    assert_eq!(fresh["claude_account"], false);
    assert_eq!(fresh["api_provider"], Value::Null);
    fs::write(
        fixture.0.join("auth.json"),
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"private-test-token"}}"#,
    )
    .unwrap();
    fs::write(
        fixture.0.join(".claude.json"),
        r#"{"oauthAccount":{"accountUuid":"private-test-account"}}"#,
    )
    .unwrap();
    let connected = fixture.read();
    assert_eq!(connected["chatgpt_account"], "saved_sign_in");
    assert_eq!(connected["claude_account"], true);
    assert!(!connected.to_string().contains("private-test"));
}

#[test]
fn validated_registry_favorites_and_private_scoped_keys() {
    let fixture = Fixture::new();
    let initial = fixture.read();
    let save = |id: &str, rev: &str, text: &str| json!({"action":"save","id":id,"revision":rev,"text":text});
    let provider = r#"[providers.demo]
base_url = "https://example.org/v1"
discovery = false
env_key = "DEMO_KEY"
[[providers.demo.models]]
id = "model"
"#;
    assert!(
        fixture
            .request(save("codeox.toml", "", "broken = ["))
            .is_err()
    );
    assert!(!fixture.0.join("codeox.toml").exists());
    assert_eq!(initial["documents"][0]["revision"], "");
    fixture.request(save("codeox.toml", "", provider)).unwrap();
    assert!(fixture.request(save("codeox.toml", "", "")).is_err());
    let key = |scope: &str, value: Value| json!({"action":"key","name":"DEMO_KEY","scope":scope,"value":value});
    assert!(
        fixture
            .request(key("https://wrong.invalid", json!("private-fixture-token")))
            .is_err()
    );
    fixture
        .request(key(
            "https://example.org/v1",
            json!("private-fixture-token"),
        ))
        .unwrap();
    let saved = fixture.read();
    assert!(!saved.to_string().contains("private-fixture-token"));
    assert_eq!(saved["credentials"][0]["saved"], true);
    assert_eq!(saved["models"], json!(["demo/model"]));
    assert_eq!(
        fs::metadata(fixture.0.join("ox-api-keys.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    fixture
        .request(save("ox-favorites.json", "", r#"["demo/model"]"#))
        .unwrap();
    assert_eq!(
        fixture.request(json!({"action":"models"})).unwrap(),
        json!(["demo/model"])
    );
    fixture
        .request(key("https://example.org/v1", Value::Null))
        .unwrap();
    assert_eq!(fixture.read()["credentials"][0]["saved"], false);
}

#[test]
fn legacy_edits_preserve_native_secrets_and_unrelated_configuration() {
    let fixture = Fixture::new();
    let path = fixture.0.join("config.toml");
    let raw = r#"# Codex preferences
model_provider = "legacy"
model = "old-model"
[model_providers.legacy]
base_url = "https://example.org/v1"
model_catalog_url = "https://example.org/models?token=private-query"
env_key = "LEGACY_KEY"
http_headers = { "X-Secret" = "private-header" }
[other]
unrelated = 123
"#;
    fs::write(&path, raw).unwrap();
    let snapshot = fixture.read();
    assert_eq!(snapshot["api_provider"], "legacy");
    assert_eq!(snapshot["models"], json!(["old-model"]));
    assert!(!snapshot.to_string().contains("private-header"));
    assert!(!snapshot.to_string().contains("private-query"));
    let doc = &snapshot["documents"][2];
    let text = doc["text"]
        .as_str()
        .unwrap()
        .replace("old-model", "new-model");
    let save = json!({"action":"save","id":"config.toml","text":text,"revision":doc["revision"]});
    fixture.request(save.clone()).unwrap();
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        raw.replace("old-model", "new-model")
    );
    assert!(fixture.request(save).is_err());
}

#[test]
fn legacy_model_discovery_uses_only_a_destination_bound_saved_key() {
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    let fixture = Fixture::new();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let path = fixture.0.join("config.toml");
    let config = format!(
        "model_provider = \"local\"\n[model_providers.local]\nbase_url = {url:?}\nenv_key = \"DEMO_API_KEY\"\n"
    );
    fs::write(&path, &config).unwrap();
    assert_eq!(fixture.read()["credentials"][0]["scope"], url);
    fixture
        .request(json!({"action":"key","name":"DEMO_API_KEY","scope":url,"value":"local-test-key"}))
        .unwrap();
    let responder = std::thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        let mut headers = Vec::new();
        let mut reader = BufReader::new(socket);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                break;
            }
            headers.push(line.trim().to_lowercase());
        }
        assert!(headers.contains(&"authorization: bearer local-test-key".to_string()));
        let body = r#"{"data":[{"id":"fixture-model"}]}"#;
        write!(reader.get_mut(),"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
    });
    assert_eq!(
        fixture.request(json!({"action":"models"})).unwrap(),
        json!(["fixture-model"])
    );
    responder.join().unwrap();
    fs::write(&path, config.replace(&url, "http://127.0.0.1:1/v1")).unwrap();
    assert!(
        fixture
            .request(json!({"action":"models"}))
            .unwrap_err()
            .contains("DEMO_API_KEY")
    );
}

#[test]
fn previous_workspace_exposes_only_ox_desktop_project_and_billing_metadata() {
    let f = Fixture::new();
    let read = || f.request(json!({"action":"legacy_workspace"})).unwrap();
    assert_eq!(read(), Value::Null);
    fs::write(
        f.0.join("ox-desktop-workspace.json"),
        r#"{"project":"/project","api_model":"old/model","extra":"private"}"#,
    )
    .unwrap();
    assert_eq!(
        read(),
        json!({"project":"/project","api_model":"old/model"})
    );
}

fn row_read(f: &Fixture) -> Value {
    f.request(json!({"action":"read_rows"})).unwrap()
}
fn row_save(f: &Fixture, revision: &Value, edit: Value) -> Result<Value, String> {
    f.request(json!({"action":"edit","revision":revision,"edit":edit}))
}
fn text(s: &str) -> Value {
    json!({"type":"text","value":s})
}

#[test]
fn typed_provider_merge_retries_without_disclosing_private_or_unknown_fields() {
    let f = Fixture::new();
    let path = f.0.join("codeox.toml");
    let file = r#"# operator-owned configuration
[future]
internal = "private-unknown"
[providers.demo]
name = "before"
base_url = "https://example.org/v1"
models_url = "https://example.org/list?token=private-query"
env_key = "DEMO_KEY"
future_flag = 37
[[providers.demo.models]]
id = "old"
custom = "private-model-metadata"
"#;
    fs::write(&path, file).unwrap();
    let snapshot = row_read(&f);
    assert!(snapshot.get("documents").is_none());
    assert!(!snapshot.to_string().contains("private-"));
    assert_eq!(
        snapshot["registry"]["providers"][0]["fields"]["models_url"],
        json!({"type":"private"})
    );
    let op = json!({"section":"providers","changes":[{"row":{"original":"demo","id":"renamed","fields":{"name":text("edited")}},"models":[{"original":"old","id":"new","fields":{}}]}]});
    fs::write(&path, file.replace("future_flag = 37", "future_flag = 42")).unwrap();
    let conflict = row_save(&f, &snapshot["revisions"]["registry"], op.clone()).unwrap();
    assert_eq!(conflict["status"], "changed_on_disk");
    assert!(!fs::read_to_string(&path).unwrap().contains("edited"));
    assert_eq!(
        row_save(&f, &conflict["revision"], op).unwrap()["status"],
        "applied"
    );
    let saved = fs::read_to_string(&path).unwrap();
    for expected in [
        "future_flag = 42",
        "private-unknown",
        "private-query",
        "private-model-metadata",
        "[providers.renamed]",
        "id = \"new\"",
        "name = \"edited\"",
    ] {
        assert!(saved.contains(expected), "{expected}")
    }
    let fresh = row_read(&f);
    let bad = json!({"section":"providers","changes":[{"row":{"original":"renamed","id":"renamed","fields":{"timeout_seconds":{"type":"integer","value":2}}},"models":[]}]});
    assert!(row_save(&f, &fresh["revisions"]["registry"], bad).is_err());
    assert_eq!(saved, fs::read_to_string(&path).unwrap());
}

#[test]
fn typed_keys_search_favorites_and_legacy_follow_independent_revisions() {
    let f = Fixture::new();
    fs::write(
        f.0.join("config.toml"),
        r#"
model_provider="demo"
model="old"
[model_providers.demo]
base_url="https://example.org/v1"
env_key="DEMO_KEY"
http_headers={ "X-Private"="private-header" }
"#,
    )
    .unwrap();
    let r = row_read(&f);
    assert!(!r.to_string().contains("private-header"));
    let op = json!({"section":"legacy","id":"demo","fields":{"model":text("new")}});
    assert_eq!(
        row_save(&f, &r["revisions"]["legacy"], op.clone()).unwrap()["status"],
        "applied"
    );
    assert_eq!(
        row_save(&f, &r["revisions"]["legacy"], op).unwrap()["status"],
        "changed_on_disk"
    );
    assert!(
        fs::read_to_string(f.0.join("config.toml"))
            .unwrap()
            .contains("private-header")
    );
    let key = |rev: &Value, value: Option<&str>| json!({"action":"edit_key","name":"DEMO_KEY","scope":"https://example.org/v1","value":value,"revision":rev});
    f.request(key(&r["revisions"]["keys"], Some("private-api-key")))
        .unwrap();
    let blocked = f.request(key(&r["revisions"]["keys"], None)).unwrap();
    assert_eq!(blocked["status"], "changed_on_disk");
    let s = row_read(&f);
    assert!(!s.to_string().contains("private-api-key"));
    assert_eq!(s["credentials"][0]["saved"], true);
    assert_eq!(
        f.request(key(&blocked["revision"], None)).unwrap()["status"],
        "applied"
    );
    let provider = json!({"section":"providers","changes":[{"row":{"original":null,"id":"demo","fields":{"base_url":text("https://example.org/v1")}},"models":[]}]});
    assert_eq!(
        row_save(&f, &s["revisions"]["registry"], provider).unwrap()["status"],
        "applied"
    );
    let r = row_read(&f);
    let search = json!({"section":"search","fields":{"enabled":{"type":"bool","value":true},"max_results":{"type":"integer","value":4}}});
    assert_eq!(
        row_save(&f, &r["revisions"]["registry"], search).unwrap()["status"],
        "applied"
    );
    assert_eq!(
        row_read(&f)["registry"]["search"]["max_results"]["value"],
        4
    );
    let fav = json!({"section":"favorites","models":["demo/new","demo/next"]});
    assert_eq!(
        row_save(&f, &s["revisions"]["favorites"], fav).unwrap()["status"],
        "applied"
    );
    assert_eq!(row_read(&f)["favorites"], json!(["demo/new", "demo/next"]));
}

#[test]
fn test_saved_provider_lists_only_its_real_endpoint_once() {
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    let f = Fixture::new();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    fs::write(f.0.join("codeox.toml"),format!("[providers.demo]\nbase_url={url:?}\nenv_key=\"TEST_KEY\"\ndiscovery=false\n[providers.other]\nbase_url=\"http://127.0.0.1:1/v1\"\nenv_key=\"OTHER_KEY\"\n")).unwrap();
    let s = row_read(&f);
    f.request(json!({"action":"edit_key","revision":s["revisions"]["keys"],"name":"TEST_KEY","scope":url,"value":"fixture-key"})).unwrap();
    let responder = std::thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        let mut r = BufReader::new(socket);
        let mut first = String::new();
        r.read_line(&mut first).unwrap();
        assert!(first.starts_with("GET /v1/models "));
        let mut headers = Vec::new();
        for line in (&mut r).lines() {
            let line = line.unwrap();
            if line.is_empty() {
                break;
            }
            headers.push(line.to_lowercase())
        }
        assert!(headers.contains(&"authorization: bearer fixture-key".into()));
        let body = r#"{"data":[{"id":"first"},{"id":"second"}]}"#;
        write!(r.get_mut(),"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
    });
    assert_eq!(
        f.request(
            json!({"action":"test_provider","id":"demo","revision":s["revisions"]["registry"]})
        )
        .unwrap(),
        2
    );
    responder.join().unwrap();
}

#[test]
fn advanced_registry_edits_merge_across_conflicts_and_keep_private_values_in_the_helper() {
    let f = Fixture::new();
    let path = f.0.join("codeox.toml");
    let original = r#"# private-comment
[future]
private_extra = "private-extra"
[providers.demo]
name = "Before"
base_url = "https://example.org/v1"
models_url = "https://example.org/list?token=private-query"
env_key="DEMO_KEY"
[[providers.demo.models]]
id="old"
reserved="private-model-extra"
"#;
    fs::write(&path, original).unwrap();
    let docs = f.request(json!({"action":"read_advanced"})).unwrap();
    assert!(!docs.to_string().contains("private-"));
    let doc = &docs[0];
    let base = doc["text"].as_str().unwrap();
    assert!(base.contains("<existing private value>"));
    let request = |rev: &Value| json!({"action":"save_advanced","id":doc["id"],"revision":rev,"original":base,"text":base.replace("Before","After")});
    fs::write(&path, original.replace("DEMO_KEY", "UPDATED_KEY")).unwrap();
    let conflict = f.request(request(&doc["revision"])).unwrap();
    assert_eq!(conflict["status"], "changed_on_disk");
    assert_eq!(
        f.request(request(&conflict["revision"])).unwrap()["status"],
        "applied"
    );
    let disk = fs::read_to_string(&path).unwrap();
    for kept in [
        "private-comment",
        "private-extra",
        "private-query",
        "private-model-extra",
        "UPDATED_KEY",
        "After",
    ] {
        assert!(disk.contains(kept), "{kept}")
    }
    assert!(!disk.contains("<existing private value>"));
    let fresh = f.request(json!({"action":"read_advanced"})).unwrap();
    let bad = json!({"action":"save_advanced","id":doc["id"],"revision":fresh[0]["revision"],"original":fresh[0]["text"],"text":"model = ["});
    assert!(f.request(bad).is_err());
    let base = fresh[0]["text"].as_str().unwrap();
    for edited in [
        String::new(),
        base.lines()
            .filter(|line| !line.contains("models_url"))
            .collect::<Vec<_>>()
            .join("\n"),
        base.split("[[providers.demo.models]]")
            .next()
            .unwrap()
            .to_owned(),
    ] {
        let refused = json!({"action":"save_advanced","id":doc["id"],"revision":fresh[0]["revision"],"original":base,"text":edited});
        assert!(f.request(refused).is_err());
    }
    assert_eq!(disk, fs::read_to_string(path).unwrap());
}

#[test]
fn starred_discovered_models_return_in_route_order_only_for_configured_providers() {
    let f = Fixture::new();
    fs::write(
        f.0.join("codeox.toml"),
        "[providers.demo]\nbase_url=\"https://example.org/v1\"\nenv_key=\"DEMO_KEY\"\n",
    )
    .unwrap();
    fs::write(
        f.0.join("ox-favorites.json"),
        r#"["demo/second","other/unavailable","demo/first"]"#,
    )
    .unwrap();
    assert_eq!(row_read(&f)["models"], json!([]));
    let s = row_read(&f);
    f.request(json!({"action":"edit_key","revision":s["revisions"]["keys"],"name":"DEMO_KEY","scope":"https://example.org/v1","value":"dummy-key"})).unwrap();
    assert_eq!(row_read(&f)["models"], json!(["demo/second", "demo/first"]));
}

fn advanced_doc(f: &Fixture, id: &str) -> Value {
    let docs = f.request(json!({"action":"read_advanced"})).unwrap();
    docs.as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == id)
        .unwrap()
        .clone()
}

fn retry_advanced(f: &Fixture, doc: &Value, edited: &str) -> Result<Value, String> {
    let mut request = json!({"action":"save_advanced","id":doc["id"],"revision":doc["revision"],"original":doc["text"],"text":edited});
    let conflict = f.request(request.clone()).unwrap();
    assert_eq!(conflict["status"], "changed_on_disk");
    request["revision"] = conflict["revision"].clone();
    f.request(request)
}

#[test]
fn advanced_empty_registry_retry_keeps_a_concurrently_added_provider() {
    let f = Fixture::new();
    let file = f.0.join("codeox.toml");
    let a = "[providers.a]\nbase_url='https://a.example/v1'\nenv_key='A_KEY'\n";
    let b = "[providers.b]\nbase_url='https://b.example/v1'\nenv_key='B_KEY'\n";
    fs::write(&file, a).unwrap();
    let draft = advanced_doc(&f, "codeox.toml");
    fs::write(&file, format!("{a}{b}")).unwrap();
    assert_eq!(retry_advanced(&f, &draft, "").unwrap()["status"], "applied");
    let disk = fs::read_to_string(&file).unwrap();
    assert!(!disk.contains("providers.a"));
    assert!(disk.contains("providers.b") && disk.contains("B_KEY"));
    let doc = advanced_doc(&f, "codeox.toml");
    assert_eq!(f.request(json!({"action":"save_advanced","id":doc["id"],"revision":doc["revision"],"original":doc["text"],"text":""})).unwrap()["status"],"applied");
    assert!(!file.exists());
}

#[test]
fn advanced_legacy_retry_keeps_external_fields_and_refuses_a_different_provider() {
    let f = Fixture::new();
    let file = f.0.join("config.toml");
    let base = r#"model = "old"
model_provider = "relay"
[model_providers.relay]
name = "Original"
base_url = "https://example.org/v1"
env_key = "OLD_KEY"
wire_api = "responses"
http_headers = { X = "private-fixture" }
"#;
    fs::write(&file, base).unwrap();
    let doc = advanced_doc(&f, "config.toml");
    let edited = doc["text"].as_str().unwrap().replace("Original", "Renamed");
    assert!(!edited.contains("private-fixture"));
    fs::write(
        &file,
        base.replace("OLD_KEY", "NEW_KEY")
            .replace("\"old\"", "\"external-model\""),
    )
    .unwrap();
    assert_eq!(
        retry_advanced(&f, &doc, &edited).unwrap()["status"],
        "applied"
    );
    let disk = fs::read_to_string(&file).unwrap();
    for value in ["Renamed", "NEW_KEY", "external-model", "private-fixture"] {
        assert!(disk.contains(value), "{value}");
    }
    let doc = advanced_doc(&f, "config.toml");
    let switched = disk.replace("relay", "other_provider");
    fs::write(&file, &switched).unwrap();
    let edited = doc["text"]
        .as_str()
        .unwrap()
        .replace("Renamed", "wrong provider");
    assert!(
        retry_advanced(&f, &doc, &edited)
            .unwrap_err()
            .contains("selected legacy provider changed")
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), switched);
}

#[test]
fn advanced_favorites_retry_preserves_independent_additions_removals_and_reordering() {
    let f = Fixture::new();
    let file = f.0.join("ox-favorites.json");
    let before = ["p/a", "p/b", "p/remove-on-disk", "p/remove-here"];
    fs::write(&file, serde_json::to_vec(&before).unwrap()).unwrap();
    let doc = advanced_doc(&f, "ox-favorites.json");
    fs::write(&file, r#"["p/b","p/a","p/remove-here","p/disk"]"#).unwrap();
    assert_eq!(
        retry_advanced(&f, &doc, r#"["p/a","p/b","p/remove-on-disk","p/editor"]"#).unwrap()["status"],
        "applied"
    );
    let saved: Vec<String> = serde_json::from_reader(fs::File::open(&file).unwrap()).unwrap();
    assert_eq!(saved, ["p/b", "p/a", "p/disk", "p/editor"]);

    let doc = advanced_doc(&f, "ox-favorites.json");
    let changed = r#"["p/disk","p/b","p/a","p/editor"]"#;
    fs::write(&file, changed).unwrap();
    assert!(
        retry_advanced(&f, &doc, r#"["p/a","p/b","p/disk","p/editor"]"#)
            .unwrap_err()
            .contains("reordered")
    );
    assert_eq!(fs::read_to_string(file).unwrap(), changed);
}

#[test]
fn advanced_model_edit_retry_keeps_another_models_concurrent_removal() {
    let f = Fixture::new();
    let file = f.0.join("codeox.toml");
    let kept = "[providers.demo]\nbase_url='https://example.org/v1'\nenv_key='API_KEY'\n[[providers.demo.models]]\nid='edit-me'\nname='Before'\n";
    fs::write(
        &file,
        format!("{kept}[[providers.demo.models]]\nid='removed'\n"),
    )
    .unwrap();
    let doc = advanced_doc(&f, "codeox.toml");
    fs::write(&file, kept).unwrap();
    let edited = doc["text"].as_str().unwrap().replace("Before", "After");
    assert_eq!(
        retry_advanced(&f, &doc, &edited).unwrap()["status"],
        "applied"
    );
    let saved = fs::read_to_string(file).unwrap();
    assert!(saved.contains("After"));
    assert!(!saved.contains("removed"));
}

#[test]
fn claude_profile_discovery_does_not_create_session_directories() {
    let fixture = Fixture::new();
    let paths = fixture
        .request(json!({"action":"claude_profiles"}))
        .unwrap();
    assert_eq!(
        paths["subscription"],
        fixture.0.join(".claude").to_str().unwrap()
    );
    assert_eq!(
        paths["api"],
        fixture.0.join("ox-clients/claude").to_str().unwrap()
    );
    assert!(!fixture.0.join("ox-clients").exists());
    assert!(!fixture.0.join(".claude").exists());
}

#[test]
fn transfer_reviews_scoped_provider_and_brave_keys_and_commits_a_batch_once() {
    let desktop = Fixture::new();
    let server = Fixture::new();
    let provider = json!({"name":"DEMO_KEY","scope":"https://example.org/v1"});
    let brave = json!({"name":"BRAVE_SEARCH_API_KEY","scope":"https://api.search.brave.com/"});
    let registry = "[providers.demo]\nbase_url='https://example.org/v1'\nenv_key='DEMO_KEY'\n[search]\nenv_key='BRAVE_SEARCH_API_KEY'\n";
    desktop
        .request(json!({"action":"save","id":"codeox.toml","revision":"","text":registry}))
        .unwrap();
    desktop.request(json!({"action":"key","name":"DEMO_KEY","scope":provider["scope"],"value":"fixture-saved"})).unwrap();
    let source = row_read(&desktop)["revisions"]["keys"].clone();
    let dest = row_read(&server)["revisions"]["keys"].clone();
    let request =
        |selected: Value| json!({"action":"export_keys","revision":source,"selected":selected});
    let environment = [
        ("DEMO_KEY", "fixture-active"),
        ("BRAVE_SEARCH_API_KEY", "fixture-brave"),
        ("UNRELATED", "fixture-private"),
    ];
    let keys = desktop
        .request_using(request(json!([provider, brave])), &environment)
        .unwrap();
    assert_eq!(keys[0]["value"], "fixture-active");
    assert_eq!(keys[1]["value"], "fixture-brave");
    assert!(
        desktop
            .request_using(
                request(json!([{"name":"UNRELATED","scope":brave["scope"]}])),
                &environment
            )
            .is_err()
    );
    let import = |keys: Value| json!({"action":"import_keys","revision":dest,"keys":keys});
    let mut invalid = keys.clone();
    invalid[1]["name"] = json!("CODEOX_TOKEN");
    assert!(server.request(import(invalid)).is_err());
    assert!(!server.0.join("ox-api-keys.json").exists());
    let conflict = Fixture::new();
    let alternate = registry.replace("https://example.org/v1", "https://elsewhere.invalid/v1");
    conflict
        .request(json!({"action":"save","id":"codeox.toml","revision":"","text":alternate}))
        .unwrap();
    assert!(conflict.request(import(keys.clone())).is_err());
    assert!(!conflict.0.join("ox-api-keys.json").exists());
    assert_eq!(
        server.request(import(keys.clone())).unwrap()["status"],
        "applied"
    );
    assert_eq!(
        server.request(import(keys)).unwrap()["status"],
        "changed_on_disk"
    );
    let snapshot = row_read(&server);
    assert!(!snapshot.to_string().contains("fixture-"));
    assert_eq!(snapshot["credentials"].as_array().unwrap().len(), 2);
    assert_eq!(
        fs::metadata(server.0.join("ox-api-keys.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(server.request(json!({"action":"export_keys","revision":snapshot["revisions"]["keys"],"selected":[{"name":"DEMO_KEY","scope":brave["scope"]}]})).is_err());
}

#[test]
fn imported_presets_register_on_first_use_and_keep_custom_keys_scoped() {
    let host = Fixture::new();
    let initial = row_read(&host);
    assert!(initial["api_provider"].is_null());
    let keys = json!([
        {"name":"OPENCODE_API_KEY","scope":"https://opencode.ai/inference/go/openai/v1","value":"fixture-go"},
        {"name":"ALIBABA_API_KEY","scope":"https://fixture.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1","value":"fixture-cloud"},
        {"name":"OPENAI_API_KEY","scope":"http://127.0.0.1:8000/v1","value":"fixture-custom"}
    ]);
    assert_eq!(
        host.request(
            json!({"action":"import_keys", "revision":initial["revisions"]["keys"], "keys":keys})
        )
        .unwrap(),
        json!({"status":"applied"})
    );

    let after = row_read(&host);
    let credentials = after["credentials"].as_array().unwrap();
    assert_eq!(credentials.len(), 3);
    for key in credentials {
        assert_eq!(key["saved"], true);
        assert_eq!(key["configured"], key["name"] != "OPENAI_API_KEY");
    }
    assert_eq!(after["models"], json!(["alibaba/qwen3.8-flash"]));
    let path = host.0.join("codeox.toml");
    let registry = fs::read_to_string(&path).unwrap();
    assert!(registry.contains("https://opencode.ai/inference/go/v1/models"));
    assert!(!registry.contains("fixture-"));

    // Replacing a key uses the host's existing definition, including edits.
    let customized = registry.replace("OpenCode Go", "My OpenCode");
    fs::write(&path, &customized).unwrap();
    assert_eq!(
        host.request(json!({"action":"import_keys", "revision":after["revisions"]["keys"], "keys":[
            {"name":"OPENCODE_API_KEY","scope":"https://opencode.ai/inference/go/openai/v1","value":"fixture-new"}
        ]})).unwrap(),
        json!({"status":"applied"})
    );
    assert_eq!(fs::read_to_string(path).unwrap(), customized);
}

#[test]
fn importing_a_preset_does_not_replace_a_legacy_provider_or_an_occupied_id() {
    let key = json!({"name":"OPENAI_API_KEY","scope":"https://api.openai.com/v1","value":"fixture-openai"});
    let legacy = Fixture::new();
    fs::write(
        legacy.0.join("config.toml"),
        "model_provider='private'\nmodel='local-model'\n[model_providers.private]\nbase_url='https://example.org/v1'\nenv_key='PRIVATE_KEY'\nwire_api='responses'\n",
    ).unwrap();
    let occupied = Fixture::new();
    let registry = "[providers.openai]\nbase_url='https://example.org/v1'\nenv_key='PRIVATE_KEY'\n";
    fs::write(occupied.0.join("codeox.toml"), registry).unwrap();
    for host in [&legacy, &occupied] {
        let initial = row_read(host);
        host.request(
            json!({"action":"import_keys", "revision":initial["revisions"]["keys"], "keys":[key]}),
        )
        .unwrap();
        let after = row_read(host);
        let imported = after["credentials"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == "OPENAI_API_KEY")
            .unwrap();
        assert_eq!(imported["saved"], true);
        assert_eq!(imported["configured"], false);
    }
    assert!(!legacy.0.join("codeox.toml").exists());
    assert_eq!(
        fs::read_to_string(occupied.0.join("codeox.toml")).unwrap(),
        registry
    );
}
