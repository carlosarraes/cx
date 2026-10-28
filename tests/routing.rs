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
