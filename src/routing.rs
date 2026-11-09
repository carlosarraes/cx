use crate::{
    auth,
    config::RoutingConfig,
    sessions::{Binding, Identity, ManagedSession, Mode, Process, Registry, Reservation},
    state::{Account, Paths, State, Store},
    usage::{self, Observation},
};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
#[derive(Default, Clone)]
pub struct RunOptions {
    pub account: Option<String>,
    pub label: Option<String>,
    pub usage_file: Option<std::path::PathBuf>,
}
#[derive(Debug)]
pub struct Failure {
    pub code: i32,
    pub message: String,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Failure {}
pub fn failure(code: i32, message: impl Into<String>) -> anyhow::Error {
    Failure {
        code,
        message: message.into(),
    }
    .into()
}
pub fn resolve(state: &State, name: &str) -> Result<String> {
    if state.accounts.contains_key(name) {
        return Ok(name.into());
    }
    let matches: Vec<_> = state
        .accounts
        .iter()
        .filter(|(_, a)| a.email.eq_ignore_ascii_case(name))
        .map(|(alias, _)| alias.clone())
        .collect();
    match matches.as_slice() {
        [alias] => Ok(alias.clone()),
        [] => Err(failure(21, format!("{name}: unknown account; use cx list"))),
        _ => Err(failure(
            21,
            format!("{name}: ambiguous email; choose an alias"),
        )),
    }
}
pub fn fresh(u: &Observation, now: i64, cfg: &RoutingConfig) -> bool {
    u.observed_at <= now
        && now - u.observed_at <= cfg.usage_max_age_seconds
        && u.primary
            .iter()
            .chain(u.secondary.iter())
            .all(|w| w.resets_at.is_none_or(|t| t > now))
}
pub fn eligible(u: &Observation, now: i64, cfg: &RoutingConfig) -> bool {
    fresh(u, now, cfg)
        && u.allowed
        && (u.primary.is_some() || u.secondary.is_some())
        && u.primary
            .iter()
            .chain(u.secondary.iter())
            .all(|w| w.used_percent.is_finite() && (0.0..100.0).contains(&w.used_percent))
}
fn rates(u: &Observation, now: i64) -> (f64, f64) {
    let mut windows: Vec<_> = u.primary.iter().chain(u.secondary.iter()).collect();
    windows.sort_by_key(|w| std::cmp::Reverse(w.limit_window_seconds.unwrap_or(18000)));
    let rate = |w: &crate::usage::Window| {
        (100. - w.used_percent)
            / (w.resets_at
                .map(|t| t - now)
                .unwrap_or(w.limit_window_seconds.unwrap_or(18000))
                .max(900) as f64
                / 3600.)
    };
    (
        windows.first().map_or(0., |w| rate(w)),
        windows.get(1).map_or(0., |w| rate(w)),
    )
}
pub fn select(
    s: &State,
    counts: &BTreeMap<String, usize>,
    now: i64,
    cfg: &RoutingConfig,
) -> Option<String> {
    s.accounts
        .iter()
        .filter_map(|(alias, a)| {
            let u = a.usage.as_ref()?;
            if !eligible(u, now, cfg) {
                return None;
            }
            let rates = rates(u, now);
            Some((alias, counts.get(alias).copied().unwrap_or(0), rates))
        })
        .min_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| b.2 .0.total_cmp(&a.2 .0))
                .then_with(|| b.2 .1.total_cmp(&a.2 .1))
                .then_with(|| a.0.cmp(b.0))
        })
        .map(|(a, _, _)| a.clone())
}
/// Offline pick: eligible cached quota first, else the least-used alias whose quota is unknown or stale.
pub fn select_local(
    s: &State,
    counts: &BTreeMap<String, usize>,
    now: i64,
    cfg: &RoutingConfig,
) -> Option<String> {
    select(s, counts, now, cfg).or_else(|| {
        s.accounts
            .iter()
            .filter(|(_, a)| a.usage.as_ref().is_none_or(|u| !fresh(u, now, cfg)))
            .min_by_key(|(alias, _)| (counts.get(*alias).copied().unwrap_or(0), *alias))
            .map(|(alias, _)| alias.clone())
    })
}

