//! Borrower side of `CX_BORROW`: a 0600 cache of short-lived owner tokens, a
//! single-flight refresh over SSH, and the exec-only t3 path that never touches
//! the network while a valid cached token exists.
use crate::{
    borrow::{self, Inventory},
    passthrough::{self, BorrowedTokens},
    state::{self, Paths},
    usage::Observation,
};
use anyhow::{ensure, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    time::{Duration, Instant},
};

/// A token this close to expiry is treated as expired: Codex itself starts
/// refreshing inside the same window, and the borrower has no refresh token.
const MARGIN_SECONDS: i64 = 300;
const SPAWN_DEADLINE: Duration = Duration::from_secs(15);
const MANUAL_DEADLINE: Duration = Duration::from_secs(90);
/// After a failed refresh, spawns go straight to the local path for this long.
const FAILURE_BACKOFF_SECONDS: i64 = 60;

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    email: String,
    account_id: String,
    expires_at: i64,
    access_token: String,
    id_token: String,
    #[serde(default)]
    last_used_ns: i64,
    #[serde(default)]
    usage: Option<Observation>,
}
impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Entry { credentials: [redacted] }")
    }
}

#[derive(Default, Debug, Serialize, Deserialize)]
struct Cache {
    #[serde(default)]
    refresh_failed_at: i64,
    #[serde(default)]
    accounts: BTreeMap<String, Entry>,
}

struct Files {
    cache: PathBuf,
    cache_lock: PathBuf,
    refresh_lock: PathBuf,
}

fn files(paths: &Paths, host: &str) -> Result<Files> {
    ensure!(borrow::valid_host(host), "invalid borrow host");
    let dir = paths.data.join("borrow");
    fs::create_dir_all(&dir)?;
    for path in [&paths.data, &dir] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(Files {
        cache: dir.join(format!("{host}.json")),
        cache_lock: dir.join(format!("{host}.lock")),
        refresh_lock: dir.join(format!("{host}.refresh.lock")),
    })
}

fn open_lock(path: &PathBuf) -> Result<File> {
    Ok(OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)?)
}

