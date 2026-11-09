use crate::{
    auth, routing, runtime,
    sessions::{Mode, Reservation},
    state::{Paths, Store},
};
use anyhow::{ensure, Context, Result};
use std::{ffi::OsString, fs, os::unix::process::ExitStatusExt, process::Stdio};
use tokio::signal::unix::{signal, SignalKind};

fn reserve(paths: &Paths) -> Result<Option<Reservation>> {
    if Store::new(paths.clone()).read()?.accounts.is_empty() {
        eprintln!("cx: no saved accounts; routing fallback to current login");
        return Ok(None);
    }
    routing::reserve(
        paths,
        &routing::RunOptions::default(),
        None,
        Mode::Interactive,
    )
    .map(Some)
}

pub fn pick(paths: &Paths) -> Result<i32> {
    if let Some(reservation) = reserve(paths)? {
        let account = reservation.get()?.account;
        println!("{} {}", account.alias, account.email);
    } else {
        println!("current");
    }
    Ok(0)
}

pub fn run(paths: Paths, args: Vec<OsString>) -> Result<i32> {
    run_overlay(paths, args, None)
}

pub fn run_overlay(
    paths: Paths,
    args: Vec<OsString>,
    borrowed: Option<&crate::borrow::Grant>,
) -> Result<i32> {
    let reservation = if borrowed.is_some() {
        None
    } else {
        reserve(&paths)?
    };
    let mut overlay = None;
    if let Some(reservation) = &reservation {
        let identity = reservation.get()?.account;
        let account = auth::credentials(&Store::new(paths.clone()), &identity.alias, false)?;
        let home = tempfile::tempdir()?;
        let config_path = paths.codex_home.join("config.toml");
        let mut config = if config_path.exists() {
            fs::read_to_string(config_path)?.parse::<toml::Table>()?
        } else {
            toml::Table::new()
        };
        config.insert("cli_auth_credentials_store".into(), "file".into());
        fs::write(home.path().join("config.toml"), toml::to_string(&config)?)?;
        if paths.codex_home.exists() {
            for entry in fs::read_dir(&paths.codex_home)? {
                let entry = entry?;
                if entry.file_name() == "auth.json" || entry.file_name() == "config.toml" {
                    continue;
                }
                std::os::unix::fs::symlink(entry.path(), home.path().join(entry.file_name()))?;
            }
        }
        auth::activate(
            &Paths {
                data: paths.data.clone(),
                codex_home: home.path().into(),
            },
            &account,
        )?;
        eprintln!("cx: t3-codex -> {} ({})", identity.alias, identity.email);
        overlay = Some(home);
    }
    if let Some(grant) = borrowed {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir()?;
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700))?;
        let config_path = paths.codex_home.join("config.toml");
        let mut config = if config_path.exists() {
            fs::read_to_string(config_path)?.parse::<toml::Table>()?
        } else {
            toml::Table::new()
        };
        config.insert("cli_auth_credentials_store".into(), "file".into());
        fs::write(home.path().join("config.toml"), toml::to_string(&config)?)?;
        if paths.codex_home.exists() {
            for entry in fs::read_dir(&paths.codex_home)? {
                let entry = entry?;
                if entry.file_name() != "auth.json" && entry.file_name() != "config.toml" {
                    std::os::unix::fs::symlink(entry.path(), home.path().join(entry.file_name()))?;
                }
            }
        }
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(home.path().join("auth.json"))?;
        file.write_all(serde_json::to_string(&serde_json::json!({"auth_mode":"chatgpt","tokens":{"access_token":grant.access_token,"account_id":grant.account_id}}))?.as_bytes())?;
        overlay = Some(home);
    }
    let result = tokio::runtime::Runtime::new()?.block_on(async {
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let mut hangup = signal(SignalKind::hangup())?;
        let mut command = tokio::process::Command::new(runtime::codex_binary());
        command
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        if let Some(home) = &overlay {
            command.env("CODEX_HOME", home.path());
        }
        let mut child = command.spawn()?;
        let status = loop {
            let sig = tokio::select! {
                status = child.wait() => break status?,
                _ = interrupt.recv() => libc::SIGINT,
                _ = terminate.recv() => libc::SIGTERM,
                _ = hangup.recv() => libc::SIGHUP,
            };
            if let Some(pid) = child.id() {
                unsafe {
                    libc::kill(pid as i32, sig);
                }
            }
        };
        Ok(status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)))
    });
    if let (Some(home), Some(reservation)) = (&overlay, &reservation) {
        if home.path().join("auth.json").exists() {
            let identity = reservation.get()?.account;
            let updated = auth::import_current(&Paths {
                data: paths.data.clone(),
                codex_home: home.path().into(),
            })
            .map_err(|_| anyhow::anyhow!("cannot read updated overlay credentials"))?;
            ensure!(
                identity.matches(&updated),
                "overlay account identity changed"
            );
            Store::new(paths.clone()).transaction(|state| {
                let saved = state
                    .accounts
                    .get_mut(&identity.alias)
                    .context("selected account disappeared before credential sync")?;
                ensure!(identity.matches(saved), "saved account identity changed");
                saved.data = updated.data;
                Ok(())
            })?;
        }
    }
    if let Some(grant) = borrowed {
        grant.release_remote();
    }
    result
}
