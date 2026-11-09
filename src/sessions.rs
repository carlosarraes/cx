use crate::state::{Account, Paths};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub start: String,
}
impl Process {
    pub fn current() -> Result<Self> {
        Self::read(std::process::id()).context("cannot identify launcher process")
    }
    pub fn read(pid: u32) -> Option<Self> {
        process_start(pid).map(|start| Self { pid, start })
    }
    pub fn alive(&self) -> bool {
        if self.pid == 0 || self.pid > i32::MAX as u32 {
            return false;
        }
        match Self::read(self.pid) {
            Some(p) => p == *self,
            None => unsafe {
                libc::kill(self.pid as i32, 0) == 0
                    || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
            },
        }
    }
}
#[cfg(target_os = "linux")]
fn process_start(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Some(format!("{}:{}", boot.trim(), fields.get(19)?))
}
#[cfg(target_os = "macos")]
fn process_start(pid: u32) -> Option<String> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    // The kernel fills this fixed-size structure; only read it after a complete write.
    let n = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size as i32,
        )
    };
    if n != size as i32 {
        return None;
    }
    let info = unsafe { info.assume_init() };
    Some(format!(
        "{}:{}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    ))
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Identity {
    pub alias: String,
    pub email: String,
    pub account_id: String,
}
impl Identity {
    pub fn new(alias: &str, a: &Account) -> Self {
        Self {
            alias: alias.into(),
            email: a.email.clone(),
            account_id: a.account_id.clone(),
        }
    }
    pub fn matches(&self, a: &Account) -> bool {
        self.email == a.email && self.account_id == a.account_id
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Interactive,
    Headless,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ManagedSession {
    #[serde(default)]
    pub expires_at: Option<i64>,
    pub id: String,
    pub process: Process,
    pub account: Identity,
    pub mode: Mode,
    pub pinned: bool,
    pub label: Option<String>,
    pub started_at: i64,
    pub threads: Vec<String>,
    pub pending: Option<Identity>,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Binding {
    pub account: Identity,
    pub last_prompt: i64,
}
#[derive(Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub runs: BTreeMap<String, ManagedSession>,
    #[serde(default)]
    pub bindings: BTreeMap<String, Binding>,
}
#[derive(Clone)]
pub struct Registry {
    pub path: PathBuf,
}
impl Registry {
    pub fn new(paths: &Paths) -> Self {
        Self {
            path: paths.data.join("sessions.json"),
        }
    }
    pub fn update<T>(&self, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let dir = self.path.parent().context("missing registry directory")?;
        fs::create_dir_all(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(dir.join("sessions.lock"))?;
        lock.lock_exclusive()?;
        let mut s: State = match fs::read(&self.path) {
            Ok(b) => serde_json::from_slice(&b)
                .map_err(|_| anyhow::anyhow!("invalid session registry"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e.into()),
        };
        let before = serde_json::to_vec(&s)?;
        s.runs.retain(|_, r| {
            r.expires_at
                .map_or_else(|| r.process.alive(), |t| chrono::Utc::now().timestamp() < t)
        });
        let result = f(&mut s)?;
        let bytes = serde_json::to_vec(&s)?;
        if bytes != before {
            let mut temp = tempfile::NamedTempFile::new_in(dir)?;
            temp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
            temp.write_all(&bytes)?;
            temp.as_file().sync_all()?;
            temp.persist(&self.path).map_err(|e| e.error)?;
        }
        Ok(result)
    }
    pub fn list(&self) -> Result<Vec<ManagedSession>> {
        self.update(|s| Ok(s.runs.values().cloned().collect()))
    }
}
pub struct Reservation {
    pub registry: Registry,
    pub id: String,
}
impl Reservation {
    pub fn get(&self) -> Result<ManagedSession> {
        self.registry.update(|s| {
            s.runs
                .get(&self.id)
                .cloned()
                .context("session reservation disappeared")
        })
    }
    pub fn attach(&self, thread: &str, prompt: bool) -> Result<()> {
        self.registry.update(|s| {
            if s.runs
                .iter()
                .any(|(id, r)| id != &self.id && r.threads.iter().any(|t| t == thread))
            {
                bail!("conversation already running in another managed session");
            }
            let r = s.runs.get_mut(&self.id).context("missing reservation")?;
            if !r.threads.iter().any(|t| t == thread) {
                r.threads.push(thread.into());
            }
            let last = s.bindings.get(thread).map_or(0, |b| b.last_prompt);
            s.bindings.insert(
                thread.into(),
                Binding {
                    account: r.account.clone(),
                    last_prompt: if prompt {
                        chrono::Utc::now().timestamp()
                    } else {
                        last
                    },
                },
            );
            Ok(())
        })
    }
    pub fn set_account(&self, account: Identity) -> Result<()> {
        self.registry.update(|s| {
            let r = s.runs.get_mut(&self.id).context("missing reservation")?;
            r.account = account.clone();
            r.pending = None;
            r.error = None;
            for t in &r.threads {
                if let Some(b) = s.bindings.get_mut(t) {
                    b.account = account.clone();
                }
            }
            Ok(())
        })
    }
    pub fn status(&self, pending: Option<Identity>, error: Option<String>) -> Result<()> {
        self.registry.update(|s| {
            let r = s.runs.get_mut(&self.id).context("missing reservation")?;
            r.pending = pending;
            r.error = error;
            Ok(())
        })
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let _ = self.registry.update(|s| {
            if s.runs.get(&self.id).is_none_or(|r| r.expires_at.is_none()) {
                s.runs.remove(&self.id);
            }
            Ok(())
        });
    }
}
