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
    let pin = crate::borrow::pin_from_env();
    if Store::new(paths.clone()).read()?.accounts.is_empty() {
        if let Some(pin) = pin {
            anyhow::bail!("{pin}: unknown account; use cx list");
        }
        eprintln!("cx: no saved accounts; routing fallback to current login");
        return Ok(None);
    }
    routing::reserve(
        paths,
        &routing::RunOptions {
            account: pin,
            ..routing::RunOptions::default()
        },
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
    exec_with_home(args, overlay.as_ref().unwrap_or(&paths.codex_home))
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
    let pin = crate::borrow::pin_from_env();
    if store.read()?.accounts.is_empty() {
        if let Some(pin) = pin {
            anyhow::bail!("{pin}: unknown account; use cx list");
        }
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
        let alias = if let Some(pin) = pin.as_deref() {
            routing::resolve(state, pin)?
        } else {
            routing::select_local(state, &counts, now, &cfg)
                .context("no saved account has locally eligible quota")?
        };
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
        ensure_separate(paths, &overlay.codex_home)?;
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
        write_overlay_config(&paths.codex_home, &overlay.codex_home)?;
        link_shared_entries(&paths.codex_home, &overlay.codex_home)?;
        auth::activate(&overlay, account)?;
        eprintln!("cx: t3-codex -> {} ({})", alias, account.email);
        Ok(Some(overlay.codex_home))
    })
}

pub fn exec_with_home(args: Vec<OsString>, home: &Path) -> Result<i32> {
    let mut command = Command::new(runtime::codex_binary());
    command.args(args).env("CODEX_HOME", home);
    Err(command.exec()).context("cannot exec Codex")
}

fn write_overlay_config(shared: &Path, overlay: &Path) -> Result<()> {
    let config_path = shared.join("config.toml");
    let mut config = if config_path.exists() {
        fs::read_to_string(config_path)?
            .parse::<toml::Table>()
            .map_err(|_| anyhow::anyhow!("Codex config is malformed"))?
    } else {
        toml::Table::new()
    };
    config.insert("cli_auth_credentials_store".into(), "file".into());
    let mut temporary = tempfile::NamedTempFile::new_in(overlay)?;
    temporary.write_all(toml::to_string(&config)?.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(overlay.join("config.toml"))
        .map_err(|e| e.error)?;
    Ok(())
}

fn link_shared_entries(shared: &Path, overlay: &Path) -> Result<()> {
    if !shared.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(shared.canonicalize()?)? {
        let entry = entry?;
        if entry.file_name() == "auth.json" || entry.file_name() == "config.toml" {
            continue;
        }
        let target = overlay.join(entry.file_name());
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
    Ok(())
}

/// Short-lived tokens handed out by an owner. The owner's refresh token never
/// reaches this host, so Codex gets an empty one purely so auth.json parses.
pub struct BorrowedTokens<'a> {
    pub id_token: &'a str,
    pub access_token: &'a str,
    pub account_id: &'a str,
}

/// Codex requires id_token, access_token and refresh_token to deserialize and
/// last_refresh to treat the tokens as available. It will attempt a refresh
/// near expiry, fail on the empty refresh token, and a long-lived app-server
/// can then 401; `cx borrow refresh` keeps the cache warm for the next spawn.
pub fn borrowed_auth_json(
    tokens: &BorrowedTokens,
    now: chrono::DateTime<chrono::Utc>,
) -> serde_json::Value {
    serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": tokens.id_token,
            "access_token": tokens.access_token,
            "refresh_token": "",
            "account_id": tokens.account_id,
        },
        "last_refresh": now.to_rfc3339(),
    })
}

fn write_borrowed_auth(overlay: &Path, tokens: &BorrowedTokens) -> Result<()> {
    let mut temporary = tempfile::NamedTempFile::new_in(overlay)?;
    temporary.write_all(&serde_json::to_vec_pretty(&borrowed_auth_json(
        tokens,
        chrono::Utc::now(),
    ))?)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temporary
        .persist(overlay.join("auth.json"))
        .map_err(|e| e.error)?;
    Ok(())
}

fn ensure_separate(paths: &Paths, overlay: &Path) -> Result<()> {
    ensure!(
        paths.codex_home.canonicalize().ok().as_deref() != Some(overlay),
        "overlay directory must be separate from shared CODEX_HOME"
    );
    Ok(())
}

/// Stable per-host, per-alias overlay so exec'd Codex keeps a home that outlives this process.
pub fn prepare_borrow_overlay(
    paths: &Paths,
    host: &str,
    alias: &str,
    tokens: &BorrowedTokens,
) -> Result<PathBuf> {
    fs::create_dir_all(&paths.data)?;
    let mut home = paths.data.join("borrow-overlays");
    private_overlay_dir(&home)?;
    for part in [host, alias] {
        home = home.join(part);
        private_overlay_dir(&home)?;
    }
    let home = home.canonicalize()?;
    ensure_separate(paths, &home)?;
    write_overlay_config(&paths.codex_home, &home)?;
    link_shared_entries(&paths.codex_home, &home)?;
    write_borrowed_auth(&home, tokens)?;
    Ok(home)
}

/// Interactive borrow only: Codex is supervised here and the overlay dies with it.
pub fn run_overlay(
    paths: Paths,
    args: Vec<OsString>,
    borrowed: Option<&crate::borrow::Grant>,
) -> Result<i32> {
    let mut overlay = None;
    if let Some(grant) = borrowed {
        let home = tempfile::tempdir()?;
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700))?;
        ensure_separate(&paths, &home.path().canonicalize()?)?;
        write_overlay_config(&paths.codex_home, home.path())?;
        link_shared_entries(&paths.codex_home, home.path())?;
        write_borrowed_auth(home.path(), &grant.tokens()?)?;
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
