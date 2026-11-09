use crate::{
    auth,
    config::RoutingConfig,
    routing, runtime,
    sessions::{Mode, Reservation},
    state::{Paths, Store},
};
use anyhow::{ensure, Context, Result};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::Write,
    os::unix::{
        fs::{DirBuilderExt, PermissionsExt},
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
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
    let overlay = prepare_overlay(&paths)?;
    let mut command = Command::new(runtime::codex_binary());
    command
        .args(args)
        .env("CODEX_HOME", overlay.as_ref().unwrap_or(&paths.codex_home));
    Err(command.exec()).context("cannot exec Codex")
}

fn private_overlay_dir(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                fs::symlink_metadata(path)?.file_type().is_dir(),
                "overlay path must be a directory, not a symlink: {}",
                path.display()
            );
        }
        Err(error) => return Err(error.into()),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn prepare_overlay(paths: &Paths) -> Result<Option<PathBuf>> {
    let store = Store::new(paths.clone());
    if store.read()?.accounts.is_empty() {
        eprintln!("cx: no saved accounts; routing fallback to current login");
        return Ok(None);
    }
    let cfg = RoutingConfig::load()?;
    let mut counts = BTreeMap::new();
    for session in crate::sessions::Registry::new(paths).list()? {
        *counts.entry(session.account.alias).or_insert(0) += 1;
    }
    // Serialize reconciliation and overlay writes across simultaneous t3 probes.
    store.transaction(|state| {
        let now = chrono::Utc::now().timestamp();
        let alias = routing::select(state, &counts, now, &cfg)
            .or_else(|| {
                state
                    .accounts
                    .iter()
                    .filter(|(_, account)| {
                        account
                            .usage
                            .as_ref()
                            .is_none_or(|u| !routing::fresh(u, now, &cfg))
                    })
                    .min_by_key(|(alias, _)| (counts.get(*alias).copied().unwrap_or(0), *alias))
                    .map(|(alias, _)| alias.clone())
            })
            .context("no saved account has locally eligible quota")?;
        let account = state
            .accounts
            .get_mut(&alias)
            .context("selected account disappeared")?;
        let root = paths.data.join("t3-overlays");
        private_overlay_dir(&root)?;
        let home = root.join(&alias);
        private_overlay_dir(&home)?;
        let overlay = Paths {
            data: paths.data.clone(),
            codex_home: home.canonicalize()?,
        };
        ensure!(
            paths.codex_home.canonicalize().ok().as_ref() != Some(&overlay.codex_home),
            "overlay directory must be separate from shared CODEX_HOME"
        );
        if let Ok(mut updated) = auth::import_current(&overlay) {
            if updated.email == account.email
                && updated.account_id == account.account_id
                && auth::refresh_time(&updated.data) > auth::refresh_time(&account.data)
            {
                // Only reuse refresh tokens already held in the saved account.
                updated.data["tokens"]["refresh_token"] =
                    account.data["tokens"]["refresh_token"].clone();
                account.data = updated.data;
            }
        }
        let config_path = paths.codex_home.join("config.toml");
        let mut config = if config_path.exists() {
            fs::read_to_string(config_path)?
                .parse::<toml::Table>()
                .map_err(|_| anyhow::anyhow!("Codex config is malformed"))?
        } else {
            toml::Table::new()
        };
        config.insert("cli_auth_credentials_store".into(), "file".into());
        let mut temporary = tempfile::NamedTempFile::new_in(&overlay.codex_home)?;
        temporary.write_all(toml::to_string(&config)?.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(overlay.codex_home.join("config.toml"))
            .map_err(|e| e.error)?;
        if paths.codex_home.exists() {
            for entry in fs::read_dir(paths.codex_home.canonicalize()?)? {
                let entry = entry?;
                if entry.file_name() == "auth.json" || entry.file_name() == "config.toml" {
                    continue;
                }
                let target = overlay.codex_home.join(entry.file_name());
                match fs::read_link(&target) {
                    Ok(source) if source == entry.path() => continue,
                    Ok(_) => fs::remove_file(&target)?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
                        anyhow::bail!(
                            "overlay has local data at {}; reconcile it with {} before launching",
                            target.display(),
                            entry.path().display()
                        );
                    }
                    Err(e) => return Err(e.into()),
                }
                std::os::unix::fs::symlink(entry.path(), target)?;
            }
        }
        auth::activate(&overlay, account)?;
        eprintln!("cx: t3-codex -> {} ({})", alias, account.email);
        Ok(Some(overlay.codex_home))
    })
}

pub fn run_overlay(
    paths: Paths,
    args: Vec<OsString>,
    borrowed: Option<&crate::borrow::Grant>,
) -> Result<i32> {
    let mut overlay = None;
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
    if let Some(grant) = borrowed {
        grant.release_remote();
    }
    result
}
