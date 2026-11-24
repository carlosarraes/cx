use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::auth;
use crate::state::{Account, State, Store};

const MAX_OBSERVATION_AGE_SECONDS: i64 = 60 * 60;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Observation {
    pub primary: Option<Window>,
    pub secondary: Option<Window>,
    pub observed_at: i64,
    pub allowed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Window {
    pub used_percent: f64,
    pub resets_at: Option<i64>,
    #[serde(default)]
    pub limit_window_seconds: Option<i64>,
}

#[derive(Deserialize)]
struct UsageResponse {
    rate_limit: RateLimit,
}

#[derive(Deserialize)]
struct RateLimit {
    allowed: bool,
    primary_window: Option<UsageWindow>,
    secondary_window: Option<UsageWindow>,
}

#[derive(Deserialize)]
struct UsageWindow {
    used_percent: f64,
    reset_at: Option<i64>,
    limit_window_seconds: Option<i64>,
}

/// Fresh credentials for `alias` plus the bearer token the ChatGPT backend expects.
pub(crate) fn bearer(store: &Store, alias: &str) -> Result<(Account, String)> {
    let account = auth::credentials(store, alias, false)?;
    let access_token = access_token(&account)?;
    Ok((account, access_token))
}

fn access_token(account: &Account) -> Result<String> {
    account
        .data
        .pointer("/tokens/access_token")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .context("stored credentials lack an access token")
}

enum Answer {
    Usage(Observation),
    Revoked,
    Rejected(reqwest::StatusCode),
}

fn request(account: &Account, access_token: &str, timeout: Duration) -> Result<Answer> {
    let endpoint = std::env::var("CX_USAGE_URL")
        .unwrap_or_else(|_| "https://chatgpt.com/backend-api/wham/usage".into());
    let response = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()?
        .get(endpoint)
        .bearer_auth(access_token)
        .header("chatgpt-account-id", &account.account_id)
        .send()
        .context("usage request failed")?;
    let status = response.status();
    if !status.is_success() {
        // The body is only classified, never echoed.
        let body = response.text().unwrap_or_default();
        return Ok(if auth::revoked_response(status, &body) {
            Answer::Revoked
        } else {
            Answer::Rejected(status)
        });
    }
    let response = response
        .json::<UsageResponse>()
        .context("usage response is malformed")?;
    let map = |window: Option<UsageWindow>| {
        window.map(|value| Window {
            used_percent: value.used_percent,
            resets_at: value.reset_at,
            limit_window_seconds: value.limit_window_seconds,
        })
    };
    Ok(Answer::Usage(Observation {
        primary: map(response.rate_limit.primary_window),
        secondary: map(response.rate_limit.secondary_window),
        observed_at: Utc::now().timestamp(),
        allowed: response.rate_limit.allowed,
    }))
}

fn record(
    state: &mut State,
    alias: &str,
    account: &Account,
    observation: &Observation,
) -> Result<()> {
    let stored = state
        .accounts
        .get_mut(alias)
        .context("account was deleted during usage request")?;
    if stored.account_id != account.account_id || stored.email != account.email {
        bail!("account identity changed during usage request");
    }
    stored.usage = Some(observation.clone());
    stored.login_revoked_at = None;
    Ok(())
}

/// Live usage check; a revoked login is recorded and returned as `auth::LoginRevoked`.
pub fn fetch(store: &Store, alias: &str) -> Result<Observation> {
    let (account, token) = bearer(store, alias)?;
    match request(&account, &token, Duration::from_secs(15))? {
        Answer::Usage(observation) => {
            store.transaction(|state| record(state, alias, &account, &observation))?;
            Ok(observation)
        }
        Answer::Revoked => {
            auth::mark_revoked(store, alias, &account.email, &account.account_id);
            Err(auth::LoginRevoked {
                alias: alias.to_owned(),
            }
            .into())
        }
        Answer::Rejected(status) => bail!("usage request was rejected with status {status}"),
    }
}

#[derive(Debug, PartialEq)]
pub enum Probe {
    Observed,
    Revoked,
    /// Nothing was learned and nothing hit the network (e.g. the token needs a refresh).
    Skipped,
    /// The backend was unreachable, slow, or answered unexpectedly.
    Unreachable,
}

/// Spawn-path usage check bounded by `deadline` in wall-clock time, lock waits included.
/// It never refreshes tokens: refresh tokens rotate, so refreshing outside `state.lock`
/// could race another cx refresh, and refreshing inside it would hold the lock on I/O.
pub fn probe(store: &Store, alias: &str, deadline: std::time::Instant) -> Probe {
    let Ok(state) = store.read() else {
        return Probe::Skipped;
    };
    let Some(account) = state.accounts.get(alias) else {
        return Probe::Skipped;
    };
    let Ok(token) = access_token(account) else {
        return Probe::Skipped;
    };
    if auth::expires_soon(account).unwrap_or(true) {
        return Probe::Skipped;
    }
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return Probe::Skipped;
    }
    match request(account, &token, remaining) {
        Ok(Answer::Usage(observation)) => {
            let _ = store.transaction_until(deadline, |state| {
                record(state, alias, account, &observation)
            });
            Probe::Observed
        }
        Ok(Answer::Revoked) => {
            let _ = store.transaction_until(deadline, |state| {
                auth::revoke(state, alias, &account.email, &account.account_id);
                Ok(())
            });
            Probe::Revoked
        }
        Ok(Answer::Rejected(_)) | Err(_) => Probe::Unreachable,
    }
}

