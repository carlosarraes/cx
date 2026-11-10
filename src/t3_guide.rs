//! Live `cx --t3` guide: how the t3 shim works, instance ids on this
//! machine, how to pin one, and which account aliases are available.

use cx::state::{Paths, Store};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub fn print() {
    print!("{}", guide());
}

struct Instance {
    id: String,
    driver: Option<String>,
    enabled: Option<bool>,
    pin: Option<String>,
}

fn guide() -> String {
    let path = t3_settings_path();
    let instances = read_instances(&path);
    let mut out = String::new();
    out.push_str(HOW_IT_WORKS);
    out.push_str(&instances_section(&path, &instances));
    out.push_str(&pin_section(example_instance(&instances, "codex", "codex")));
    out.push_str(&aliases_section(&local_aliases(), &borrowed_aliases()));
    out.push_str(INVALID_PIN);
    out
}

const HOW_IT_WORKS: &str = "\
cx and t3
=========

How the shim works
------------------
t3 starts Codex by exec'ing a provider instance's binaryPath, usually
~/.local/bin/cx-t3-codex. That path is a symlink whose argv0 is
cx-t3-codex, so cx runs its t3-codex path: pick one saved or borrowed
account and exec Codex with the original arguments and stdio. t3 tracks
and kills the pid it started, so the shim must exec rather than
spawn-and-supervise.

Borrowed accounts: set CX_BORROW=<host> on the t3 instance's environment
(and CX_BORROW_JUMP if the owner is reached through another hop). Fill
the cache with `cx borrow refresh --from <host>`. Spawns read that cache
and do not SSH while a token is still valid. Codex cannot refresh a
borrowed token, so an app-server that outlives the access token will 401
until the next spawn.

Pin an account with CX_PIN=<alias> (empty means off). With CX_BORROW set,
the pin must name a cached borrowed alias. An unknown or expired pin fails
without falling back to another account or the local login.

";

const INVALID_PIN: &str = "\
Invalid account pin
-------------------
`cx run --account NAME` pins a cx alias or unique email. If NAME is
unknown, cx exits with \"unknown account; use cx list\" and never starts
Codex. There is no fallback to another account.
";

fn t3_settings_path() -> PathBuf {
    t3_root().join("userdata/settings.json")
}

fn t3_root() -> PathBuf {
    if let Some(root) = std::env::var_os("T3CODE_HOME") {
        PathBuf::from(root)
    } else if let Some(home) = dirs::home_dir() {
        home.join(".t3")
    } else {
        PathBuf::from(".t3")
    }
}

fn read_instances(path: &Path) -> Result<Vec<Instance>, String> {
    if !path.exists() {
        return Err(format!("No t3 settings file at {}.", path.display()));
    }
    let bytes = fs::read(path).map_err(|e| format!("Could not read {}: {e}", path.display()))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| format!("Could not parse {} as JSON.", path.display()))?;
    let Some(map) = value.get("providerInstances").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    Ok(map
        .iter()
        .map(|(id, inst)| Instance {
            id: id.clone(),
            driver: inst
                .get("driver")
                .and_then(Value::as_str)
                .map(str::to_owned),
            enabled: inst.get("enabled").and_then(Value::as_bool),
            pin: env_value(inst, "CX_PIN"),
        })
        .collect())
}

