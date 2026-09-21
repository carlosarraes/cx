use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::auth;
use crate::state::{State, Store};

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

pub fn fetch(store: &Store, alias: &str) -> Result<Observation> {
    let account = auth::credentials(store, alias, false)?;
    let access_token = account
        .data
        .pointer("/tokens/access_token")
        .and_then(serde_json::Value::as_str)
        .context("stored credentials lack an access token")?;
    let endpoint = std::env::var("CX_USAGE_URL")
        .unwrap_or_else(|_| "https://chatgpt.com/backend-api/wham/usage".into());
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?
        .get(endpoint)
        .bearer_auth(access_token)
        .header("chatgpt-account-id", &account.account_id)
        .send()
        .context("usage request failed")?
        .error_for_status()
        .context("usage request was rejected")?
        .json::<UsageResponse>()
        .context("usage response is malformed")?;
    let map = |window: Option<UsageWindow>| {
        window.map(|value| Window {
            used_percent: value.used_percent,
            resets_at: value.reset_at,
            limit_window_seconds: value.limit_window_seconds,
        })
    };
    let observation = Observation {
        primary: map(response.rate_limit.primary_window),
        secondary: map(response.rate_limit.secondary_window),
        observed_at: Utc::now().timestamp(),
        allowed: response.rate_limit.allowed,
    };
    store.transaction(|state| {
        let stored = state
            .accounts
            .get_mut(alias)
            .context("account was deleted during usage request")?;
        if stored.account_id != account.account_id || stored.email != account.email {
            bail!("account identity changed during usage request");
        }
        stored.usage = Some(observation.clone());
        Ok(())
    })?;
    Ok(observation)
}

pub fn choose_next(state: &State) -> Result<String> {
    let now = Utc::now().timestamp();
    state
        .accounts
        .iter()
        .filter(|(alias, _)| state.current.as_ref() != Some(*alias))
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
    format_lines_colored(state, now, false)
}

pub fn format_lines_colored(state: &State, now: i64, color: bool) -> Vec<String> {
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
            let marker = if current {
                '*'
            } else if known {
                '-'
            } else {
                '~'
            };
            let mut parts = Vec::new();
            if let Some(usage) = &account.usage {
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
            if !known {
                parts.insert(0, "?? usage unknown (run `cx usage --live`)".into());
            }
            let (activity, since) = if current {
                ("running", state.current_since)
            } else {
                ("idle", state.last_active_at.get(alias).copied())
            };
            parts.push(match since {
                Some(since) => format!("{activity} {}", format_duration(now.saturating_sub(since))),
                None => activity.into(),
            });
            if let Some(usage) = &account.usage {
                let age = now.saturating_sub(usage.observed_at);
                if age > 120 {
                    parts.push(format!("as of {} ago", format_duration(age)));
                }
            }
            format!("{marker} {alias:<width$}  {}", parts.join(" · "))
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

fn format_duration(seconds: i64) -> String {
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
    fn usage_colors_each_window_by_its_own_quota() {
        let mut obs = State::default();
        for (alias, percent) in [("a", 69.0), ("b", 70.0), ("c", 90.0), ("d", 100.0)] {
            obs.accounts.insert(alias.into(), serde_json::from_value(serde_json::json!({
                "email": "fixture@example.test", "account_id": alias, "data": {},
                "usage": {"primary": {"used_percent": percent, "resets_at": 3600, "limit_window_seconds": 18000},
                    "secondary": {"used_percent": 100, "resets_at": 298800, "limit_window_seconds": 604800}, "observed_at": 0, "allowed": true}
            })).unwrap());
        }

        let lines = format_lines_colored(&obs, 0, true);
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
        assert!(format_lines_colored(&obs, 0, false)
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