pub fn choose_next(state: &State) -> Result<String> {
    let now = Utc::now().timestamp();
    state
        .accounts
        .iter()
        .filter(|(alias, _)| state.current.as_ref() != Some(*alias))
        .filter(|(_, account)| !account.revoked())
        .filter_map(|(alias, account)| {
            let usage = account.usage.as_ref()?;
            if !usage.allowed
                || usage.observed_at > now
                || now.saturating_sub(usage.observed_at) > MAX_OBSERVATION_AGE_SECONDS
            {
                return None;
            }
            let primary = usage.primary.as_ref()?;
            let valid = |window: &Window| {
                window.used_percent.is_finite() && (0.0..100.0).contains(&window.used_percent)
            };
            (valid(primary) && usage.secondary.as_ref().is_none_or(valid))
                .then_some((alias, primary.used_percent))
        })
        .min_by(|(alias_a, used_a), (alias_b, used_b)| {
            used_a.total_cmp(used_b).then_with(|| alias_a.cmp(alias_b))
        })
        .map(|(alias, _)| alias.clone())
        .context("no other account has fresh eligible usage data")
}

/// Uses the same selection-based activity timers and layout as cs.
pub fn format_lines(state: &State, now: i64) -> Vec<String> {
    format_lines_colored(state, now, &BTreeMap::new(), false)
}

/// Live sessions on one account; `leaving` of them have a switch pending.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct SessionCount {
    pub active: usize,
    pub leaving: usize,
}

/// Tally `(account, has_pending_switch)` pairs per account.
pub fn count_sessions<'a>(
    sessions: impl IntoIterator<Item = (&'a str, bool)>,
) -> BTreeMap<String, SessionCount> {
    let mut counts = BTreeMap::<String, SessionCount>::new();
    for (account, pending) in sessions {
        let c = counts.entry(account.to_string()).or_default();
        c.active += 1;
        c.leaving += usize::from(pending);
    }
    counts
}

/// `(5)`, `(4 →)` when all are switching away, `(5, 2 →)` when some are.
fn format_sessions(c: SessionCount) -> String {
    match c.leaving {
        0 => format!("({})", c.active),
        n if n == c.active => format!("({n} →)"),
        n => format!("({}, {n} →)", c.active),
    }
}

pub fn format_lines_colored(
    state: &State,
    now: i64,
    sessions: &BTreeMap<String, SessionCount>,
    color: bool,
) -> Vec<String> {
    let width = state.accounts.keys().map(String::len).max().unwrap_or(0);
    state
        .accounts
        .iter()
        .map(|(alias, account)| {
            let current = state.current.as_ref() == Some(alias);
            let known = account
                .usage
                .as_ref()
                .is_some_and(|u| u.primary.is_some() || u.secondary.is_some());
            let mut parts = Vec::new();
            if account.revoked() {
                let text = format!("login revoked, run `{}`", auth::reauth_command(alias));
                parts.push(if color {
                    format!("\x1b[1;31m{text}\x1b[0m")
                } else {
                    text
                });
            } else if let Some(usage) = &account.usage {
                for (fallback, window) in
                    [("primary", &usage.primary), ("secondary", &usage.secondary)]
                {
                    if let Some(window) = window {
                        parts.push(color_window(
                            format_window(window, fallback, now),
                            window.used_percent,
                            color,
                        ));
                    }
                }
                if !usage.allowed {
                    parts.push("[limited]".into());
                }
            }
            if !known && !account.revoked() {
                parts.insert(0, "?? usage unknown (run `cx usage --live`)".into());
            }
            let (activity, since) = if current {
                ("running", state.current_since)
            } else {
                ("idle", state.last_active_at.get(alias).copied())
            };
            let activity = match since {
                Some(since) => format!("{activity} {}", format_duration(now.saturating_sub(since))),
                None => activity.into(),
            };
            parts.push(activity);
            if let Some(usage) = account.usage.as_ref().filter(|_| !account.revoked()) {
                let age = now.saturating_sub(usage.observed_at);
                if age > 120 {
                    parts.push(format!("as of {} ago", format_duration(age)));
                }
            }
            format!(
                "{alias:<width$} {}  {}",
                format_sessions(sessions.get(alias).copied().unwrap_or_default()),
                parts.join(" · ")
            )
        })
        .collect()
}

