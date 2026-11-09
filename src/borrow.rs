use crate::{
    auth, routing,
    sessions::{Mode, Registry},
    state::{Paths, Store},
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{ffi::OsString, process::Command};

#[derive(Serialize, Deserialize)]
pub struct Grant {
    pub alias: String,
    pub email: String,
    pub account_id: String,
    pub lease_id: String,
    pub expires_at: i64,
    pub access_token: String,
    #[serde(skip)]
    host: Option<String>,
}
impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Grant { credentials: [redacted] }")
    }
}
fn ssh(host: &str, command: &str) -> Result<std::process::Output> {
    ensure!(
        !host.starts_with('-') && !host.is_empty(),
        "invalid borrow host"
    );
    Command::new(std::env::var_os("CX_SSH").unwrap_or_else(|| "ssh".into()))
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ControlPath=none",
            host,
            command,
        ])
        .output()
        .context("cannot run ssh")
}
impl Grant {
    pub fn release_remote(&self) {
        if let Some(host) = &self.host {
            let id = self.lease_id.replace('\'', "'\\''");
            let command =
                format!("export PATH=\"$HOME/.local/bin:$PATH\"; exec cx lend-release '{id}'");
            if !ssh(host, &command).is_ok_and(|o| o.status.success()) {
                eprintln!("cx: warning: lend-release failed");
            }
        }
    }
}
impl Drop for Grant {
    fn drop(&mut self) {
        self.release_remote();
    }
}
pub fn extract(args: &mut Vec<OsString>) -> Result<Option<String>> {
    let mut host = std::env::var("CX_BORROW").ok().filter(|s| !s.is_empty());
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--borrow" {
            args.remove(i);
            ensure!(i < args.len(), "--borrow requires a host");
            host = Some(
                args.remove(i)
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid borrow host"))?,
            );
        } else if let Some(value) = args[i].to_str().and_then(|s| s.strip_prefix("--borrow=")) {
            host = Some(value.to_string());
            args.remove(i);
        } else {
            i += 1;
        }
    }
    Ok(host)
}
fn obtain(host: &str) -> Result<Grant> {
    let output = ssh(host, "export PATH=\"$HOME/.local/bin:$PATH\"; exec cx lend")?;
    ensure!(output.status.success(), "lend failed on {host}");
    let mut grant: Grant = serde_json::from_slice(&output.stdout)
        .map_err(|_| anyhow::anyhow!("invalid lend response from {host}"))?;
    grant.host = Some(host.into());
    ensure!(
        grant.expires_at > chrono::Utc::now().timestamp(),
        "expired lend response from {host}"
    );
    Ok(grant)
}
pub fn run(paths: Paths, host: &str, args: Vec<OsString>) -> Result<i32> {
    let mut grant = obtain(host)?;
    eprintln!(
        "cx: t3-codex --borrow {host} -> {} ({})",
        grant.alias, grant.email
    );
    let result = crate::passthrough::run_overlay(paths, args, Some(&grant));
    // The overlay releases before deletion; retain Drop as cleanup for setup errors.
    if result.is_ok() {
        grant.host = None;
    }
    result
}
pub fn pick(host: &str) -> Result<i32> {
    let grant = obtain(host)?;
    println!("{} {}", grant.alias, grant.email);
    Ok(0)
}
pub fn release(paths: &Paths, id: &str) -> Result<()> {
    Registry::new(paths).update(|s| {
        if s.runs.get(id).is_some_and(|r| r.expires_at.is_some()) {
            s.runs.remove(id);
        }
        Ok(())
    })
}
pub fn lend(paths: &Paths) -> Result<i32> {
    ensure!(
        !Store::new(paths.clone()).read()?.accounts.is_empty(),
        "no saved accounts to lend"
    );
    let reservation = routing::reserve(
        paths,
        &routing::RunOptions::default(),
        None,
        Mode::Interactive,
    )?;
    let identity = reservation.get()?.account;
    let account = auth::credentials(&Store::new(paths.clone()), &identity.alias, false)?;
    let access_token = account.data["tokens"]["access_token"]
        .as_str()
        .context("missing access token")?
        .to_owned();
    let expires_at = auth::jwt_claims(&access_token)
        .ok()
        .and_then(|v| v["exp"].as_i64())
        .unwrap_or_else(|| chrono::Utc::now().timestamp() + 600);
    reservation.registry.update(|s| {
        s.runs
            .get_mut(&reservation.id)
            .context("missing lease")?
            .expires_at = Some(expires_at);
        Ok(())
    })?;
    let grant = Grant {
        alias: identity.alias,
        email: identity.email,
        account_id: identity.account_id,
        lease_id: reservation.id.clone(),
        expires_at,
        access_token,
        host: None,
    };
    eprintln!("cx: lend -> {} ({})", grant.alias, grant.email);
    println!("{}", serde_json::to_string(&grant)?);
    Ok(0)
}
