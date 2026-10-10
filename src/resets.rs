use std::cmp::Ordering;
use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};

use crate::state::Store;
use crate::usage;

#[derive(Clone, Debug, PartialEq)]
pub enum Expiry {
    At(i64),
    Never,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Credit {
    pub id: String,
    pub expiry: Expiry,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Reset,
    NothingToReset,
    NoCredit,
    AlreadyRedeemed,
}

#[derive(Debug, Deserialize)]
pub struct Consumed {
    pub code: Outcome,
    #[serde(default)]
    pub windows_reset: i64,
}

#[derive(Deserialize)]
struct ListResponse {
    credits: Vec<RawCredit>,
}

#[derive(Deserialize)]
struct RawCredit {
    id: String,
    status: String,
    expires_at: Option<String>,
}

#[derive(Serialize)]
struct ConsumeRequest<'a> {
    redeem_request_id: &'a str,
    credit_id: &'a str,
}

fn endpoint() -> String {
    std::env::var("CX_RESETS_URL")
        .unwrap_or_else(|_| "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits".into())
}

fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?)
}

/// Available credits for `alias`, the one to spend first at index 0.
pub fn list(store: &Store, alias: &str) -> Result<Vec<Credit>> {
    let (account, token) = usage::bearer(store, alias)?;
    let response = client()?
        .get(endpoint())
        .bearer_auth(token)
        .header("chatgpt-account-id", &account.account_id)
        .send()
        .context("reset credits request failed")?
        .error_for_status()
        .context("reset credits request was rejected")?
        .json::<ListResponse>()
        .context("reset credits response is malformed")?;
    Ok(available(response))
}

fn available(response: ListResponse) -> Vec<Credit> {
    let mut credits: Vec<Credit> = response
        .credits
        .into_iter()
        .filter(|c| c.status == "available")
        .map(|c| Credit {
            expiry: parse_expiry(c.expires_at.as_deref()),
            id: c.id,
        })
        .collect();
    credits.sort_by(|a, b| compare_expiry(&a.expiry, &b.expiry));
    credits
}

fn parse_expiry(value: Option<&str>) -> Expiry {
    match value {
        None => Expiry::Never,
        Some(text) => DateTime::parse_from_rfc3339(text)
            .map(|t| Expiry::At(t.timestamp()))
            .unwrap_or(Expiry::Unknown),
    }
}

/// Order in which credits are offered; `cx reset` spends the first one by default.
fn compare_expiry(a: &Expiry, b: &Expiry) -> Ordering {
    let rank = |e: &Expiry| match e {
        Expiry::At(_) => 0,
        Expiry::Never => 1,
        Expiry::Unknown => 2,
    };
    match (a, b) {
        (Expiry::At(x), Expiry::At(y)) => x.cmp(y),
        _ => rank(a).cmp(&rank(b)),
    }
}

/// Spends `credit_id`. The request id makes a retry after a lost response safe.
pub fn consume(store: &Store, alias: &str, credit_id: &str) -> Result<Consumed> {
    let (account, token) = usage::bearer(store, alias)?;
    let request_id = uuid_v4()?;
    let client = client()?;
    let send = || {
        client
            .post(format!("{}/consume", endpoint()))
            .bearer_auth(&token)
            .header("chatgpt-account-id", &account.account_id)
            .json(&ConsumeRequest {
                redeem_request_id: &request_id,
                credit_id,
            })
            .send()
    };
    let response = match send() {
        Err(error) if error.is_timeout() || error.is_connect() => send(),
        other => other,
    };
    response
        .context("reset request failed")?
        .error_for_status()
        .context("reset request was rejected")?
        .json::<Consumed>()
        .context("reset response is malformed")
}

fn uuid_v4() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("reading random bytes")?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

pub fn format_expiry(expiry: &Expiry) -> String {
    match expiry {
        Expiry::At(timestamp) => match Local.timestamp_opt(*timestamp, 0).single() {
            Some(time) => format!("expires {}", time.format("%d %b %H:%M")),
            None => "expiry unavailable".into(),
        },
        Expiry::Never => "does not expire".into(),
        Expiry::Unknown => "expiry unavailable".into(),
    }
}

pub fn format_credit(index: usize, credit: &Credit) -> String {
    format!("#{} {}", index + 1, format_expiry(&credit.expiry))
}

