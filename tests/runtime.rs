#![cfg(unix)]
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use std::os::unix::process::CommandExt;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    child: Option<Child>,
}
impl Fixture {
    fn new(scenario: &str) -> Self {
        let dir = TempDir::new().unwrap();
        let script = dir.path().join("codex");
        fs::write(&script, include_str!("fake_codex.py")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(dir.path().join("home")).unwrap();
        let mut fixture = Self { dir, child: None };
        for account in ["personal", "work"] {
            let claims = json!({"email":format!("{account}@example.test"),"exp":4102444800_i64,"https://api.openai.com/auth":{"chatgpt_account_id":account,"chatgpt_user_id":account,"chatgpt_plan_type":"plus"}});
            let jwt = format!(
                "e30.{}.sig",
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
            );
            fs::write(fixture.dir.path().join("home/auth.json"),serde_json::to_vec(&json!({"auth_mode":"chatgpt","tokens":{"account_id":account,"access_token":jwt,"id_token":jwt,"refresh_token":"secret"}})).unwrap()).unwrap();
            fixture.ok(&["add", account, "--current"]);
        }
        fixture.ok(&["switch", "personal", "--yes"]);
        let child = fixture
            .command()
            .env("CX_TEST_SCENARIO", scenario)
            .stdin(Stdio::null())
            .stdout(Stdio::from(
                fs::File::create(fixture.dir.path().join("stdout")).unwrap(),
            ))
            .stderr(Stdio::from(
                fs::File::create(fixture.dir.path().join("stderr")).unwrap(),
            ))
            .process_group(0)
            .spawn()
            .unwrap();
        fixture.child = Some(child);
        fixture
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cx"));
        cmd.env("XDG_DATA_HOME", self.dir.path().join("data"))
            .env("CODEX_HOME", self.dir.path().join("home"))
            .env("CX_CODEX_BIN", self.dir.path().join("codex"))
            .env("CX_TEST_DIR", self.dir.path())
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY");
        cmd
    }
    fn ok(&self, args: &[&str]) -> String {
        let out = self.command().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
    fn touch(&self, name: &str) {
        fs::write(self.dir.path().join(name), "").unwrap();
    }
    fn events(&self) -> Vec<Value> {
        fs::read_to_string(self.dir.path().join("events"))
            .unwrap_or_default()
            .lines()
            .filter_map(|s| serde_json::from_str(s).ok())
            .collect()
    }
    fn wait(&mut self, condition: impl Fn(&Self) -> bool) {
        let start = Instant::now();
        while !condition(self) {
            if let Some(exit) = self.child.as_mut().unwrap().try_wait().unwrap() {
                panic!(
                    "cx exited {exit}: {}",
                    fs::read_to_string(self.dir.path().join("stderr")).unwrap()
                );
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "timed out; stderr: {}",
                fs::read_to_string(self.dir.path().join("stderr")).unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn finish(&mut self) -> std::process::ExitStatus {
        self.touch("quit");
        let start = Instant::now();
        loop {
            if let Some(exit) = self.child.as_mut().unwrap().try_wait().unwrap() {
                return exit;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "cx did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            if child.try_wait().ok().flatten().is_none() {
                unsafe {
                    libc::kill(child.id() as i32, libc::SIGTERM);
                }
                for _ in 0..50 {
                    if child.try_wait().ok().flatten().is_some() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

#[test]
fn bare_cx_keeps_same_children_when_switching_an_idle_session() {
    let mut f = Fixture::new("idle");
    f.wait(|f| f.dir.path().join("ready").exists());
    let output = f.ok(&["switch", "work", "--yes"]);
    assert!(!output.contains("pending"), "{output}");
    f.wait(|f| f.events().iter().any(|e| e["login"] == "work"));
    assert_eq!(
        f.events()
            .iter()
            .filter(|e| e.get("tui_pid").is_some())
            .count(),
        1
    );
    let logins: Vec<_> = f
        .events()
        .into_iter()
        .filter(|e| e.get("login").is_some())
        .collect();
    assert_eq!(logins.len(), 2);
    assert_eq!(logins[0]["server_pid"], logins[1]["server_pid"]);
    assert!(f.finish().success());
    assert_eq!(
        fs::read_dir(f.dir.path().join("data/cx/runtimes"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn native_turn_creating_commands_defer_switch_until_completion() {
    for method in [
        "turn/start",
        "review/start",
        "thread/compact/start",
        "thread/queue/start",
    ] {
        let mut f = Fixture::new(method);
        f.wait(|f| f.dir.path().join("busy").exists());
        let report = f.ok(&["switch", "work", "--yes"]);
        assert!(report.contains("pending"), "{method}: {report}");
        assert!(!f.events().iter().any(|e| e["login"] == "work"));
        f.touch("release");
        f.touch("complete");
        f.wait(|f| f.events().iter().any(|e| e["login"] == "work"));
        assert!(f.finish().success());
    }
}

#[test]
fn rejected_switch_reports_failure_without_echoing_provider_payload() {
    let mut f = Fixture::new("reject");
    f.wait(|f| f.dir.path().join("ready").exists());
    let output = f
        .command()
        .args(["switch", "work", "--yes"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-do-not-print"));
    assert!(!f.finish().success());
}

#[test]
fn replacing_the_active_alias_updates_the_runtime_identity() {
    let mut f = Fixture::new("idle");
    f.wait(|f| f.dir.path().join("ready").exists());
    let state: Value =
        serde_json::from_slice(&fs::read(f.dir.path().join("data/cx/state.json")).unwrap())
            .unwrap();
    let work = state["accounts"]["work"]["data"].clone();
    f.ok(&["del", "work"]);
    fs::write(
        f.dir.path().join("home/auth.json"),
        serde_json::to_vec(&work).unwrap(),
    )
    .unwrap();
    let report = f.ok(&["add", "personal", "--current", "--force"]);
    assert!(report.contains("work@example.test"), "{report}");
    f.wait(|f| f.events().iter().any(|e| e["login"] == "work"));
    assert!(f.finish().success());
}

#[test]
fn an_alias_replaced_during_login_is_not_acknowledged_as_the_old_identity() {
    let mut f = Fixture::new("hold-login");
    f.wait(|f| f.dir.path().join("auth_pending").exists());
    let state: Value =
        serde_json::from_slice(&fs::read(f.dir.path().join("data/cx/state.json")).unwrap())
            .unwrap();
    let work = state["accounts"]["work"]["data"].clone();
    f.ok(&["del", "work"]);
    fs::write(
        f.dir.path().join("home/auth.json"),
        serde_json::to_vec(&work).unwrap(),
    )
    .unwrap();
    let report = f.ok(&["add", "personal", "--current", "--force"]);
    assert!(report.contains("pending"), "{report}");
    f.touch("auth_release");
    f.wait(|f| f.events().iter().any(|e| e["login"] == "work"));
    assert!(f.finish().success());
}

#[test]
fn terminal_interrupt_does_not_kill_the_app_server() {
    let mut f = Fixture::new("idle");
    f.wait(|f| f.dir.path().join("ready").exists());
    let pid = f.child.as_ref().unwrap().id() as i32;
    assert_eq!(unsafe { libc::kill(-pid, libc::SIGINT) }, 0);
    f.wait(|f| f.dir.path().join("interrupted").exists());
    f.ok(&["switch", "work", "--yes"]);
    assert!(f.finish().success());
}

#[test]
fn termination_during_startup_cleans_both_children_and_socket() {
    let mut f = Fixture::new("startup");
    f.wait(|f| {
        f.events().iter().any(|e| e.get("tui_pid").is_some())
            && f.events().iter().any(|e| e.get("server_pid").is_some())
    });
    let pid = f.child.as_ref().unwrap().id() as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    assert_eq!(f.finish().code(), Some(143));
    for event in f.events() {
        for key in ["server_pid", "tui_pid"] {
            if let Some(pid) = event[key].as_i64() {
                assert_eq!(
                    unsafe { libc::kill(pid as i32, 0) },
                    -1,
                    "child {pid} still alive"
                );
            }
        }
    }
    assert_eq!(
        fs::read_dir(f.dir.path().join("data/cx/runtimes"))
            .unwrap()
            .count(),
        0
    );
}

/// Uses isolated homes and synthetic JWT claims: validates actual Codex account
/// loading, without real credentials, inference, or a browser login.
#[test]
#[ignore = "requires Codex CLI on PATH; no real credentials needed"]
fn installed_codex_applies_account_changes_without_restarting() {
    let mut f = Fixture::new("actual");
    f.wait(|f| {
        f.events()
            .iter()
            .any(|e| e["actual_email"] == "personal@example.test")
    });
    f.ok(&["switch", "work", "--yes"]);
    f.wait(|f| {
        f.events()
            .iter()
            .any(|e| e["actual_email"] == "work@example.test")
    });
    assert_eq!(
        f.events()
            .iter()
            .filter(|e| e.get("server_pid").is_some())
            .count(),
        1
    );
    assert!(f.finish().success());
}
