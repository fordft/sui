use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn desktop_setup_and_bridge_start_without_a_terminal() {
    let root =
        std::env::temp_dir().join(format!("sui-desktop-bridge-{:016x}", rand::random::<u64>()));
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let binary = env!("CARGO_BIN_EXE_sui");
    let mut setup = Command::new(binary)
        .arg("desktop-setup")
        .env("HOME", &root)
        .env("SUI_HOME", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    setup
        .stdin
        .take()
        .unwrap()
        .write_all(
            format!(
                "{}\n",
                json!({"name":"desktop-test","kind":"ollama","base_url":"http://127.0.0.1:11434/v1","model":"fixture-model","api_key":"private-fixture-key"})
            )
            .as_bytes(),
        )
        .unwrap();
    let result = setup.wait_with_output().unwrap();
    assert!(result.status.success());
    assert!(!String::from_utf8_lossy(&result.stdout).contains("private-fixture-key"));
    let config = root.join("config.toml");
    assert!(std::fs::read_to_string(&config)
        .unwrap()
        .contains("desktop-test"));
    // Provider discovery works before a workspace/agent bridge exists.
    let mut discovery = Command::new(binary)
        .arg("desktop-provider")
        .env("HOME", &root)
        .env("SUI_HOME", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    discovery
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"action\":\"profiles\"}\n")
        .unwrap();
    let output = discovery.wait_with_output().unwrap();
    assert!(output.status.success());
    let discovered: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(discovered["type"], "profiles");
    assert!(discovered["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["name"] == "desktop-test" && p["kind"] == "ollama"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-fixture-key"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&config).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    let mut bridge = Command::new(binary)
        .args(["desktop-bridge", "--workspace"])
        .arg(&workspace)
        .args(["--profile", "desktop-test"])
        .env("HOME", &root)
        .env("SUI_HOME", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(bridge.stdout.take().unwrap()).lines();
    let hello: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["profile"], "desktop-test");
    assert_eq!(
        hello["workspace"],
        workspace.canonicalize().unwrap().to_str().unwrap()
    );
    let mut input = bridge.stdin.take().unwrap();
    input.write_all(b"{\"type\":\"list_profiles\"}\n").unwrap();
    let profiles: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(profiles["type"], "profiles");
    assert_eq!(profiles["truncated"], false);
    let profile = profiles["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "desktop-test")
        .unwrap();
    assert_eq!(
        profile,
        &json!({"name":"desktop-test","model":"fixture-model"})
    );
    assert!(!profiles.to_string().contains("private-fixture-key"));
    input.write_all(b"{\"type\":\"list_sessions\"}\n").unwrap();
    drop(input);
    let sessions: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(sessions["type"], "sessions");
    assert!(bridge.wait().unwrap().success());
    std::fs::remove_dir_all(root).unwrap();
}