/// Record a time-limited lease; it expires on its own, so no release is required.
pub fn lease(registry: &Registry, identity: Identity, expires_at: i64) -> Result<String> {
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let process = Process::current()?;
    let id = format!(
        "{}-{}-{}",
        process.pid,
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    registry.update(|s| {
        s.runs.insert(
            id.clone(),
            ManagedSession {
                expires_at: Some(expires_at),
                id: id.clone(),
                process,
                account: identity,
                mode: Mode::Interactive,
                pinned: false,
                label: None,
                started_at: chrono::Utc::now().timestamp(),
                threads: vec![],
                pending: None,
                error: None,
            },
        );
        Ok(id.clone())
    })
}

pub fn prepare(paths: &Paths, pin: Option<&str>) -> Result<(State, RoutingConfig, Option<String>)> {
    let cfg = RoutingConfig::load()?;
    let store = Store::new(paths.clone());
    let mut s = store.read()?;
    let forced = pin.map(|v| resolve(&s, v)).transpose()?;
    let now = chrono::Utc::now().timestamp();
    let aliases: Vec<_> = s
        .accounts
        .keys()
        .filter(|a| forced.as_ref().is_none_or(|p| p == *a))
        .cloned()
        .collect();
    for alias in aliases {
        let account = match auth::credentials(&store, &alias, false) {
            Ok(a) => a,
            Err(_) => {
                if forced.is_some() {
                    return Err(failure(
                        21,
                        format!("{alias}: credentials unavailable; no fallback"),
                    ));
                }
                s.accounts.remove(&alias);
                continue;
            }
        };
        let usage = if account.usage.as_ref().is_some_and(|u| fresh(u, now, &cfg)) {
            account.usage.clone()
        } else {
            usage::fetch(&store, &alias).ok()
        };
        if usage.is_none() {
            if forced.is_some() {
                return Err(failure(
                    70,
                    format!("{alias}: usage unavailable; no fallback"),
                ));
            }
            s.accounts.remove(&alias);
            continue;
        }
        s.accounts.insert(alias, Account { usage, ..account });
    }
    if forced.is_none() {
        let now = chrono::Utc::now().timestamp();
        s.accounts
            .retain(|_, a| a.usage.as_ref().is_some_and(|u| eligible(u, now, &cfg)));
    }
    Ok((s, cfg, forced))
}
#[allow(clippy::too_many_arguments)]
pub fn reserve_prepared(
    registry: Registry,
    state: &State,
    cfg: &RoutingConfig,
    options: &RunOptions,
    forced: Option<&str>,
    resume: Option<&str>,
    mode: Mode,
    now: i64,
) -> Result<Reservation> {
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let process = Process::current()?;
    let id = format!(
        "{}-{}-{}",
        process.pid,
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    registry.update(|s| {
        if resume.is_some_and(|t| s.runs.values().any(|r| r.threads.iter().any(|v| v == t))) {
            return Err(failure(70, "conversation already running"));
        }
        let mut candidates = state.clone();
        let mut counts = BTreeMap::new();
        let mut headless = BTreeMap::new();
        for r in s.runs.values() {
            *counts.entry(r.account.alias.clone()).or_insert(0) += 1;
            if r.mode == Mode::Headless {
                *headless.entry(r.account.alias.clone()).or_insert(0) += 1;
            }
        }
        if mode == Mode::Headless {
            candidates.accounts.retain(|a, _| {
                headless.get(a).copied().unwrap_or(0) < cfg.max_headless_per_account
            });
        }
        let alias = if let Some(alias) = forced {
            let a = state
                .accounts
                .get(alias)
                .context("pinned account unavailable")?;
            if !a.usage.as_ref().is_some_and(|u| eligible(u, now, cfg)) {
                return Err(failure(
                    20,
                    format!("{alias}: quota exhausted or unusable; no fallback"),
                ));
            }
            if !candidates.accounts.contains_key(alias) {
                return Err(failure(
                    23,
                    format!("{alias}: headless concurrency cap reached; no fallback"),
                ));
            }
            alias.to_string()
        } else {
            let retained = resume
                .and_then(|t| s.bindings.get(t))
                .filter(|b| {
                    now >= b.last_prompt
                        && now - b.last_prompt < cfg.affinity_seconds
                        && b.last_prompt > 0
                })
                .and_then(|b| {
                    candidates
                        .accounts
                        .get(&b.account.alias)
                        .filter(|a| {
                            b.account.matches(a)
                                && a.usage.as_ref().is_some_and(|u| eligible(u, now, cfg))
                        })
                        .map(|_| b.account.alias.clone())
                });
            retained
                .or_else(|| select(&candidates, &counts, now, cfg))
                .ok_or_else(|| {
                    failure(
                        if candidates.accounts.is_empty() && !state.accounts.is_empty() {
                            23
                        } else {
                            20
                        },
                        "no account has eligible quota and available capacity",
                    )
                })?
        };
        let identity = Identity::new(&alias, &state.accounts[&alias]);
        s.runs.insert(
            id.clone(),
            ManagedSession {
                expires_at: None,
                id: id.clone(),
                process,
                account: identity.clone(),
                mode,
                pinned: forced.is_some(),
                label: options.label.clone(),
                started_at: now,
                threads: resume.map(|r| vec![r.into()]).unwrap_or_default(),
                pending: None,
                error: None,
            },
        );
        if let Some(root) = resume {
            let last_prompt = s.bindings.get(root).map_or(0, |b| b.last_prompt);
            s.bindings.insert(
                root.into(),
                Binding {
                    account: identity,
                    last_prompt,
                },
            );
        }
        Ok(())
    })?;
    Ok(Reservation { registry, id })
}
pub fn reserve(
    paths: &Paths,
    options: &RunOptions,
    resume: Option<&str>,
    mode: Mode,
) -> Result<Reservation> {
    import_legacy(paths)?;
    let (s, cfg, forced) = prepare(paths, options.account.as_deref())?;
    reserve_prepared(
        Registry::new(paths),
        &s,
        &cfg,
        options,
        forced.as_deref(),
        resume,
        mode,
        chrono::Utc::now().timestamp(),
    )
}

pub fn import_legacy(paths: &Paths) -> Result<()> {
    let reports = crate::runtime::sessions(paths)?;
    let state = Store::new(paths.clone()).read()?;
    Registry::new(paths).update(|s| {
        for report in reports {
            if s.runs
                .values()
                .any(|r| r.process.pid == report.pid && !r.id.starts_with("legacy:"))
            {
                continue;
            }
            let Some(alias) = report.account else {
                continue;
            };
            let Some(account) = state.accounts.get(&alias) else {
                continue;
            };
            if report.email.as_deref() != Some(&account.email)
                || report.account_id.as_deref() != Some(&account.account_id)
            {
                continue;
            }
            let Some(process) = Process::read(report.pid) else {
                continue;
            };
            let id = format!("legacy:{}", report.pid);
            s.runs.insert(
                id.clone(),
                ManagedSession {
                    expires_at: None,
                    id,
                    process,
                    account: Identity::new(&alias, account),
                    mode: Mode::Interactive,
                    pinned: false,
                    label: None,
                    started_at: 0,
                    threads: vec![],
                    pending: None,
                    error: Some(
                        "older launcher; reopen with updated cx for targeted switching".into(),
                    ),
                },
            );
        }
        Ok(())
    })
}

pub fn destination(paths: &Paths, name: &str) -> Result<Identity> {
    let (state, cfg, forced) = prepare(paths, Some(name))?;
    let alias = forced.context("missing explicit destination")?;
    let a = &state.accounts[&alias];
    if !a
        .usage
        .as_ref()
        .is_some_and(|u| eligible(u, chrono::Utc::now().timestamp(), &cfg))
    {
        return Err(failure(
            20,
            format!("{alias}: quota exhausted; no fallback"),
        ));
    }
    Ok(Identity::new(&alias, a))
}

pub fn preferred_resume(paths: &Paths, id: &str, root: &str) -> Result<Identity> {
    let registry = Registry::new(paths);
    let run = registry.update(|s| s.runs.get(id).cloned().context("missing reservation"))?;
    let (state, cfg, _) = prepare(paths, run.pinned.then_some(run.account.alias.as_str()))?;
    let now = chrono::Utc::now().timestamp();
    registry.update(|s| {
        let run = s.runs.get(id).context("missing reservation")?;
        if s.runs
            .iter()
            .any(|(other, r)| other != id && r.threads.iter().any(|t| t == root))
        {
            return Err(failure(70, "conversation already running"));
        }
        let mut counts = BTreeMap::new();
        for (other, r) in &s.runs {
            if other != id {
                *counts.entry(r.account.alias.clone()).or_insert(0) += 1;
            }
        }
        let recent = s
            .bindings
            .get(root)
            .filter(|b| {
                b.last_prompt > 0
                    && now >= b.last_prompt
                    && now - b.last_prompt < cfg.affinity_seconds
            })
            .and_then(|b| {
                state
                    .accounts
                    .get(&b.account.alias)
                    .filter(|a| b.account.matches(a))
                    .map(|_| b.account.clone())
            });
        let current = state
            .accounts
            .get(&run.account.alias)
            .filter(|a| run.account.matches(a))
            .map(|_| run.account.clone());
        let target = if run.pinned {
            current.filter(|a| {
                state.accounts[&a.alias]
                    .usage
                    .as_ref()
                    .is_some_and(|u| eligible(u, now, &cfg))
            })
        } else {
            recent.or(current).or_else(|| {
                select(&state, &counts, now, &cfg).map(|a| Identity::new(&a, &state.accounts[&a]))
            })
        }
        .ok_or_else(|| failure(20, "no account has eligible quota for resume"))?;
        let run = s.runs.get_mut(id).context("missing reservation")?;
        if !run.threads.iter().any(|t| t == root) {
            run.threads.push(root.into());
        }
        Ok(target)
    })
}
