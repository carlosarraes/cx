use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::{fs, process::Command};

fn fixture(scenario: &str) -> (tempfile::TempDir, Command) {
    let d = tempfile::tempdir().unwrap();
    fs::create_dir_all(d.path().join("home")).unwrap();
    let claims = json!({"email":"bot@example.test","exp":4102444800_i64,"https://api.openai.com/auth":{"chatgpt_account_id":"bot","chatgpt_user_id":"bot","chatgpt_plan_type":"plus"}});
    let token = format!(
        "e30.{}.sig",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    );
    fs::write(d.path().join("home/auth.json"),json!({"auth_mode":"chatgpt","tokens":{"account_id":"bot","access_token":token,"id_token":token,"refresh_token":"fixture-secret"}}).to_string()).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cx"));
    cmd.env("CODEX_HOME", d.path().join("home"))
        .env("XDG_DATA_HOME", d.path().join("data"))
        .env("XDG_CONFIG_HOME", d.path().join("config"));
    assert!(Command::new(env!("CARGO_BIN_EXE_cx"))
        .envs(cmd.get_envs().map(|(k, v)| (k, v.unwrap())))
        .args(["add", "bot", "--current"])
        .output()
        .unwrap()
        .status
        .success());
    let p = d.path().join("data/cx/state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    state["accounts"]["bot"]["usage"] = json!({"primary":{"used_percent":0,"resets_at":chrono::Utc::now().timestamp()+3600,"limit_window_seconds":18000},"allowed":true,"observed_at":chrono::Utc::now().timestamp()});
    fs::write(p, state.to_string()).unwrap();
    let script = d.path().join("codex");
    fs::write(&script, include_str!("headless_codex.py")).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    cmd.env("CX_CODEX_BIN", script)
        .env("CX_TEST_SCENARIO", scenario);
    (d, cmd)
}

#[test]
fn headless_pin_records_model_usage_and_clean_json() {
    let (d, mut cmd) = fixture("success");
    let record = d.path().join("run.json");
    let output = d.path().join("last.txt");
    let out = cmd
        .args([
            "run",
            "--account",
            "bot@example.test",
            "--label",
            "qa",
            "--usage-file",
        ])
        .arg(&record)
        .args([
            "--",
            "exec",
            "-m",
            "model-test",
            "-c",
            "model_reasoning_effort=high",
            "--json",
            "--skip-git-repo-check",
            "-o",
        ])
        .arg(&output)
        .arg("say hi")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for line in String::from_utf8(out.stdout).unwrap().lines() {
        let _: Value = serde_json::from_str(line).unwrap();
    }
    assert_eq!(fs::read_to_string(output).unwrap(), "hi\n");
    let r: Value = serde_json::from_slice(&fs::read(record).unwrap()).unwrap();
    assert_eq!(r["account"]["alias"], "bot");
    assert_eq!(r["resolved_model"], "model-test");
    assert_eq!(r["resolved_effort"], "high");
    assert_eq!(r["usage"]["input_tokens"], 10);
    assert_eq!(r["exit_code"], 0);
    assert!(!r.to_string().contains("fixture-secret"));
}

#[test]
fn headless_errors_are_classified_recorded_and_redacted() {
    for (scenario, code) in [
        ("quota", 20),
        ("mismatch", 22),
        ("auth", 21),
        ("approval", 1),
    ] {
        let (d, mut cmd) = fixture(scenario);
        let record = d.path().join("run.json");
        let out = cmd
            .args(["run", "--account", "bot", "--usage-file"])
            .arg(&record)
            .args([
                "--",
                "exec",
                "-m",
                "model-test",
                "--skip-git-repo-check",
                "--json",
                "hi",
            ])
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(code),
            "{scenario}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let r: Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
        assert_eq!(r["exit_code"], code);
        assert!(!String::from_utf8_lossy(&out.stderr).contains("fixture-secret"));
        assert!(!String::from_utf8_lossy(&out.stdout).contains("fixture-secret"));
        assert!(!r.to_string().contains("fixture-secret"));
        let sessions: Value =
            serde_json::from_slice(&fs::read(d.path().join("data/cx/sessions.json")).unwrap())
                .unwrap();
        assert_eq!(sessions["runs"], json!({}));
    }
}

#[test]
fn resume_reports_only_this_runs_tokens_or_missing_evidence() {
    for (scenario, expected) in [
        ("resume-baseline", json!(10)),
        ("resume-unknown", Value::Null),
    ] {
        let (d, mut cmd) = fixture(scenario);
        let record = d.path().join("run.json");
        let out = cmd
            .args(["run", "--usage-file"])
            .arg(&record)
            .args([
                "--",
                "exec",
                "resume",
                "headless-root",
                "--skip-git-repo-check",
                "hi",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let r: Value = serde_json::from_slice(&fs::read(record).unwrap()).unwrap();
        assert_eq!(r["usage"]["input_tokens"], expected);
    }
}

#[test]
fn unsupported_flags_fail_before_starting_a_session() {
    let (d, mut cmd) = fixture("success");
    let out = cmd
        .args(["run", "--", "exec", "--imaginary", "hi"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(!d.path().join("data/cx/sessions.json").exists());
}

#[test]
fn stdin_and_last_message_work_without_a_tty() {
    use std::io::Write;
    let (d, mut cmd) = fixture("stdin");
    let output = d.path().join("answer");
    let mut child = cmd
        .args(["run", "--", "exec", "--skip-git-repo-check", "-o"])
        .arg(&output)
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"stdin prompt")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"hi\n");
    assert_eq!(fs::read(output).unwrap(), b"hi\n");
}

#[test]
fn absent_model_fields_are_explicitly_unavailable() {
    let (d, mut cmd) = fixture("missing-evidence");
    let record = d.path().join("run.json");
    let out = cmd
        .args(["run", "--usage-file"])
        .arg(&record)
        .args(["--", "exec", "--skip-git-repo-check", "hi"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let r: Value = serde_json::from_slice(&fs::read(record).unwrap()).unwrap();
    assert!(r["resolved_model"].is_null());
    assert!(r["model_evidence"]
        .as_str()
        .unwrap()
        .contains("unavailable"));
    assert!(r["effort_evidence"]
        .as_str()
        .unwrap()
        .contains("unavailable"));
}

#[test]
fn termination_stops_tools_even_when_server_exits_first() {
    use std::time::{Duration, Instant};
    let (d, mut cmd) = fixture("orphan-tool");
    let marker = d.path().join("tool-marker");
    cmd.env("CX_TEST_MARKER", &marker);
    let mut child = cmd
        .args(["run", "--", "exec", "--skip-git-repo-check", "hi"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !marker.exists() {
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    assert_eq!(child.wait().unwrap().code(), Some(143));
    let size = fs::metadata(&marker).unwrap().len();
    std::thread::sleep(Duration::from_millis(150));
    let after = fs::metadata(&marker).unwrap().len();
    let pid: i32 = fs::read_to_string(marker.with_extension("pid"))
        .unwrap()
        .parse()
        .unwrap();
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    assert_eq!(
        size, after,
        "tool kept running after cx released its reservation"
    );
}
