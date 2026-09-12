use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::usage::Observation;

#[derive(Clone, Debug)]
pub struct Paths {
    pub data: PathBuf,
    pub codex_home: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let home = dirs::home_dir().context("cannot determine home directory")?;
        let data = env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
            .join("cx");
        let codex_home = env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        Ok(Self { data, codex_home })
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Account {
    pub email: String,
    pub account_id: String,
    pub data: serde_json::Value,
    #[serde(default)]
    pub usage: Option<Observation>,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("email", &self.email)
            .field("account_id", &self.account_id)
            .field("data", &"<redacted>")
            .field("usage", &self.usage)
            .finish()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub current: Option<String>,
    #[serde(default)]
    pub previous: Option<String>,
    #[serde(default)]
    pub accounts: BTreeMap<String, Account>,
}

impl State {
    pub fn add(&mut self, alias: &str, account: Account, force: bool) -> Result<()> {
        validate_alias(alias)?;
        validate_account(&account)?;
        let collision = self.accounts.iter().find_map(|(existing_alias, existing)| {
            (existing.account_id == account.account_id
                && existing.email == account.email
                && existing_alias != alias)
                .then_some(existing_alias)
        });
        if let Some(existing) = collision {
            bail!("account identity is already stored as alias '{existing}'");
        }
        if self.accounts.contains_key(alias) && !force {
            bail!("alias '{alias}' already exists");
        }
        self.accounts.insert(alias.to_owned(), account);
        Ok(())
    }

    pub fn select(&mut self, alias: &str) -> Result<String> {
        let selected = if alias == "-" {
            self.previous.clone().context("no previous account")?
        } else {
            alias.to_owned()
        };
        if !self.accounts.contains_key(&selected) {
            bail!("unknown account alias '{selected}'");
        }
        if self.current.as_deref() != Some(&selected) {
            self.previous = self.current.replace(selected.clone());
        }
        Ok(selected)
    }

    pub fn delete(&mut self, alias: &str) -> Result<()> {
        if self.accounts.remove(alias).is_none() {
            bail!("unknown account alias '{alias}'");
        }
        if self.current.as_deref() == Some(alias) {
            self.current = None;
        }
        if self.previous.as_deref() == Some(alias) {
            self.previous = None;
        }
        Ok(())
    }
}

pub fn validate_alias(alias: &str) -> Result<()> {
    if alias.is_empty()
        || matches!(alias, "-" | "next" | "." | "..")
        || !alias
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        bail!("invalid alias '{alias}'; use letters, digits, '.', '-' or '_'");
    }
    Ok(())
}

fn validate_account(account: &Account) -> Result<()> {
    if account.email.trim().is_empty() {
        bail!("account email is missing");
    }
    if account.account_id.trim().is_empty() {
        bail!("account workspace identity is missing");
    }
    if !account.data.is_object() {
        bail!("account credentials are malformed");
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Store {
    pub paths: Paths,
}

impl Store {
    pub fn new(paths: Paths) -> Self {
        Self { paths }
    }

    pub fn read(&self) -> Result<State> {
        let path = self.paths.data.join("state.json");
        match fs::read(&path) {
            Ok(bytes) => {
                // Serde's typed errors can quote a credential placed in the wrong field.
                let state: State = serde_json::from_slice(&bytes).map_err(|_| {
                    anyhow::anyhow!("stored cx state is corrupt: {}", path.display())
                })?;
                validate_state(&state)
                    .with_context(|| format!("stored cx state is invalid: {}", path.display()))?;
                Ok(state)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    pub fn transaction<T>(&self, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        fs::create_dir_all(&self.paths.data)
            .with_context(|| format!("cannot create {}", self.paths.data.display()))?;
        set_private_dir(&self.paths.data)?;
        let lock_path = self.paths.data.join("state.lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        lock.lock_exclusive()?;
        let mut state = self.read()?;
        let result = f(&mut state)?;
        validate_state(&state)?;
        write_state(&self.paths.data, &state)?;
        FileExt::unlock(&lock)?;
        Ok(result)
    }
}

fn validate_state(state: &State) -> Result<()> {
    let mut identities = std::collections::BTreeSet::new();
    for (alias, account) in &state.accounts {
        validate_alias(alias)?;
        validate_account(account)?;
        if !identities.insert((&account.email, &account.account_id)) {
            bail!("duplicate account user and workspace identity");
        }
    }
    for (name, selected) in [("current", &state.current), ("previous", &state.previous)] {
        if selected
            .as_ref()
            .is_some_and(|alias| !state.accounts.contains_key(alias))
        {
            bail!("{name} selection references an unknown alias");
        }
    }
    Ok(())
}

fn write_state(directory: &std::path::Path, state: &State) -> Result<()> {
    let target = directory.join("state.json");
    let mut temp = tempfile::NamedTempFile::new_in(directory)?;
    temp.as_file_mut()
        .write_all(&serde_json::to_vec_pretty(state)?)?;
    temp.as_file_mut().write_all(b"\n")?;
    temp.as_file_mut().sync_all()?;
    set_private_file(temp.as_file())?;
    temp.persist(&target).map_err(|error| error.error)?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn set_private_dir(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_dir(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file(file: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file(_file: &File) -> Result<()> {
    Ok(())
}