/// One `cx usage`-style line: `* work  2 resets · #1 6d4h (16 Oct) · #2 ...`.
pub fn format_line(
    alias: &str,
    width: usize,
    current: bool,
    credits: &[Credit],
    now: i64,
    color: bool,
) -> String {
    let marker = if current { '*' } else { '-' };
    let mut parts = vec![match credits.len() {
        0 => "no resets".to_string(),
        1 => "1 reset".to_string(),
        n => format!("{n} resets"),
    }];
    for (index, credit) in credits.iter().enumerate() {
        let (text, days) = match credit.expiry {
            Expiry::At(timestamp) => {
                let date = Local
                    .timestamp_opt(timestamp, 0)
                    .single()
                    .map(|t| format!(" ({})", t.format("%d %b")))
                    .unwrap_or_default();
                let left = timestamp.saturating_sub(now);
                (
                    format!("#{} {}{date}", index + 1, usage::format_duration(left)),
                    Some(left / 86400),
                )
            }
            Expiry::Never => (format!("#{} no expiry", index + 1), Some(i64::MAX)),
            Expiry::Unknown => (format!("#{} expiry unknown", index + 1), None),
        };
        parts.push(match days.filter(|_| color) {
            Some(days) => format!("\x1b[{}m{text}\x1b[0m", expiry_color(days)),
            None => text,
        });
    }
    format!("{marker} {alias:<width$}  {}", parts.join(" · "))
}

fn expiry_color(days_left: i64) -> &'static str {
    match days_left {
        25.. => "32",
        20.. => "92",
        15.. => "33",
        7.. => "38;5;208",
        3.. => "31",
        _ => "1;31",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_only_available_credits_soonest_expiry_first() {
        let response: ListResponse = serde_json::from_value(serde_json::json!({
            "available_count": 4,
            "credits": [
                {"id": "never", "reset_type": "codex_rate_limits", "status": "available", "granted_at": "2026-10-01T00:00:00Z", "expires_at": null},
                {"id": "late", "reset_type": "codex_rate_limits", "status": "available", "granted_at": "2026-10-01T00:00:00Z", "expires_at": "2026-11-12T09:30:00Z"},
                {"id": "used", "reset_type": "codex_rate_limits", "status": "redeemed", "granted_at": "2026-10-01T00:00:00Z", "expires_at": "2026-10-20T00:00:00Z"},
                {"id": "garbled", "reset_type": "codex_rate_limits", "status": "available", "granted_at": "2026-10-01T00:00:00Z", "expires_at": "soon"},
                {"id": "soon", "reset_type": "codex_rate_limits", "status": "available", "granted_at": "2026-10-01T00:00:00Z", "expires_at": "2026-11-07T14:00:00Z"}
            ]
        }))
        .unwrap();
        let ids: Vec<_> = available(response).into_iter().map(|c| c.id).collect();
        assert_eq!(ids, ["soon", "late", "never", "garbled"]);
    }

    #[test]
    fn lines_match_usage_layout_and_color_by_days_left() {
        let day = 86400;
        let credit = |id: &str, expiry| Credit {
            id: id.into(),
            expiry,
        };
        let credits = [
            credit("a", Expiry::At(2 * day)),
            credit("b", Expiry::At(10 * day)),
            credit("c", Expiry::Never),
            credit("d", Expiry::Unknown),
        ];
        let plain = format_line("work", 6, true, &credits, 0, false);
        assert!(
            plain.starts_with("* work    4 resets · #1 2d0h ("),
            "{plain}"
        );
        assert!(
            plain.contains(" · #3 no expiry · #4 expiry unknown"),
            "{plain}"
        );
        assert!(!plain.contains('\x1b'));
        let colored = format_line("work", 4, false, &credits, 0, true);
        assert!(
            colored.starts_with("- work  4 resets · \x1b[1;31m#1 2d0h"),
            "{colored}"
        );
        assert!(colored.contains("\x1b[38;5;208m#2 10d0h"), "{colored}");
        assert!(colored.contains("\x1b[32m#3 no expiry\x1b[0m"), "{colored}");
        assert!(colored.ends_with(" · #4 expiry unknown"), "{colored}");
        assert_eq!(
            format_line("idle", 4, false, &[], 0, true),
            "- idle  no resets"
        );
        for (days, code) in [
            (25, "32"),
            (20, "92"),
            (15, "33"),
            (7, "38;5;208"),
            (3, "31"),
            (2, "1;31"),
        ] {
            assert_eq!(expiry_color(days), code, "{days}");
        }
    }

    #[test]
    fn parses_every_consume_outcome() {
        for (code, outcome) in [
            ("reset", Outcome::Reset),
            ("nothing_to_reset", Outcome::NothingToReset),
            ("no_credit", Outcome::NoCredit),
            ("already_redeemed", Outcome::AlreadyRedeemed),
        ] {
            let parsed: Consumed =
                serde_json::from_value(serde_json::json!({"code": code})).unwrap();
            assert_eq!(parsed.code, outcome);
        }
        let parsed: Consumed =
            serde_json::from_value(serde_json::json!({"code": "reset", "windows_reset": 2}))
                .unwrap();
        assert_eq!(parsed.windows_reset, 2);
    }

    #[test]
    fn request_ids_are_version_4_uuids() {
        let id = uuid_v4().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!("89ab".contains(&id[19..20]));
        assert_ne!(id, uuid_v4().unwrap());
    }
}
