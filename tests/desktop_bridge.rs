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

mod common;

#[test]
fn yolo_dropdown_releases_pending_commands_and_keeps_surface_consent_independent() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    let requests = AtomicUsize::new(0);
    let port = common::serve(move |_, _| {
        let request = requests.fetch_add(1, Ordering::SeqCst) + 1;
        let tool = match request {
            1 => Some((
                "bash",
                json!({"command":"pwd && ls -la && printf first > first.txt"}),
            )),
            2 => Some((
                "bash",
                json!({"command":"python3 - <<'PY'\nfrom pathlib import Path\nPath('first.txt').unlink()\nPath('script.txt').write_text('ran')\nPY"}),
            )),
            3 => Some(("web_fetch", json!({"url":"http://127.0.0.1/never-fetch"}))),
            4 => Some(("write_file", json!({"path":"auto.txt","content":"written"}))),
            6 => Some(("bash", json!({"command":"rm script.txt"}))),
            8 => Some((
                "write_file",
                json!({"path":"denied.txt","content":"must not write"}),
            )),
            _ => None,
        };
        let event = match tool {
            Some((name, args)) => {
                json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":format!("call-{request}"),"name":name,"arguments":args.to_string()}})
            }
            None => json!({"type":"response.output_text.delta","delta":"done"}),
        };
        format!(
            "data: {event}\n\ndata: {}\n\n",
            json!({"type":"response.completed","response":{"model":"gpt-6.1-sol"}})
        )
    });
    let root =
        std::env::temp_dir().join(format!("sui-yolo-dropdown-{:016x}", rand::random::<u64>()));
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(root.join("config.toml"), format!("[profiles.test]\nkind=\"openai-responses\"\nmodel=\"gpt-6.1-sol\"\nbase_url=\"http://127.0.0.1:{port}/v1\"\n[web]\naccess=\"ask\"\n")).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_sui"))
        .args(["--desktop-protocol", "3", "desktop-bridge", "--workspace"])
        .arg(&workspace)
        .args(["--profile", "test"])
        .env("HOME", &root)
        .env("SUI_HOME", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx
                .send(serde_json::from_str::<Value>(&line.unwrap()).unwrap())
                .is_err()
            {
                break;
            }
        }
    });
    let next = || {
        rx.recv_timeout(Duration::from_secs(20))
            .expect("bridge timed out")
    };
    assert_eq!(next()["approval"], "ask");
    for (run, mode) in [(1, "ask"), (2, "auto-edits"), (3, "ask")] {
        if run != 1 {
            writeln!(input, "{}", json!({"type":"set_approval","mode":mode})).unwrap();
            assert_eq!(next()["approval"], mode);
        }
        writeln!(
            input,
            "{}",
            json!({"type":"send","run":run,"text":"Exercise the local approval policy."})
        )
        .unwrap();
        let mut prompts = 0;
        loop {
            let event = next();
            if event["type"] == "permission" {
                prompts += 1;
                if run == 1 && prompts == 1 {
                    assert_eq!(event["allow_session"], false);
                    writeln!(input, "{}", json!({"type":"set_approval","mode":"yolo"})).unwrap();
                    let resolved = next();
                    assert_eq!(resolved["type"], "permission_resolved");
                    assert_eq!(resolved["id"], event["id"]);
                    assert_eq!(next()["approval"], "yolo");
                    // No separate Allow click: this command must now execute.
                    continue;
                }
                if run == 1 {
                    assert_eq!(
                        prompts, 2,
                        "YOLO must not prompt for scripts or file writes"
                    );
                    assert_eq!(
                        event["allow_yolo"], false,
                        "only independent web consent may prompt"
                    );
                    writeln!(input, "{}", json!({"type":"set_approval","mode":"yolo"})).unwrap();
                    assert_eq!(
                        next()["type"],
                        "settings",
                        "changing YOLO must not resolve web consent"
                    );
                }
                writeln!(
                    input,
                    "{}",
                    json!({"type":"approve","id":event["id"],"choice":"deny"})
                )
                .unwrap();
            }
            if event["type"] == "run_done" {
                break;
            }
        }
        assert_eq!(prompts, if run == 1 { 2 } else { 1 });
        assert_eq!(next()["type"], "settings");
    }
    assert!(
        !workspace.join("first.txt").exists(),
        "YOLO authorizes local deletion"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("script.txt")).unwrap(),
        "ran"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("auto.txt")).unwrap(),
        "written"
    );
    assert!(
        !workspace.join("denied.txt").exists(),
        "Ask must revoke file approval"
    );
    drop(input);
    assert!(child.wait().unwrap().success());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn reasoning_and_edit_mode_reach_runtime_without_granting_commands() {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = requests.clone();
    let port = common::serve(move |body, _| {
        let mut requests = captured.lock().unwrap();
        requests.push(serde_json::from_slice(body).unwrap());
        let call = match requests.len() {
            1 => Some(("write_file", json!({"path":"kept.txt","content":"keep"}))),
            2 => Some(("bash", json!({"command":"printf ok > command-output.txt"}))),
            3 => Some(("bash", json!({"command":"rm kept.txt"}))),
            5 => Some(("write_file", json!({"path":"denied.txt","content":"deny"}))),
            _ => None,
        };
        let event = if let Some((name, args)) = call {
            json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":format!("call-{}",requests.len()),"name":name,"arguments":args.to_string()}})
        } else {
            json!({"type":"response.output_text.delta","delta":"done"})
        };
        format!(
            "data: {event}\n\ndata: {}\n\n",
            json!({"type":"response.completed","response":{"model":"gpt-6.1-sol","usage":{"input_tokens":10,"output_tokens":1}}})
        )
    });
    let root = std::env::temp_dir().join(format!("sui-settings-{:016x}", rand::random::<u64>()));
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(root.join("config.toml"), format!("[profiles.test]\nkind = \"openai-responses\"\nmodel = \"gpt-6.1-sol\"\nbase_url = \"http://127.0.0.1:{port}/v1\"\n")).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_sui"))
        .args(["--desktop-protocol", "1", "desktop-bridge", "--workspace"])
        .arg(&workspace)
        .args(["--profile", "test", "--reasoning-effort", "medium"])
        .env("HOME", &root)
        .env("SUI_HOME", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx
                .send(serde_json::from_str::<Value>(&line.unwrap()).unwrap())
                .is_err()
            {
                break;
            }
        }
    });
    let next = || {
        rx.recv_timeout(Duration::from_secs(20))
            .expect("bridge event timed out")
    };
    let hello = next();
    assert_eq!(hello["reasoning_effort"], "medium");
    assert!(!hello["reasoning_choices"]
        .as_array()
        .unwrap()
        .contains(&json!("none")));
    let mut input = child.stdin.take().unwrap();
    writeln!(
        input,
        "{}",
        json!({"type":"set_approval","mode":"auto-edits"})
    )
    .unwrap();
    assert_eq!(next()["approval"], "auto-edits");
    writeln!(
        input,
        "{}",
        json!({"type":"send","run":1,"text":"Write the file, run the command, then finish."})
    )
    .unwrap();
    let mut permissions = 0;
    loop {
        let event = next();
        if event["type"] == "permission" {
            permissions += 1;
            if permissions == 1 {
                writeln!(
                    input,
                    "{}",
                    json!({"type":"set_approval","mode":"auto-edits"})
                )
                .unwrap();
                assert_eq!(
                    next()["approval"],
                    "auto-edits",
                    "mode switching works at a pending prompt"
                );
            }
            assert_eq!(
                event["allow_session"], false,
                "file edits should not prompt in auto edits: {event}"
            );
            writeln!(input, "{}", json!({"type":"approve","id":event["id"],"choice":if permissions == 1 { "session" } else { "deny" }})).unwrap();
        }
        if event["type"] == "run_done" {
            break;
        }
    }
    assert_eq!(
        permissions, 2,
        "session approval must not grant later commands"
    );
    assert_eq!(next()["approval"], "auto-edits");
    writeln!(input, "{}", json!({"type":"set_approval","mode":"ask"})).unwrap();
    assert_eq!(next()["approval"], "ask");
    writeln!(
        input,
        "{}",
        json!({"type":"send","run":2,"text":"Write another file."})
    )
    .unwrap();
    let mut revoked_prompt = false;
    loop {
        let event = next();
        if event["type"] == "permission" {
            assert_eq!(event["allow_session"], true);
            revoked_prompt = true;
            writeln!(
                input,
                "{}",
                json!({"type":"approve","id":event["id"],"choice":"deny"})
            )
            .unwrap();
        }
        if event["type"] == "run_done" {
            break;
        }
    }
    assert!(
        revoked_prompt,
        "Ask mode must revoke automatic file approval"
    );
    assert!(!workspace.join("denied.txt").exists());
    drop(input);
    assert!(child.wait().unwrap().success());
    assert_eq!(
        std::fs::read_to_string(workspace.join("kept.txt")).unwrap(),
        "keep"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("command-output.txt")).unwrap(),
        "ok"
    );
    assert_eq!(requests.lock().unwrap().len(), 6);
    for request in requests.lock().unwrap().iter() {
        assert_eq!(request["reasoning"]["effort"], "medium");
        assert_eq!(request["model"], "gpt-6.1-sol");
    }
    let mut resume = Command::new(env!("CARGO_BIN_EXE_sui"))
        .args(["--desktop-protocol", "1", "desktop-bridge", "--workspace"])
        .arg(&workspace)
        .args([
            "--profile",
            "test",
            "--resume",
            hello["run_id"].as_str().unwrap(),
        ])
        .env("HOME", &root)
        .env("SUI_HOME", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(resume.stdout.take().unwrap()).lines();
    let restored: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(restored["reasoning_effort"], "medium");
    assert_eq!(
        restored["approval"], "ask",
        "approval is reset for each connection"
    );
    drop(resume.stdin.take());
    assert!(resume.wait().unwrap().success());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn desktop_protocol_never_falls_back_to_a_model_prompt() {
    let result = Command::new(env!("CARGO_BIN_EXE_sui"))
        .args(["--desktop-protocol", "1", "not-a-desktop-command"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(error.contains("Desktop protocol requires a desktop command"));
    assert!(!error.contains("no API key"));
}

#[test]
fn account_catalog_selects_multiple_models_without_changing_profile_or_signing_in() {
    let root = std::env::temp_dir().join(format!(
        "sui-account-catalog-{:016x}",
        rand::random::<u64>()
    ));
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let config = "[profiles.account]\nkind = \"chatgpt\"\nmodel = \"\"\n";
    std::fs::write(root.join("config.toml"), config).unwrap();
    std::fs::create_dir_all(root.join("chatgpt")).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let credentials = json!({"client_id":"oaiapp_fixture","subject":"subject","access_token":"private-fixture-token","scopes":["chatgpt.tokens.use.direct"],"expires_at":now+3600});
    std::fs::write(root.join("chatgpt/account.json"), credentials.to_string()).unwrap();
    let cache = json!({"schema":1,"account":sui::context::sha256_hex(b"oaiapp_fixture:subject"),"fetched_at":now,"items":[
        {"id":"first-id","name":"First display label","reasoning_levels":[{"effort":"low","description":"Fast"},{"effort":"high","description":"Deep"}],"default_reasoning":"low"},
        {"id":"second-id","name":"Second display label","reasoning_levels":[{"effort":"medium","description":"Balanced"},{"effort":"xhigh","description":"Deep"},{"effort":"ultra","description":"Delegation"}],"default_reasoning":"medium"}
    ]});
    std::fs::write(root.join("chatgpt/account.models.json"), cache.to_string()).unwrap();
    let start = |extra: &[&str]| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sui"))
            .args(["--desktop-protocol", "2", "desktop-bridge", "--workspace"])
            .arg(&workspace)
            .args(["--profile", "account"])
            .args(extra)
            .env("HOME", &root)
            .env("SUI_HOME", &root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let first = lines
            .next()
            .expect("bridge did not start from saved catalog")
            .unwrap();
        let hello: Value = serde_json::from_str(&first).unwrap();
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
        hello
    };
    let first = start(&[]);
    assert_eq!(
        first["model"], "first-id",
        "blank account profiles choose the first catalog model"
    );
    assert_eq!(first["reasoning_effort"], "low");
    let second = start(&["--model", "second-id", "--reasoning-effort", "xhigh"]);
    assert_eq!(second["model"], "second-id");
    assert_eq!(second["model_name"], "Second display label");
    assert_eq!(
        second["reasoning_choices"],
        json!(["default", "medium", "xhigh"])
    );
    assert_eq!(second["reasoning_effort"], "xhigh");
    assert_eq!(
        second["reasoning_levels"][2]["effort"], "ultra",
        "catalog modes are retained without pretending to implement them"
    );
    // Supply a completed native history fixture; empty chats are deliberately
    // not resumable. No account inference is performed by this test.
    {
        let dir = root
            .join(".local/share/sui/runs")
            .join(second["run_id"].as_str().unwrap());
        let mut journal = sui::journal::Journal::open_named(&dir, "solo").unwrap();
        journal.log("turn_start", json!({"run":1}));
        journal.log("user", json!({"content":"fixture question"}));
        journal.log(
            "assistant",
            json!({"content":"fixture answer","tool_calls":[]}),
        );
        journal.log("turn_end", json!({"run":1,"outcome":"returned"}));
    }
    let restored = start(&["--resume", second["run_id"].as_str().unwrap()]);
    assert_eq!(restored["model"], "second-id");
    assert_eq!(restored["reasoning_effort"], "xhigh");
    for args in [
        ["--model", "second-id", "--reasoning-effort", "low"],
        ["--model", "not-in-account", "--reasoning-effort", "high"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_sui"))
            .args(["--desktop-protocol", "2", "desktop-bridge", "--workspace"])
            .arg(&workspace)
            .args(["--profile", "account"])
            .args(args)
            .env("HOME", &root)
            .env("SUI_HOME", &root)
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
    assert_eq!(
        std::fs::read_to_string(root.join("config.toml")).unwrap(),
        config
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(root.join("chatgpt/account.json")).unwrap())
            .unwrap(),
        credentials
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn image_turn_live_yolo_switch_and_eof_restore_preserve_real_runtime_state() {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let capture = requests.clone();
    let port = common::serve(move |body, _| {
        let mut requests = capture.lock().unwrap();
        requests.push(serde_json::from_slice(body).unwrap());
        let tool = match requests.len() {
            1 => Some(("write_file", json!({"path":"keep.txt","content":"keep"}))),
            2 => Some(("bash", json!({"command":"pwd && ls -la"}))),
            3 => Some(("bash", json!({"command":"rm keep.txt"}))),
            5 => Some((
                "write_file",
                json!({"path":"cancelled.txt","content":"must not write"}),
            )),
            _ => None,
        };
        let event = match tool {
            Some((name, args)) => {
                json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":format!("call-{}",requests.len()),"name":name,"arguments":args.to_string()}})
            }
            None => json!({"type":"response.output_text.delta","delta":"done"}),
        };
        format!(
            "data: {event}\n\ndata: {}\n\n",
            json!({"type":"response.completed","response":{"model":"gpt-6.1-sol"}})
        )
    });
    let root = std::env::temp_dir().join(format!("sui-images-yolo-{:016x}", rand::random::<u64>()));
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(root.join("config.toml"),format!("[profiles.test]\nkind=\"openai-responses\"\nmodel=\"gpt-6.1-sol\"\nimage_input=true\nbase_url=\"http://127.0.0.1:{port}/v1\"\n")).unwrap();
    let start = |resume: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sui"));
        command
            .args(["--desktop-protocol", "3", "desktop-bridge", "--workspace"])
            .arg(&workspace)
            .args(["--profile", "test"]);
        if let Some(id) = resume {
            command.args(["--resume", id]);
        }
        let mut child = command
            .env("HOME", &root)
            .env("SUI_HOME", &root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx
                    .send(serde_json::from_str::<Value>(&line.unwrap()).unwrap())
                    .is_err()
                {
                    break;
                }
            }
        });
        (child, rx)
    };
    let (mut child, rx) = start(None);
    let next = || {
        rx.recv_timeout(Duration::from_secs(20))
            .expect("native bridge timed out")
    };
    let hello = next();
    assert_eq!(hello["image_input"], true);
    let mut input = child.stdin.take().unwrap();
    let pixels="iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jEn8AAAAASUVORK5CYII=";
    writeln!(input,"{}",json!({"type":"send","run":1,"text":"Inspect this screenshot and make the file.","images":[{"name":"screen.png","mime":"image/png","data":pixels}]})).unwrap();
    let mut prompts = 0;
    loop {
        let event = next();
        if event["type"] == "permission" {
            prompts += 1;
            assert_eq!(event["allow_yolo"], true);
            assert_eq!(
                prompts, 1,
                "YOLO must not prompt for later local commands: {event}"
            );
            assert_eq!(event["allow_session"], true);
            writeln!(
                input,
                "{}",
                json!({"type":"approve","id":event["id"],"choice":"yolo"})
            )
            .unwrap();
        }
        if event["type"] == "run_done" {
            break;
        }
    }
    assert_eq!(
        prompts, 1,
        "YOLO approves compound commands and destructive local commands"
    );
    assert_eq!(next()["approval"], "yolo");
    writeln!(input, "{}", json!({"type":"set_approval","mode":"ask"})).unwrap();
    assert_eq!(next()["approval"], "ask");
    writeln!(
        input,
        "{}",
        json!({"type":"send","run":2,"text":"Start another edit."})
    )
    .unwrap();
    loop {
        if next()["type"] == "permission" {
            break;
        }
    }
    drop(input);
    assert!(
        child.wait().unwrap().success(),
        "EOF should cancel and finish journaling before exit"
    );
    assert!(!workspace.join("cancelled.txt").exists());
    assert!(
        !workspace.join("keep.txt").exists(),
        "YOLO approved the removal"
    );
    assert_eq!(requests.lock().unwrap().len(), 5);
    let journal = root
        .join(".local/share/sui/runs")
        .join(hello["run_id"].as_str().unwrap())
        .join("solo.jsonl");
    let source = std::fs::read_to_string(journal).unwrap();
    assert!(!source.contains(pixels));
    assert!(source.contains("content_ref"));
    let (mut restored, rx) = start(Some(hello["run_id"].as_str().unwrap()));
    let next = || rx.recv_timeout(Duration::from_secs(20)).unwrap();
    let hello = next();
    assert_eq!(hello["recovered"], false);
    assert_eq!(hello["approval"], "ask");
    assert_eq!(hello["image_input"], true);
    let mut input = restored.stdin.take().unwrap();
    writeln!(
        input,
        "{}",
        json!({"type":"send","run":1,"text":"Describe the original screenshot."})
    )
    .unwrap();
    loop {
        if next()["type"] == "run_done" {
            break;
        }
    }
    drop(input);
    assert!(restored.wait().unwrap().success());
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 6);
    for request in [requests.first().unwrap(), requests.last().unwrap()] {
        assert!(
            request["input"].to_string().contains(pixels),
            "image content survives native recovery"
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}