fn env_value(inst: &Value, name: &str) -> Option<String> {
    inst.get("environment")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|entry| {
            if entry.get("name").and_then(Value::as_str) != Some(name) {
                return None;
            }
            entry
                .get("value")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
}

fn example_instance(
    instances: &Result<Vec<Instance>, String>,
    preferred_id: &str,
    preferred_driver: &str,
) -> String {
    if let Ok(list) = instances {
        if let Some(inst) = list.iter().find(|inst| {
            inst.id == preferred_id || inst.driver.as_deref() == Some(preferred_driver)
        }) {
            return inst.id.clone();
        }
        if let Some(inst) = list.first() {
            return inst.id.clone();
        }
    }
    preferred_id.to_string()
}

fn instances_section(path: &Path, instances: &Result<Vec<Instance>, String>) -> String {
    let mut out = String::from("t3 instances on this machine\n----------------------------\n");
    match instances {
        Ok(list) if list.is_empty() => {
            out.push_str(&format!(
                "No providerInstances found in {}.\n\n",
                path.display()
            ));
        }
        Ok(list) => {
            out.push_str(&format!("Read from {}:\n", path.display()));
            for inst in list {
                let driver = inst.driver.as_deref().unwrap_or("?");
                let enabled = match inst.enabled {
                    Some(true) => "enabled",
                    Some(false) => "disabled",
                    None => "enabled=?",
                };
                match &inst.pin {
                    Some(pin) => out.push_str(&format!(
                        "  {}  driver={}  {}  pin={pin}\n",
                        inst.id, driver, enabled
                    )),
                    None => {
                        out.push_str(&format!("  {}  driver={}  {}\n", inst.id, driver, enabled))
                    }
                }
            }
            out.push('\n');
        }
        Err(msg) => {
            out.push_str(msg);
            out.push_str("\n\n");
        }
    }
    out
}

fn pin_section(example_id: String) -> String {
    format!(
        "\
Pinning an instance
-------------------
Pass modelSelection.instanceId in t3_thread_launch. Example:

  t3_thread_launch
    title: \"forja: example\"
    projectId: \"<project-id>\"
    modelSelection: {{ instanceId: \"{example_id}\", model: \"<model>\" }}
    message: \"...\"

A dedicated instance can pin one cx alias with CX_PIN=<alias> on that
instance (listed as pin= above). Empty CX_PIN means off, so the default
instance keeps auto-rotating.

If instanceId is omitted, t3 inherits the caller's instance. If it does
not match any provider instance on this machine, t3 rejects the launch
(\"No provider instance registered for id '...'\") and does not start a
thread. It does not fall back to another instance.

"
    )
}

fn aliases_section(local: &[String], borrowed: &[(String, Vec<String>)]) -> String {
    let mut out =
        String::from("Account aliases on this machine\n-------------------------------\n");
    if local.is_empty() && borrowed.is_empty() {
        out.push_str("No saved or borrowed aliases found.\n\n");
        out.push_str(
            "These are cx account aliases, not t3 instance ids. The shim picks among them.\n\n",
        );
        return out;
    }
    if local.is_empty() {
        out.push_str("Local: (none)\n");
    } else {
        out.push_str(&format!("Local: {}\n", local.join(", ")));
    }
    if borrowed.is_empty() {
        out.push_str("Borrowed: (none)\n");
    } else {
        for (host, aliases) in borrowed {
            out.push_str(&format!("Borrowed from {host}: {}\n", aliases.join(", ")));
        }
    }
    out.push_str(
        "\nThese are cx account aliases, not t3 instance ids. The shim picks among them\n\
(least-recently-used valid token when CX_BORROW is set).\n\n",
    );
    out
}

fn local_aliases() -> Vec<String> {
    Paths::discover()
        .ok()
        .and_then(|paths| Store::new(paths).read().ok())
        .map(|state| state.accounts.keys().cloned().collect())
        .unwrap_or_default()
}

fn borrowed_aliases() -> Vec<(String, Vec<String>)> {
    let Ok(paths) = Paths::discover() else {
        return Vec::new();
    };
    object_keys_in_dir(&paths.data.join("borrow"), "accounts")
}

fn object_keys_in_dir(dir: &Path, field: &str) -> Vec<(String, Vec<String>)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json") && path.is_file() {
            let host = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let aliases = json_object_keys(&path, field);
            if !aliases.is_empty() {
                out.push((host, aliases));
            }
        }
    }
    out.sort();
    out
}

fn json_object_keys(path: &Path, field: &str) -> Vec<String> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return Vec::new();
    };
    value
        .get(field)
        .and_then(Value::as_object)
        .map(|object| object.keys().cloned().collect())
        .unwrap_or_default()
}
