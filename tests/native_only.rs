//! Removed executable-agent entry points fail before native requests or writes.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "sui-native-only-{}-{:x}",
            std::process::id(),
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(path.join(".config/sui")).unwrap();
        Self(path)
    }

    fn command(&self, binary: &str) -> Command {
        let mut command = Command::new(binary);
        command.current_dir(&self.0).env("HOME", &self.0);
        command
    }

    fn headless(&self, config: Option<&Path>) -> Output {
        let mut command = self.command(env!("CARGO_BIN_EXE_sui"));
        command.args(["--base-url", "http://127.0.0.1:9/v1", "--model", "fixture"]);
        if let Some(path) = config {
            command.arg("--config").arg(path);
        }
        command.arg("hello").output().unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn failure(output: Output) -> String {
    assert!(!output.status.success());
    String::from_utf8(output.stderr).unwrap()
}

#[test]
fn existing_tool_schemas_preserve_the_v047_wire_fingerprint() {
    // Captured from the released v0.4.7 binary, before SDK removal.
    // Its transitive preserve_order feature must remain native-owned.
    let schemas = sui::tools::schemas();
    let schema = serde_json::to_string(&schemas[..10]).unwrap();
    assert_eq!(
        sui::context::sha256_hex(schema.as_bytes()),
        "8f670344f77bcd258b8aa6d8108380c2bcbd713d7b688ed276375e7ec7794e80"
    );
    assert_eq!(schemas[10]["function"]["name"], "inventory");
}

#[test]
fn retired_commands_fail_and_native_profiles_remain_in_help() {
    let fixture = Fixture::new();
    let output = fixture
        .command(env!("CARGO_BIN_EXE_sui"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!String::from_utf8(output.stdout)
        .unwrap()
        .contains("acp-bridge"));
    let error = failure(
        fixture
            .command(env!("CARGO_BIN_EXE_sui"))
            .arg("acp-bridge")
            .output()
            .unwrap(),
    );
    assert!(error.contains("have been removed"), "{error}");
    let output = fixture
        .command(env!("CARGO_BIN_EXE_sui-mission"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for role in ["control", "worker", "auditor"] {
        assert!(help.contains(&format!("--{role}-profile")));
        assert!(!help.contains(&format!("--{role}-agent")));
        let error = failure(
            fixture
                .command(env!("CARGO_BIN_EXE_sui-mission"))
                .args([format!("--{role}-agent"), "retired".into()])
                .output()
                .unwrap(),
        );
        assert!(error.contains("unexpected argument"), "{error}");
    }
    assert!(!fixture.0.join(".local/share/sui/runs").exists());
}

#[test]
fn retired_config_is_rejected_without_rewriting_or_exposing_values() {
    for location in ["explicit", "project", "global"] {
        let fixture = Fixture::new();
        let path = fixture.0.join(match location {
            "explicit" => "legacy.toml",
            "project" => "sui.toml",
            _ => ".config/sui/config.toml",
        });
        let contents = "[agents.retired]\ncommand = 'DO_NOT_EXECUTE'\nargs = ['private-sentinel-value']\napproved = true\n";
        std::fs::write(&path, contents).unwrap();
        let error = failure(fixture.headless((location == "explicit").then_some(path.as_path())));
        assert!(error.contains("remove [agents]"), "{error}");
        assert!(!error.contains("DO_NOT_EXECUTE"));
        assert!(!error.contains("private-sentinel-value"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        assert!(!fixture.0.join(".local/share/sui/runs").exists());
    }
}

#[test]
fn retired_ui_roles_are_rejected_before_falling_back_to_a_native_profile() {
    for role in [
        "solo_profile",
        "orchestrator_profile",
        "worker_profile",
        "auditor_profile",
    ] {
        let fixture = Fixture::new();
        let path = fixture.0.join(".config/sui/config.toml");
        let contents = format!("[ui]\n{role} = 'acp:retired'\n");
        std::fs::write(&path, &contents).unwrap();
        let error = failure(fixture.headless(None));
        assert!(error.contains(&format!("replace [ui].{role}")), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        assert!(!fixture.0.join(".local/share/sui/runs").exists());
    }
}

#[test]
fn retired_journals_can_be_exported_with_legacy_config_and_secret_masking() {
    let fixture = Fixture::new();
    let config = fixture.0.join(".config/sui/config.toml");
    let secret = "historical-private-credential";
    std::fs::write(&config, format!("[agents.retired]\ncommand = 'DO_NOT_EXECUTE'\n[profiles.native]\napi_key = '{secret}'\n")).unwrap();
    let run = fixture.0.join(".local/share/sui/runs/legacy-run");
    std::fs::create_dir_all(&run).unwrap();
    let events = [
        serde_json::json!({"type":"session","data":{"mode":"mission","workspace":fixture.0}}),
        serde_json::json!({"type":"acp_model","data":{"requested":"retired-model","applied":true}}),
        serde_json::json!({"type":"assistant","data":{"content":format!("Historical result {secret}")}}),
    ];
    let journal = events
        .iter()
        .map(|event| format!("{event}\n"))
        .collect::<String>();
    let source = run.join("acp-worker.jsonl");
    std::fs::write(&source, &journal).unwrap();
    let output = fixture
        .command(env!("CARGO_BIN_EXE_sui"))
        .args(["export", "--run", "legacy-run"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let export_path = String::from_utf8(output.stdout).unwrap();
    let report = std::fs::read_to_string(export_path.lines().nth(1).unwrap().trim()).unwrap();
    assert!(report.contains("retired-model"));
    assert!(report.contains("Historical result"));
    assert!(!report.contains(secret));
    assert_eq!(std::fs::read_to_string(source).unwrap(), journal);
}
