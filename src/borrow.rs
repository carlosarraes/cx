use crate::{
    auth,
    config::RoutingConfig,
    passthrough::BorrowedTokens,
    routing,
    sessions::{Identity, Registry},
    state::{self, Paths, Store},
    usage::Observation,
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::Read,
    os::unix::process::CommandExt,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

/// Owner-side view of one usable alias; carries no credentials.
#[derive(Serialize, Deserialize)]
pub struct InventoryEntry {
    pub alias: String,
    pub email: String,
    pub account_id: String,
    pub expires_at: i64,
    pub usage: Option<Observation>,
}

#[derive(Serialize, Deserialize)]
pub struct Inventory {
    pub accounts: Vec<InventoryEntry>,
}

#[derive(Serialize, Deserialize)]
pub struct Grant {
    pub alias: String,
    pub email: String,
    pub account_id: String,
    #[serde(default)]
    pub lease_id: String,
    pub expires_at: i64,
    pub access_token: String,
    // Optional so readers of older lenders still parse; new lends always send it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(skip)]
    host: Option<String>,
}
impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Grant { credentials: [redacted] }")
    }
}

/// An ssh host alias, hostname, or user@host. Never starts with '-'.
pub fn valid_host(host: &str) -> bool {
    let mut chars = host.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-'))
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

const SSH_OPTIONS: [&str; 6] = [
    "-o",
    "BatchMode=yes",
    "-o",
    "ControlPath=none",
    "-o",
    "ConnectTimeout=10",
];

/// Run `command` on `host`, hopping through `via` when set (`ssh via ssh host command`).
pub fn ssh(host: &str, via: Option<&str>, command: &str, deadline: Instant) -> Result<Output> {
    ensure!(valid_host(host), "invalid borrow host");
    if let Some(via) = via {
        ensure!(valid_host(via), "invalid borrow jump host");
    }
    let mut cmd = Command::new(std::env::var_os("CX_SSH").unwrap_or_else(|| "ssh".into()));
    cmd.args(SSH_OPTIONS);
    match via {
        None => cmd.args([host, command]),
        Some(via) => cmd.args([
            via.to_owned(),
            format!("ssh {} {host} {}", SSH_OPTIONS.join(" "), quote(command)),
        ]),
    };
    run_until(cmd, deadline)
}

/// Like `Command::output` but kills the whole process group at the deadline.
fn run_until(mut cmd: Command, deadline: Instant) -> Result<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .context("cannot run ssh")?;
    let drain = |stream: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            if let Some(mut stream) = stream {
                let _ = stream.read_to_end(&mut buffer);
            }
            buffer
        })
    };
    let stdout = drain(child.stdout.take().map(|s| Box::new(s) as _));
    let stderr = drain(child.stderr.take().map(|s| Box::new(s) as _));
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    let status = status.context("ssh timed out")?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

const OWNER_PATH: &str = "export PATH=\"$HOME/.local/bin:$PATH\"; exec cx";

pub fn owner_command(args: &str) -> String {
    format!("{OWNER_PATH} {args}")
}

impl Grant {
    pub fn release_remote(&self) {
        if let (Some(host), false) = (&self.host, self.lease_id.is_empty()) {
            let command = owner_command(&format!("lend-release {}", quote(&self.lease_id)));
            let deadline = Instant::now() + Duration::from_secs(15);
            if !ssh(host, None, &command, deadline).is_ok_and(|o| o.status.success()) {
                eprintln!("cx: warning: lend-release failed");
            }
        }
    }