fn format_window(window: &Window, fallback: &str, now: i64) -> String {
    let label = match window.limit_window_seconds.filter(|seconds| *seconds > 0) {
        Some(seconds) if seconds % 86400 == 0 => format!("{}d", seconds / 86400),
        Some(seconds) if seconds % 3600 == 0 => format!("{}h", seconds / 3600),
        Some(seconds) if seconds % 60 == 0 => format!("{}m", seconds / 60),
        Some(seconds) => format!("{seconds}s"),
        None => fallback.into(),
    };
    let mut result = format!("{label} {:.0}%", window.used_percent);
    if let Some(reset) = window.resets_at {
        result.push_str(&format!(
            " (resets {})",
            format_duration(reset.saturating_sub(now))
        ));
    }
    result
}

pub(crate) fn format_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (days, hours, minutes) = (seconds / 86400, seconds % 86400 / 3600, seconds % 3600 / 60);
    match (days, hours, minutes) {
        (0, 0, 0) => "<1m".into(),
        (0, 0, minutes) => format!("{minutes}m"),
        (0, hours, minutes) => format!("{hours}h{minutes:02}m"),
        (days, hours, _) => format!("{days}d{hours}h"),
    }
}

pub fn colors_enabled() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
        && std::env::var("TERM").as_deref() != Ok("dumb")
}

fn color_window(text: String, percent: f64, color: bool) -> String {
    if !color || !percent.is_finite() || percent < 0.0 {
        return text;
    }
    let code = if percent >= 100.0 {
        "1;31"
    } else if percent >= 90.0 {
        "31"
    } else if percent >= 70.0 {
        "33"
    } else {
        "32"
    };
    format!("\x1b[{code}m{text}\x1b[0m")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_shows_live_sessions_and_pending_switches() {
        let mut obs = State::default();
        for alias in ["a", "b", "c", "d"] {
            obs.accounts.insert(
                alias.into(),
                serde_json::from_value(serde_json::json!({
                    "email": "fixture@example.test", "account_id": alias, "data": {}
                }))
                .unwrap(),
            );
        }
        let sessions = count_sessions([
            ("a", true),
            ("a", true),
            ("b", false),
            ("b", false),
            ("b", true),
            ("c", false),
        ]);
        let lines = format_lines_colored(&obs, 0, &sessions, false);
        assert!(lines[0].starts_with("a (2 →)"), "{}", lines[0]);
        assert!(lines[1].starts_with("b (3, 1 →)"), "{}", lines[1]);
        assert!(lines[2].starts_with("c (1)"), "{}", lines[2]);
        assert!(lines[3].starts_with("d (0)"), "{}", lines[3]);
    }

    #[test]
    fn usage_colors_each_window_by_its_own_quota() {
        let mut obs = State::default();
        for (alias, percent) in [("a", 69.0), ("b", 70.0), ("c", 90.0), ("d", 100.0)] {
            obs.accounts.insert(alias.into(), serde_json::from_value(serde_json::json!({
                "email": "fixture@example.test", "account_id": alias, "data": {},
                "usage": {"primary": {"used_percent": percent, "resets_at": 3600, "limit_window_seconds": 18000},
                    "secondary": {"used_percent": 100, "resets_at": 298800, "limit_window_seconds": 604800}, "observed_at": 0, "allowed": true}
            })).unwrap());
        }

        let lines = format_lines_colored(&obs, 0, &BTreeMap::new(), true);
        for (line, (code, percent)) in
            lines
                .iter()
                .zip([("32", 69), ("33", 70), ("31", 90), ("1;31", 100)])
        {
            assert!(
                line.contains(&format!("\x1b[{code}m5h {percent}% (resets 1h00m)\x1b[0m")),
                "{line:?}"
            );
            assert!(!line.starts_with('\x1b'), "only windows should be colored");
        }
        assert!(lines[0].contains("\x1b[1;31m7d 100% (resets 3d11h)\x1b[0m"));
        assert!(format_lines_colored(&obs, 0, &BTreeMap::new(), false)
            .iter()
            .all(|line| !line.contains('\x1b')));
    }

    #[test]
    fn parses_documented_codex_usage_response_fields() {
        let response: UsageResponse = serde_json::from_value(serde_json::json!({
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {"used_percent": 42, "limit_window_seconds": 18000, "reset_after_seconds": 120, "reset_at": 1_800_000_000},
                "secondary_window": {"used_percent": 5, "limit_window_seconds": 604800, "reset_after_seconds": 43200, "reset_at": 1_900_000_000}
            },
            "credits": {"unlimited": false, "balance": "0"}
        })).unwrap();
        assert!(response.rate_limit.allowed);
        assert_eq!(
            response.rate_limit.primary_window.unwrap().used_percent,
            42.0
        );
        assert_eq!(
            response.rate_limit.secondary_window.unwrap().reset_at,
            Some(1_900_000_000)
        );
    }
}
