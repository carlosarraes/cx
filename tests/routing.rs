use cx::{
    config::RoutingConfig,
    routing,
    sessions::Registry,
    state::{Account, Paths, State},
    usage::{Observation, Window},
};
use std::collections::BTreeMap;
fn account(week: f64, reset: i64) -> Account {
    Account {
        email: "a@x".into(),
        account_id: "a".into(),
        data: serde_json::json!({}),
        usage: Some(Observation {
            primary: Some(Window {
                used_percent: 0.,
                resets_at: Some(2000),
                limit_window_seconds: Some(18000),
            }),
            secondary: Some(Window {
                used_percent: week,
                resets_at: Some(reset),
                limit_window_seconds: Some(604800),
            }),
            observed_at: 1000,
            allowed: true,
        }),
        login_revoked_at: None,
    }
}
#[test]
fn counts_then_capacity_until_reset_and_both_limits() {
    let mut s = State::default();
    s.accounts
        .insert("a".into(), account(60., 1000 + 12 * 3600));
    s.accounts
        .insert("b".into(), account(10., 1000 + 144 * 3600));
    let cfg = RoutingConfig::default();
    assert_eq!(
        routing::select(&s, &BTreeMap::new(), 1000, &cfg),
        Some("a".into())
    );
    assert_eq!(
        routing::select(&s, &BTreeMap::from([("a".into(), 1)]), 1000, &cfg),
        Some("b".into())
    );
    s.accounts
        .get_mut("a")
        .unwrap()
        .usage
        .as_mut()
        .unwrap()
        .secondary
        .as_mut()
        .unwrap()
        .used_percent = 100.;
    assert_eq!(
        routing::select(&s, &BTreeMap::new(), 1000, &cfg),
        Some("b".into())
    );
    s.accounts
        .get_mut("b")
        .unwrap()
        .usage
        .as_mut()
        .unwrap()
        .primary
        .as_mut()
        .unwrap()
        .used_percent = 100.;
    assert_eq!(routing::select(&s, &BTreeMap::new(), 1000, &cfg), None);
}
#[test]
fn stale_or_elapsed_observation_never_qualifies() {
    let a = account(0., 2000);
    let cfg = RoutingConfig::default();
    assert!(routing::fresh(a.usage.as_ref().unwrap(), 1000, &cfg));
    assert!(!routing::fresh(a.usage.as_ref().unwrap(), 1400, &cfg));
    assert!(!routing::fresh(a.usage.as_ref().unwrap(), 999, &cfg));
    let mut u = a.usage.unwrap();
    u.primary.as_mut().unwrap().resets_at = Some(1000);
    assert!(!routing::fresh(&u, 1000, &cfg));
}
#[test]
fn registry_is_private_and_survives_atomic_updates() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths {
        data: d.path().join("data"),
        codex_home: d.path().join("home"),
    };
    let r = Registry::new(&paths);
    r.update(|s| {
        assert!(s.runs.is_empty());
        Ok(())
    })
    .unwrap();
    assert!(r.list().unwrap().is_empty());
}
fn setup() -> (tempfile::TempDir, Registry, State) {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths {
        data: d.path().join("data"),
        codex_home: d.path().join("home"),
    };
    let mut s = State::default();
    s.accounts.insert("a".into(), account(10., 10000));
    let mut b = account(10., 10000);
    b.email = "b@x".into();
    b.account_id = "b".into();
    s.accounts.insert("b".into(), b);
    (d, Registry::new(&paths), s)
}
#[test]
fn atomic_reservations_spread_parallel_launches_and_cleanup() {
    let (_d, r, s) = setup();
    let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let r = r.clone();
        let s = s.clone();
        let gate = gate.clone();
        workers.push(std::thread::spawn(move || {
            gate.wait();
            routing::reserve_prepared(
                r,
                &s,
                &RoutingConfig::default(),
                &routing::RunOptions::default(),
                None,
                None,
                cx::sessions::Mode::Interactive,
                1000,
            )
            .unwrap()
        }));
    }
    let a = workers.remove(0).join().unwrap();
    let b = workers.remove(0).join().unwrap();
    assert_ne!(
        a.get().unwrap().account.alias,
        b.get().unwrap().account.alias
    );
    assert_eq!(r.list().unwrap().len(), 2);
    drop(a);
    drop(b);
    assert!(r.list().unwrap().is_empty());
}
#[test]
fn exhausted_affinity_rebalances_but_pin_does_not() {
    let (_d, r, mut s) = setup();
    let cfg = RoutingConfig::default();
    let options = routing::RunOptions::default();
    r.update(|v| {
        v.bindings.insert(
            "root".into(),
            cx::sessions::Binding {
                account: cx::sessions::Identity::new("a", &s.accounts["a"]),
                last_prompt: 999,
            },
        );
        Ok(())
    })
    .unwrap();
    s.accounts
        .get_mut("a")
        .unwrap()
        .usage
        .as_mut()
        .unwrap()
        .secondary
        .as_mut()
        .unwrap()
        .used_percent = 100.;
    let run = routing::reserve_prepared(
        r.clone(),
        &s,
        &cfg,
        &options,
        None,
        Some("root"),
        cx::sessions::Mode::Interactive,
        1000,
    )
    .unwrap();
    assert_eq!(run.get().unwrap().account.alias, "b");
    drop(run);
    assert!(routing::reserve_prepared(
        r,
        &s,
        &cfg,
        &options,
        Some("a"),
        Some("root"),
        cx::sessions::Mode::Interactive,
        1000
    )
    .is_err());
}
#[test]
fn recent_affinity_respects_identity_and_pin_cap() {
    let (_d, r, mut s) = setup();
    let cfg = RoutingConfig {
        max_headless_per_account: 1,
        ..Default::default()
    };
    let options = routing::RunOptions::default();
    r.update(|v| {
        v.bindings.insert(
            "root".into(),
            cx::sessions::Binding {
                account: cx::sessions::Identity::new("b", &s.accounts["b"]),
                last_prompt: 999,
            },
        );
        Ok(())
    })
    .unwrap();
    let run = routing::reserve_prepared(
        r.clone(),
        &s,
        &cfg,
        &options,
        None,
        Some("root"),
        cx::sessions::Mode::Headless,
        1000,
    )
    .unwrap();
    assert_eq!(run.get().unwrap().account.alias, "b");
    assert!(routing::reserve_prepared(
        r.clone(),
        &s,
        &cfg,
        &options,
        Some("b"),
        None,
        cx::sessions::Mode::Headless,
        1000
    )
    .is_err());
    drop(run);
    s.accounts.get_mut("b").unwrap().email = "replacement@x".into();
    let run = routing::reserve_prepared(
        r,
        &s,
        &cfg,
        &options,
        None,
        Some("root"),
        cx::sessions::Mode::Headless,
        1000,
    )
    .unwrap();
    assert_eq!(run.get().unwrap().account.alias, "a");
}
#[test]
fn reused_process_identity_is_removed() {
    let (_d, r, s) = setup();
    let run = routing::reserve_prepared(
        r.clone(),
        &s,
        &RoutingConfig::default(),
        &routing::RunOptions::default(),
        None,
        None,
        cx::sessions::Mode::Interactive,
        1000,
    )
    .unwrap();
    r.update(|s| {
        s.runs
            .get_mut(&run.id)
            .unwrap()
            .process
            .start
            .push_str("-reused");
        Ok(())
    })
    .unwrap();
    assert!(r.list().unwrap().is_empty());
}
#[test]
fn revoked_logins_are_never_selected_while_another_is_usable() {
    let (_d, r, mut s) = setup();
    let cfg = RoutingConfig::default();
    s.accounts.get_mut("a").unwrap().login_revoked_at = Some(900);
    assert_eq!(
        routing::select(&s, &BTreeMap::new(), 1000, &cfg),
        Some("b".into())
    );
    // Stale caches: the fallback used to be alphabetical, which picked the revoked "a".
    assert_eq!(
        routing::select_local(&s, &BTreeMap::from([("b".into(), 3)]), 5000, &cfg),
        Some("b".into())
    );
    let run = routing::reserve_prepared(
        r.clone(),
        &s,
        &cfg,
        &routing::RunOptions::default(),
        None,
        None,
        cx::sessions::Mode::Interactive,
        1000,
    )
    .unwrap();
    assert_eq!(run.get().unwrap().account.alias, "b");
    drop(run);
    s.accounts.get_mut("b").unwrap().login_revoked_at = Some(900);
    assert_eq!(routing::select(&s, &BTreeMap::new(), 1000, &cfg), None);
    assert_eq!(
        routing::select_local(&s, &BTreeMap::new(), 5000, &cfg),
        None
    );
}
#[test]
fn pin_to_revoked_login_fails_fast_with_the_reauth_command() {
    let (_d, r, mut s) = setup();
    s.accounts.get_mut("a").unwrap().login_revoked_at = Some(900);
    let error = routing::reserve_prepared(
        r,
        &s,
        &RoutingConfig::default(),
        &routing::RunOptions::default(),
        Some("a"),
        None,
        cx::sessions::Mode::Interactive,
        1000,
    )
    .err()
    .unwrap();
    let failure = error.downcast_ref::<routing::Failure>().unwrap();
    assert_eq!(failure.code, 21);
    assert!(
        failure.message.contains("cx add a --device-auth --force"),
        "{}",
        failure.message
    );
}
#[test]
fn stale_limited_account_is_skipped_until_its_reset_passes() {
    let (_d, _r, mut s) = setup();
    let cfg = RoutingConfig::default();
    // "a" reported 7d 100% long ago (observed_at 1000); its reset is still ahead at 10000.
    let a = s.accounts.get_mut("a").unwrap().usage.as_mut().unwrap();
    a.secondary.as_mut().unwrap().used_percent = 100.;
    let stale = 5000;
    assert_eq!(
        routing::select_local(&s, &BTreeMap::from([("b".into(), 9)]), stale, &cfg),
        Some("b".into())
    );
    // allowed=false alone also counts as limited until a reported reset.
    let mut denied = s.clone();
    let u = denied
        .accounts
        .get_mut("a")
        .unwrap()
        .usage
        .as_mut()
        .unwrap();
    u.secondary.as_mut().unwrap().used_percent = 50.;
    u.allowed = false;
    assert_eq!(
        routing::select_local(&denied, &BTreeMap::from([("b".into(), 9)]), stale, &cfg),
        Some("b".into())
    );
    // After every reported reset, "a" is just unknown again and pickable.
    assert_eq!(
        routing::select_local(&s, &BTreeMap::from([("b".into(), 9)]), 10001, &cfg),
        Some("a".into())
    );
}
#[test]
fn all_limited_or_revoked_names_each_account_and_why() {
    let (_d, _r, mut s) = setup();
    s.accounts
        .get_mut("a")
        .unwrap()
        .usage
        .as_mut()
        .unwrap()
        .primary
        .as_mut()
        .unwrap()
        .used_percent = 100.;
    assert!(
        routing::unusable_failure(&s, 1500).is_none(),
        "b is still usable"
    );
    s.accounts.get_mut("b").unwrap().login_revoked_at = Some(900);
    assert_eq!(
        routing::select_local(&s, &BTreeMap::new(), 1500, &cfg()),
        None
    );
    // Past the primary reset (2000), "a" is unknown again rather than limited.
    assert!(routing::unusable_failure(&s, 2001).is_none());
    let error = routing::unusable_failure(&s, 1500).unwrap();
    let failure = error.downcast_ref::<routing::Failure>().unwrap();
    assert_eq!(failure.code, 20);
    assert!(
        failure
            .message
            .starts_with("no usable account: a limited until ")
            && failure
                .message
                .ends_with(", b revoked (run `cx add b --device-auth --force`)"),
        "{}",
        failure.message
    );
}
fn cfg() -> RoutingConfig {
    RoutingConfig::default()
}