    /// Complete token set for an overlay; refuses grants from lenders that omit the id_token.
    pub fn tokens(&self) -> Result<BorrowedTokens<'_>> {
        let id_token = self
            .id_token
            .as_deref()
            .filter(|t| auth::jwt_claims(t).is_ok())
            .context("lend response lacks a usable id_token; update cx on the owner")?;
        Ok(BorrowedTokens {
            id_token,
            access_token: &self.access_token,
            account_id: &self.account_id,
        })
    }

    pub fn validate(&self, host: &str) -> Result<()> {
        ensure!(
            !self.access_token.is_empty() && !self.account_id.is_empty(),
            "incomplete lend response from {host}"
        );
        state::validate_alias(&self.alias)
            .map_err(|_| anyhow::anyhow!("invalid alias in lend response from {host}"))?;
        ensure!(
            self.expires_at > chrono::Utc::now().timestamp(),
            "expired lend response from {host}"
        );
        Ok(())
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

pub fn jump_from_env() -> Option<String> {
    std::env::var("CX_BORROW_JUMP")
        .ok()
        .filter(|s| !s.is_empty())
}

pub fn pin_from_env() -> Option<String> {
    std::env::var("CX_PIN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Parse a lend response without ever echoing it.
pub fn parse_grant(stdout: &[u8], host: &str) -> Result<Grant> {
    let mut grant: Grant = serde_json::from_slice(stdout)
        .map_err(|_| anyhow::anyhow!("invalid lend response from {host}"))?;
    grant.host = None;
    grant.validate(host)?;
    Ok(grant)
}

fn obtain(host: &str) -> Result<Grant> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let output = ssh(
        host,
        jump_from_env().as_deref(),
        &owner_command("lend"),
        deadline,
    )?;
    ensure!(output.status.success(), "lend failed on {host}");
    let mut grant = parse_grant(&output.stdout, host)?;
    grant.host = Some(host.into());
    Ok(grant)
}
pub fn run(paths: Paths, host: &str, args: Vec<OsString>) -> Result<i32> {
    let mut grant = obtain(host)?;
    eprintln!(
        "cx: run --borrow {host} -> {} ({})",
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

#[derive(Default)]
pub struct LendOptions {
    pub alias: Option<String>,
    pub inventory: bool,
    /// Skip the registry lease; the borrower's cache refresh does not hold one.
    pub no_lease: bool,
}

fn access_expiry(token: &str) -> Option<i64> {
    auth::jwt_claims(token).ok()?["exp"].as_i64()
}

fn inventory(paths: &Paths) -> Result<i32> {
    let cfg = RoutingConfig::load()?;
    let now = chrono::Utc::now().timestamp();
    let accounts = Store::new(paths.clone())
        .read()?
        .accounts
        .into_iter()
        // Drop only accounts with fresh evidence they cannot serve requests.
        .filter(|(_, a)| {
            a.usage
                .as_ref()
                .is_none_or(|u| !routing::fresh(u, now, &cfg) || routing::eligible(u, now, &cfg))
        })
        .map(|(alias, a)| InventoryEntry {
            expires_at: a.data["tokens"]["access_token"]
                .as_str()
                .and_then(access_expiry)
                .unwrap_or(0),
            alias,
            email: a.email,
            account_id: a.account_id,
            usage: a.usage,
        })
        .collect();
    println!("{}", serde_json::to_string(&Inventory { accounts })?);
    Ok(0)
}

pub fn lend(paths: &Paths, options: LendOptions) -> Result<i32> {
    let store = Store::new(paths.clone());
    let state = store.read()?;
    ensure!(!state.accounts.is_empty(), "no saved accounts to lend");
    if options.inventory {
        return inventory(paths);
    }
    let alias = match &options.alias {
        Some(name) => routing::resolve(&state, name)?,
        None => {
            let _ = routing::import_legacy(paths);
            let mut counts = BTreeMap::new();
            for session in Registry::new(paths).list()? {
                *counts.entry(session.account.alias).or_insert(0) += 1;
            }
            routing::select_local(
                &state,
                &counts,
                chrono::Utc::now().timestamp(),
                &RoutingConfig::load()?,
            )
            .ok_or_else(|| routing::failure(20, "no account available to lend"))?
        }
    };
    // Only the handed-out account is renewed; every other saved login is left alone.
    let account = auth::credentials(&store, &alias, false)?;
    let token = |name: &str| {
        account.data["tokens"][name]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
    };
    let access_token = token("access_token").context("missing access token")?;
    let id_token = token("id_token").context("missing id token")?;
    let expires_at =
        access_expiry(&access_token).unwrap_or_else(|| chrono::Utc::now().timestamp() + 600);
    let lease_id = if options.no_lease {
        String::new()
    } else {
        routing::lease(
            &Registry::new(paths),
            Identity::new(&alias, &account),
            expires_at,
        )?
    };
    let grant = Grant {
        alias,
        email: account.email.clone(),
        account_id: account.account_id.clone(),
        lease_id,
        expires_at,
        access_token,
        id_token: Some(id_token),
        host: None,
    };
    eprintln!("cx: lend -> {} ({})", grant.alias, grant.email);
    println!("{}", serde_json::to_string(&grant)?);
    Ok(0)
}