/// Poll for the lock until the deadline; `None` means someone else still holds it.
fn lock_until(path: &PathBuf, deadline: Instant) -> Result<Option<File>> {
    let file = open_lock(path)?;
    loop {
        if file.try_lock_exclusive().is_ok() {
            return Ok(Some(file));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn load(path: &PathBuf) -> Cache {
    // An unreadable cache is just an empty one; the next refresh rewrites it.
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save(path: &PathBuf, cache: &Cache) -> Result<()> {
    let dir = path.parent().context("cache has no directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(dir)?;
    temporary.write_all(&serde_json::to_vec_pretty(cache)?)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temporary.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Run `f` with the cache read-modify-write lock held. `f` returning `Ok(true)` saves.
fn with_cache<T>(files: &Files, f: impl FnOnce(&mut Cache) -> Result<(T, bool)>) -> Result<T> {
    let lock = open_lock(&files.cache_lock)?;
    lock.lock_exclusive()?;
    let mut cache = load(&files.cache);
    let (value, dirty) = f(&mut cache)?;
    if dirty {
        save(&files.cache, &cache)?;
    }
    Ok(value)
}

fn exhausted(entry: &Entry, now: i64) -> bool {
    entry.usage.as_ref().is_some_and(|u| {
        !u.allowed
            || u.primary
                .iter()
                .chain(u.secondary.iter())
                .any(|w| w.used_percent >= 100.0 && w.resets_at.is_none_or(|t| t > now))
    })
}

fn usable(entry: &Entry, now: i64) -> bool {
    entry.expires_at > now + MARGIN_SECONDS
}

/// Least-recently-used valid alias, preferring accounts not known to be exhausted.
fn choose(cache: &Cache, now: i64) -> Option<String> {
    cache
        .accounts
        .iter()
        .filter(|(_, e)| usable(e, now))
        .min_by_key(|(alias, e)| (exhausted(e, now), e.last_used_ns, *alias))
        .map(|(alias, _)| alias.clone())
}

/// Pick a valid cached grant, record its use, and write its stable overlay.
/// Everything happens under the cache lock so concurrent probes agree on rotation
/// and never race on the overlay's symlinks.
fn check_pinned(cache: &Cache, pin: &str, now: i64) -> Result<()> {
    let Some(entry) = cache.accounts.get(pin) else {
        anyhow::bail!("{pin}: unknown pin; not in the borrow cache; no fallback");
    };
    if !usable(entry, now) {
        anyhow::bail!("{pin}: pinned account is expired; no fallback");
    }
    if exhausted(entry, now) {
        anyhow::bail!("{pin}: quota exhausted; no fallback");
    }
    Ok(())
}

fn pick_and_prepare(
    paths: &Paths,
    files: &Files,
    host: &str,
    pin: Option<&str>,
) -> Result<Option<PathBuf>> {
    with_cache(files, |cache| {
        let now = chrono::Utc::now().timestamp();
        let alias = if let Some(pin) = pin {
            check_pinned(cache, pin, now)?;
            pin.to_string()
        } else {
            let Some(alias) = choose(cache, now) else {
                return Ok((None, false));
            };
            alias
        };
        let entry = cache.accounts.get_mut(&alias).context("alias vanished")?;
        let home = passthrough::prepare_borrow_overlay(
            paths,
            host,
            &alias,
            &BorrowedTokens {
                id_token: &entry.id_token,
                access_token: &entry.access_token,
                account_id: &entry.account_id,
            },
        )?;
        entry.last_used_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(now);
        eprintln!("cx: t3-codex --borrow {host} -> {alias} ({})", entry.email);
        Ok((Some(home), true))
    })
}

fn expiry_text(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map_or_else(|| timestamp.to_string(), |t| t.to_rfc3339())
}

/// Fetch the owner's inventory, then lend each alias that is missing or near
/// expiry. The owner renews only those accounts; aliases added on the owner
/// show up here automatically. Returns `(alias, expires_at)` for every cached alias.
fn refresh_inner(
    files: &Files,
    host: &str,
    via: Option<&str>,
    deadline: Instant,
) -> Result<Vec<(String, i64)>> {
    let output = borrow::ssh(
        host,
        via,
        &borrow::owner_command("lend --inventory"),
        deadline,
    )?;
    ensure!(output.status.success(), "inventory failed on {host}");
    let inventory: Inventory = serde_json::from_slice(&output.stdout)
        .map_err(|_| anyhow::anyhow!("invalid inventory response from {host}"))?;
    for entry in &inventory.accounts {
        state::validate_alias(&entry.alias)
            .map_err(|_| anyhow::anyhow!("invalid alias in inventory from {host}"))?;
    }
    let now = chrono::Utc::now().timestamp();
    let cached = with_cache(files, |cache| Ok((cache.accounts.clone(), false)))?;
    let mut fetched = BTreeMap::new();
    for item in &inventory.accounts {
        let current = cached.get(&item.alias).is_some_and(|e| {
            usable(e, now) && e.email == item.email && e.account_id == item.account_id
        });
        if current {
            continue;
        }
        if Instant::now() >= deadline {
            eprintln!(
                "cx: warning: borrow refresh ran out of time before {}",
                item.alias
            );
            break;
        }
        let command = borrow::owner_command(&format!("lend --no-lease --alias '{}'", item.alias));
        let grant = borrow::ssh(host, via, &command, deadline)
            .and_then(|out| {
                ensure!(out.status.success(), "lend failed");
                borrow::parse_grant(&out.stdout, host)
            })
            .and_then(|grant| {
                ensure!(
                    grant.alias == item.alias && grant.account_id == item.account_id,
                    "lend response does not match the requested alias"
                );
                grant.tokens()?;
                ensure!(
                    grant.expires_at > now + MARGIN_SECONDS,
                    "lent token expires too soon"
                );
                Ok(grant)
            });
        match grant {
            Ok(grant) => {
                fetched.insert(
                    item.alias.clone(),
                    Entry {
                        email: grant.email.clone(),
                        account_id: grant.account_id.clone(),
                        expires_at: grant.expires_at,
                        access_token: grant.access_token.clone(),
                        id_token: grant.id_token.clone().unwrap_or_default(),
                        last_used_ns: 0,
                        usage: None,
                    },
                );
            }
            Err(error) => eprintln!("cx: warning: cannot borrow {}: {error:#}", item.alias),
        }
    }
    with_cache(files, |cache| {
        let mut accounts = BTreeMap::new();
        for item in inventory.accounts {
            let previous = cache
                .accounts
                .get(&item.alias)
                .filter(|e| e.email == item.email && e.account_id == item.account_id);
            let Some(mut entry) = fetched.remove(&item.alias).or_else(|| previous.cloned()) else {
                continue;
            };
            entry.last_used_ns = previous.map_or(0, |e| e.last_used_ns);
            entry.usage = item.usage;
            accounts.insert(item.alias, entry);
        }
        cache.accounts = accounts;
        let now = chrono::Utc::now().timestamp();
        ensure!(
            cache.accounts.values().any(|e| usable(e, now)),
            "{host} provided no usable credentials"
        );
        cache.refresh_failed_at = 0;
        let summary = cache
            .accounts
            .iter()
            .map(|(alias, e)| (alias.clone(), e.expires_at))
            .collect();
        Ok((summary, true))
    })
}

/// `cx borrow refresh`: warm the cache. Prints aliases and expiry only.
pub fn refresh(paths: &Paths, host: &str, via: Option<&str>) -> Result<i32> {
    let files = files(paths, host)?;
    let deadline = Instant::now() + MANUAL_DEADLINE;
    let _guard = lock_until(&files.refresh_lock, deadline)?
        .context("another borrow refresh is still running")?;
    let result = refresh_inner(&files, host, via, deadline);
    if result.is_err() {
        note_failure(&files);
    }
    for (alias, expires_at) in result? {
        println!("{alias}  expires {}", expiry_text(expires_at));
    }
    Ok(0)
}

fn note_failure(files: &Files) {
    let _ = with_cache(files, |cache| {
        cache.refresh_failed_at = chrono::Utc::now().timestamp();
        Ok(((), true))
    });
}

/// Warm cache: no network. Cold cache: one bounded, single-flight refresh.
fn prepare_t3(paths: &Paths, host: &str) -> Result<PathBuf> {
    let files = files(paths, host)?;
    let pin = borrow::pin_from_env();
    if let Some(home) = pick_and_prepare(paths, &files, host, pin.as_deref())? {
        return Ok(home);
    }
    if let Some(pin) = pin {
        anyhow::bail!("{pin}: pinned account is not usable; no fallback");
    }
    let failed_at = with_cache(&files, |cache| Ok((cache.refresh_failed_at, false)))?;
    ensure!(
        chrono::Utc::now().timestamp() - failed_at >= FAILURE_BACKOFF_SECONDS,
        "owner refresh failed recently"
    );
    let deadline = Instant::now() + SPAWN_DEADLINE;
    let guard = lock_until(&files.refresh_lock, deadline)?
        .context("timed out waiting for another borrow refresh")?;
    // A concurrent probe may have refreshed while this one waited.
    if let Some(home) = pick_and_prepare(paths, &files, host, None)? {
        return Ok(home);
    }
    let via = borrow::jump_from_env();
    if let Err(error) = refresh_inner(&files, host, via.as_deref(), deadline) {
        note_failure(&files);
        return Err(error);
    }
    drop(guard);
    pick_and_prepare(paths, &files, host, None)?.context("no borrowed credential is usable")
}

/// t3 entry point: pick locally and replace this process with Codex. Any borrow
/// problem degrades to the ordinary local route instead of failing the probe.
/// A CX_PIN failure is fatal: it never falls back to another account.
pub fn t3(paths: Paths, host: &str, args: Vec<OsString>) -> Result<i32> {
    let pin = borrow::pin_from_env();
    match prepare_t3(&paths, host) {
        Ok(home) => passthrough::exec_with_home(args, &home),
        Err(error) if pin.is_some() => Err(error),
        Err(error) => {
            eprintln!(
                "cx: warning: borrow from {host} unavailable ({error:#}); using local routing"
            );
            passthrough::run(paths, args)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(expires_at: i64, last_used_ns: i64) -> Entry {
        Entry {
            email: "a@example.test".into(),
            account_id: "a".into(),
            expires_at,
            access_token: "x".into(),
            id_token: "y".into(),
            last_used_ns,
            usage: None,
        }
    }

    #[test]
    fn rotation_prefers_least_recently_used_valid_alias() {
        let now = 1_000_000;
        let mut cache = Cache::default();
        cache.accounts.insert("a".into(), entry(now + 3600, 20));
        cache.accounts.insert("b".into(), entry(now + 3600, 10));
        cache.accounts.insert("c".into(), entry(now + 60, 0));
        assert_eq!(choose(&cache, now).as_deref(), Some("b"));
        cache.accounts.remove("b");
        assert_eq!(choose(&cache, now).as_deref(), Some("a"));
        cache.accounts.remove("a");
        assert_eq!(choose(&cache, now), None);
    }

    #[test]
    fn debug_never_prints_credentials() {
        let mut cache = Cache::default();
        let mut secret = entry(1, 0);
        secret.access_token = "super-secret-access".into();
        cache.accounts.insert("a".into(), secret);
        assert!(!format!("{cache:?}").contains("super-secret"));
    }

    #[test]
    fn pinned_account_is_accepted_even_when_it_is_not_least_recent() {
        let now = 1_000_000;
        let mut cache = Cache::default();
        cache.accounts.insert("a".into(), entry(now + 3600, 20));
        cache.accounts.insert("b".into(), entry(now + 3600, 10));
        assert!(check_pinned(&cache, "a", now).is_ok());
        let error = check_pinned(&cache, "missing", now)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("unknown pin") && error.contains("no fallback"),
            "{error}"
        );
    }

    #[test]
    fn pinned_expired_account_is_rejected() {
        let now = 1_000_000;
        let mut cache = Cache::default();
        cache.accounts.insert("old".into(), entry(now + 60, 0));
        let error = check_pinned(&cache, "old", now).unwrap_err().to_string();
        assert!(
            error.contains("expired") && error.contains("no fallback"),
            "{error}"
        );
    }
}
