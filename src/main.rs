use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use cx::{
    auth, runtime,
    state::{self, Paths, Store},
    usage,
};
use std::ffi::OsString;

#[derive(Parser)]
#[command(
    name = "cx",
    version,
    about = "Switch between Codex accounts",
    after_help = "With no subcommand, launch Codex with live account switching.\nPass Codex arguments after --, e.g. cx -- resume --last."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    #[arg(last = true)]
    codex_args: Vec<OsString>,
}

#[derive(Subcommand)]
enum Commands {
    /// Authenticate a new account and save it under an alias
    Add {
        alias: String,
        /// Save the already-logged-in account
        #[arg(long)]
        current: bool,
        /// Replace an existing alias
        #[arg(short, long)]
        force: bool,
        /// Use Codex device-code login
        #[arg(long, conflicts_with = "current")]
        device_auth: bool,
    },
    /// Switch to an alias (- = previous, next = least-used eligible account)
    Switch {
        #[arg(allow_hyphen_values = true)]
        alias: String,
        /// Accept unmanaged-session warnings (compatible with cs)
        #[arg(short, long)]
        yes: bool,
    },
    /// Forget an alias without logging out
    Del { alias: String },
    /// List saved aliases (* = current, - = previous)
    List,
    /// Show the live login and managed sessions
    Whoami,
    /// Capture updated credentials after a login outside cx
    Refresh,
    /// Show each account's cached 5h/7d usage
    Usage {
        #[arg(long)]
        live: bool,
    },
}

fn main() {
    match execute(Cli::parse()) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::exit(1);
        }
    }
}

