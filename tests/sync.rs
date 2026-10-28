use std::{fs, os::unix::fs::PermissionsExt, process::Command};
use tempfile::TempDir;

fn fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("bin")).unwrap();
    let script = r#"#!/usr/bin/env bash
set -eu
tool=$(basename "$0")
printf '%s %s\n' "$tool" "$*" >> "$SYNC_TEST_LOG"
case "$tool" in
hostname) echo "${MOCK_LOCAL_HOST:-linux-builder}";;
uname) echo 'Linux x86_64';;
cargo) echo 'unexpected local cargo' >&2; exit 91;;
rsync) exit 0;;
scp)
 if [[ "${MOCK_COPY_FAIL:-0}" == 1 ]]; then exit 92; fi
 destination="${@: -1}"
 case "$destination" in *:*) ;; *) touch "$destination";; esac;;
ssh)
 while [[ "$1" == -* ]]; do
  case "$1" in -o) shift 2;; *) shift;; esac
 done
 host="$1"; shift
 command="$*"
 case "$command" in
  *hostname*)
   if [[ "$host" == othermac ]]; then echo Carloss-MacBook-Air.local; else echo MacBook-Air-de-Thamyris.local; fi;;
  *'uname -sm'*)
   if [[ "${MOCK_MISMATCH:-0}" == 1 && "$host" == mac ]]; then echo 'Darwin x86_64'; else echo 'Darwin arm64'; fi;;
  *'mktemp -d'*) echo '/tmp/home/.cache/cx-build/source.fixture';;
  *mktemp*) echo /tmp/cs-sync-fixture;;
  *'bash -s'*)
   body=$(cat)
   printf '%s\n' "$body" >> "$SYNC_TEST_LOG"
   if [[ "$body" == *'subprocess.run'* ]]; then
    echo "BUILD_ON $host" >> "$SYNC_TEST_LOG"
    [[ "${MOCK_BUILD_FAIL:-0}" != 1 ]]
   fi;;
 esac;;
esac
"#;
    for tool in ["ssh", "scp", "rsync", "cargo", "hostname", "uname"] {
        let path = dir.path().join("bin").join(tool);
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

fn run(env: &[(&str, &str)], builder: &str) -> (bool, String, String) {
    let dir = fixture();
    let log = dir.path().join("calls");
    let output = Command::new("just")
        .args(["--quiet", "--justfile"])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/justfile"))
        .arg("--working-directory")
        .arg(dir.path())
        .args(["sync", "mac", builder])
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().join("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("SYNC_TEST_LOG", &log)
        .envs(env.iter().copied())
        .output()
        .unwrap();
    (
        output.status.success(),
        fs::read_to_string(log).unwrap_or_default(),
        String::from_utf8_lossy(&output.stderr).into(),
    )
}

#[test]
fn sync_from_mac_builds_only_on_mact() {
    let (ok, log, error) = run(&[("MOCK_LOCAL_HOST", "Carloss-MacBook-Air")], "mact");
    assert!(ok, "{error}\n{log}");
    assert!(log.contains("BUILD_ON mact"), "{log}");
    assert!(
        !log.lines().any(|l| l.starts_with("cargo ")
            || (l.starts_with("ssh ") && l.contains(" mac ") && l.contains("cargo"))),
        "{log}"
    );
    assert!(log.contains("mv -f"), "missing atomic install: {log}");
}

#[test]
fn failed_build_does_not_install() {
    let (ok, log, _) = run(&[("MOCK_BUILD_FAIL", "1")], "mact");
    assert!(!ok);
    assert!(log.contains("BUILD_ON"), "{log}");
    assert!(!log.contains("mv -f"));
    assert!(!log.lines().any(|l| l.starts_with("scp ")));
}

#[test]
fn mac_cannot_be_selected_as_builder_even_through_an_alias() {
    for builder in ["mac", "othermac"] {
        let (ok, log, _) = run(&[], builder);
        assert!(!ok);
        assert!(!log.contains("BUILD_ON"));
        assert!(!log.contains("mv -f"));
    }
}

#[test]
fn mismatched_builder_and_destination_fail_before_build() {
    let (ok, log, _) = run(&[("MOCK_MISMATCH", "1")], "mact");
    assert!(!ok);
    assert!(!log.contains("BUILD_ON"));
    assert!(!log.contains("mv -f"));
}

#[test]
fn transfer_failure_leaves_destination_binary_untouched() {
    let (ok, log, _) = run(&[("MOCK_COPY_FAIL", "1")], "mact");
    assert!(!ok);
    assert!(log.contains("BUILD_ON"));
    assert!(log.lines().any(|l| l.starts_with("scp ")));
    assert!(!log.contains("mv -f"));
}

#[test]
fn local_build_on_mac_is_blocked_before_cargo() {
    let dir = fixture();
    let log = dir.path().join("calls");
    let output = Command::new("just")
        .args(["--quiet", "--justfile"])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/justfile"))
        .arg("--working-directory")
        .arg(dir.path())
        .arg("build")
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().join("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("SYNC_TEST_LOG", &log)
        .env("MOCK_LOCAL_HOST", "Carloss-MacBook-Air")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let calls = fs::read_to_string(log).unwrap_or_default();
    assert!(!calls.lines().any(|l| l.starts_with("cargo ")), "{calls}");
}

#[test]
fn concurrent_builds_capture_their_own_artifacts_under_the_cache_lock() {
    let dir = TempDir::new().unwrap();
    let cache = dir.path().join("cache");
    let bin = dir.path().join("bin");
    fs::create_dir_all(cache.join("target/release")).unwrap();
    fs::create_dir(&bin).unwrap();
    for source in ["a", "b"] {
        fs::create_dir(dir.path().join(source)).unwrap();
    }
    let fake = bin.join("cargo");
    fs::write(&fake, r#"#!/usr/bin/env python3
import os,pathlib,time
source=pathlib.Path.cwd()
(pathlib.Path(os.environ['CARGO_TARGET_DIR'])/'release'/os.environ['SYNC_TEST_PROGRAM']).write_text(source.name)
(source/'started').touch()
if source.name=='a':time.sleep(.3)
"#).unwrap();
    fs::set_permissions(fake, fs::Permissions::from_mode(0o755)).unwrap();
    let justfile = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/justfile")).unwrap();
    let code = justfile
        .lines()
        .skip_while(|line| !line.starts_with("    import fcntl"))
        .take_while(|line| *line != "    PYBUILD")
        .map(|line| line.strip_prefix("    ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!code.is_empty());
    let code = code.replace(
        &format!(
            "cache = pathlib.Path.home() / \".cache/{}-build\"",
            env!("CARGO_PKG_NAME")
        ),
        "cache = pathlib.Path(sys.argv[2])",
    );
    let spawn = |source: &str| {
        Command::new("python3")
            .args(["-c", &code])
            .arg(dir.path().join(source))
            .arg(&cache)
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("SYNC_TEST_PROGRAM", env!("CARGO_PKG_NAME"))
            .spawn()
            .unwrap()
    };
    let mut a = spawn("a");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !dir.path().join("a/started").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "first build did not start"
        );
        assert!(a.try_wait().unwrap().is_none(), "first build exited early");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let mut b = spawn("b");
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());
    for source in ["a", "b"] {
        assert_eq!(
            fs::read_to_string(dir.path().join(source).join(env!("CARGO_PKG_NAME"))).unwrap(),
            source
        );
    }
}
