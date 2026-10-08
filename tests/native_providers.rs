#[test]
fn ollama_and_custom_cli_setup_are_keyless_and_preserve_configuration() {
    let root = std::env::temp_dir().join(format!("sui-login-cli-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let config = root.join("config.toml");
    std::fs::write(&config, "[ui]\ntheme = \"terminal\"\n").unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sui"))
        .env("SUI_HOME", &root)
        .args(["auth", "ollama", "--model", "local-coder"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: toml::Value = std::fs::read_to_string(&config).unwrap().parse().unwrap();
    assert_eq!(doc["profiles"]["ollama"]["kind"].as_str(), Some("ollama"));
    assert_eq!(doc["ui"]["theme"].as_str(), Some("terminal"));
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sui"))
        .env("SUI_HOME", &root)
        .args([
            "login",
            "--provider",
            "openai-compatible",
            "--base-url",
            "http://127.0.0.1:1234/v1",
            "--model",
            "custom-model",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: toml::Value = std::fs::read_to_string(&config).unwrap().parse().unwrap();
    assert_eq!(
        doc["profiles"]["custom"]["model"].as_str(),
        Some("custom-model")
    );
    assert!(doc["profiles"]["custom"].get("api_key").is_none());
    assert!(doc["profiles"].get("ollama").is_some());
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sui"))
        .env("SUI_HOME", &root)
        .args(["auth", "ollama"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--model"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn removed_provider_setup_is_rejected_without_changing_configuration() {
    let root =
        std::env::temp_dir().join(format!("sui-removed-provider-cli-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let config = root.join("config.toml");
    let before = "[ui]\ntheme = \"terminal\"\n";
    std::fs::write(&config, before).unwrap();
    for provider in ["claude", "gemini"] {
        for command in ["auth", "login"] {
            let output = std::process::Command::new(env!("CARGO_BIN_EXE_sui"))
                .env("SUI_HOME", &root)
                .args([command, provider])
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("unknown sign-in provider"));
            assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