fn execute(cli: Cli) -> Result<i32> {
    let paths = Paths::discover()?;
    let store = Store::new(paths.clone());
    let Some(command) = cli.command else {
        return runtime::run(paths, runtime::codex_binary(), cli.codex_args);
    };
    match command {
        Commands::Add {
            alias,
            current,
            force,
            device_auth,
        } => {
            state::validate_alias(&alias)?;
            if store.read()?.accounts.contains_key(&alias) && !force {
                bail!("alias '{alias}' already exists; use `cx refresh` or --force");
            }
            let account = if current {
                auth::import_current(&paths)?
            } else {
                println!("Launching Codex login for '{alias}'...");
                auth::login(&paths, &runtime::codex_binary(), device_auth)?
            };
            let email = account.email.clone();
            store.transaction(|s| {
                capture_newer_live(&paths, s);
                s.add(&alias, account.clone(), force)?;
                s.select(&alias)?;
                auth::activate(&paths, &account)
            })?;
            println!("Saved account '{alias}' ({email}) and set it active.");
            report_switch(&paths)?;
        }
        Commands::Switch { alias, yes } => {
            let target = if alias == "next" {
                let mut candidates = store.read()?;
                for (name, account) in &mut candidates.accounts {
                    account.usage = match usage::fetch(&store, name) {
                        Ok(observation) => Some(observation),
                        Err(error) => {
                            eprintln!("{name}: {error}");
                            None
                        }
                    };
                }
                usage::choose_next(&candidates)?
            } else if alias == "-" {
                store
                    .read()?
                    .previous
                    .context("no previous account to switch to")?
            } else {
                alias
            };
            // Import externally rotated credentials before attempting any refresh.
            store.transaction(|s| {
                capture_newer_live(&paths, s);
                Ok(())
            })?;
            auth::credentials(&store, &target, false)?;
            let email = store.transaction(|s| {
                capture_newer_live(&paths, s);
                s.select(&target)?;
                let account = s
                    .accounts
                    .get(&target)
                    .context("selected account disappeared")?;
                auth::activate(&paths, account)?;
                Ok(account.email.clone())
            })?;
            println!("Switched to '{target}' ({email}).");
            report_switch(&paths)?;
            if !yes {
                eprintln!("Live switching applies to sessions launched with cx. Other Codex sessions may keep their old account.");
            }
        }
        Commands::List => {
            let state = store.read()?;
            if state.accounts.is_empty() {
                println!("No saved accounts. Use `cx add <alias>` or `cx add <alias> --current`.");
            }
            for (alias, account) in &state.accounts {
                let mark = if state.current.as_ref() == Some(alias) {
                    '*'
                } else if state.previous.as_ref() == Some(alias) {
                    '-'
                } else {
                    ' '
                };
                println!("{mark} {alias}  {}", account.email);
            }
        }
        Commands::Del { alias } => {
            if runtime::sessions(&paths)?
                .iter()
                .any(|s| s.account.as_ref() == Some(&alias) || s.pending.as_ref() == Some(&alias))
            {
                bail!("'{alias}' is in use by a managed session; switch that session or close it before deleting its credentials");
            }
            store.transaction(|s| s.delete(&alias))?;
            println!("Removed '{alias}'. Live Codex login was left intact.");
        }
        Commands::Whoami => {
            let state = store.read()?;
            match auth::import_current(&paths) {
                Ok(live) => {
                    let alias = state
                        .accounts
                        .iter()
                        .find(|(_, a)| a.account_id == live.account_id && a.email == live.email)
                        .map(|(name, _)| name.as_str())
                        .unwrap_or("unsaved");
                    println!("{alias}  {}  ({})", live.email, live.account_id);
                    if state.current.as_deref() != Some(alias) {
                        println!(
                            "Selected for cx: {} (live Codex login differs)",
                            state.current.as_deref().unwrap_or("none")
                        );
                    }
                }
                Err(error) => {
                    println!("Live Codex login unavailable: {error}");
                    println!(
                        "Selected for cx: {}",
                        state.current.as_deref().unwrap_or("none")
                    );
                }
            }
            for report in runtime::sessions(&paths)? {
                runtime::print_status(&report);
            }
        }
        Commands::Refresh => {
            let alias = store.transaction(|s| {
                let live = auth::import_current(&paths)?;
                let alias = s
                    .accounts
                    .iter()
                    .find(|(_, a)| a.account_id == live.account_id && a.email == live.email)
                    .map(|(name, _)| name.clone())
                    .context("live account has no saved alias; use `cx add <alias> --current`")?;
                let saved = s.accounts.get_mut(&alias).unwrap();
                if refresh_timestamp(&live.data) < refresh_timestamp(&saved.data) {
                    bail!("live credentials are older than the saved account; run `cx switch {alias}` to restore the latest credentials");
                }
                saved.data = live.data;
                s.select(&alias)?;
                Ok(alias)
            })?;
            println!("Refreshed '{alias}'.");
            report_switch(&paths)?;
        }
        Commands::Usage { live } => {
            if live {
                for alias in store.read()?.accounts.keys() {
                    if let Err(error) = usage::fetch(&store, alias) {
                        eprintln!("{alias}: {error}");
                    }
                }
            }
            let state = store.read()?;
            for (alias, account) in &state.accounts {
                let mark = if state.current.as_ref() == Some(alias) {
                    '*'
                } else {
                    ' '
                };
                if let Some(observation) = &account.usage {
                    println!(
                        "{mark} {alias}  5h {}  7d {}  (observed {}){}",
                        window(&observation.primary),
                        window(&observation.secondary),
                        chrono::DateTime::from_timestamp(observation.observed_at, 0)
                            .map(|t| t
                                .with_timezone(&chrono::Local)
                                .format("%m-%d %H:%M")
                                .to_string())
                            .unwrap_or_else(|| "unknown".into()),
                        if observation.allowed {
                            ""
                        } else {
                            " [limited]"
                        }
                    );
                } else {
                    println!("{mark} {alias}  usage unknown (run `cx usage --live`)");
                }
            }
            if state.accounts.is_empty() {
                println!("No saved accounts.");
            }
        }
    }
    Ok(0)
}

fn window(value: &Option<usage::Window>) -> String {
    value
        .as_ref()
        .map(|w| {
            let reset = w
                .resets_at
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                .map(|t| {
                    format!(
                        ", resets {}",
                        t.with_timezone(&chrono::Local).format("%m-%d %H:%M")
                    )
                })
                .unwrap_or_default();
            format!("{:.0}%{reset}", w.used_percent)
        })
        .unwrap_or_else(|| "unknown".into())
}

fn report_switch(paths: &Paths) -> Result<()> {
    let reports = runtime::notify(paths)?;
    let failed = reports.iter().any(|r| r.error.is_some());
    for report in &reports {
        runtime::print_status(report);
    }
    if failed {
        bail!("account selected, but some sessions did not apply it; see `cx whoami` and retry `cx switch <alias>`");
    }
    Ok(())
}

fn capture_newer_live(paths: &Paths, state: &mut state::State) {
    let Ok(live) = auth::import_current(paths) else {
        return;
    };
    if let Some(saved) = state
        .accounts
        .values_mut()
        .find(|a| a.account_id == live.account_id && a.email == live.email)
    {
        let timestamp = |v: &serde_json::Value| {
            v["last_refresh"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        };
        if timestamp(&live.data) > timestamp(&saved.data) {
            saved.data = live.data;
        }
    }
}

fn refresh_timestamp(value: &serde_json::Value) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    value["last_refresh"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
}
