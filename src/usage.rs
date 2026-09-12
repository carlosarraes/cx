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

#[cfg(test)]
mod tests {
    use super::*;

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
