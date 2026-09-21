use anyhow::{bail, ensure, Context, Result};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use tokio::net::UnixStream;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessEvidence {
    pub pid: u32,
    pub parent: u32,
    pub uid: u32,
    pub boot_id: String,
    pub start_marker: u64,
    pub executable: PathBuf,
}

impl ProcessEvidence {
    #[cfg(target_os = "linux")]
    pub fn read(pid: u32) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;

        ensure!(pid > 0 && pid <= i32::MAX as u32, "invalid process ID");
        let directory = PathBuf::from(format!("/proc/{pid}"));
        let first = std::fs::read_to_string(directory.join("stat"))?;
        let (parent, start_marker) = parse_linux_stat(&first)?;
        let executable = std::fs::read_link(directory.join("exe"))?;
        ensure!(
            executable.is_absolute(),
            "process executable is not absolute"
        );
        let uid = std::fs::metadata(&directory)?.uid();
        ensure!(
            uid == unsafe { libc::geteuid() },
            "process belongs to another user"
        );
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .to_owned();
        uuid::Uuid::parse_str(&boot_id).context("invalid boot ID")?;

        let second = std::fs::read_to_string(directory.join("stat"))?;
        let after = parse_linux_stat(&second)?;
        ensure!(
            after == (parent, start_marker),
            "process changed during validation"
        );

        Ok(Self {
            pid,
            parent,
            uid,
            boot_id,
            start_marker,
            executable,
        })
    }

    #[cfg(target_os = "macos")]
    pub fn read(pid: u32) -> Result<Self> {
        use std::ffi::CStr;
        use std::os::unix::ffi::OsStringExt;

        ensure!(pid > 0 && pid <= i32::MAX as u32, "invalid process ID");
        let mut raw: RawSnapshot = unsafe { std::mem::zeroed() };
        if unsafe { cx_process_snapshot(pid as libc::c_int, &mut raw) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        ensure!(raw.pid == pid, "process ID changed during validation");
        ensure!(
            raw.parent > 0 && raw.parent != pid,
            "invalid process parent"
        );
        ensure!(raw.start_usec > 0, "missing process lifetime evidence");
        ensure!(
            raw.uid == unsafe { libc::geteuid() },
            "process belongs to another user"
        );
        let executable = PathBuf::from(std::ffi::OsString::from_vec(
            CStr::from_bytes_until_nul(&raw.executable)?
                .to_bytes()
                .to_vec(),
        ));
        ensure!(
            executable.is_absolute(),
            "process executable is not absolute"
        );
        let boot_id =
            uuid::Uuid::parse_str(CStr::from_bytes_until_nul(&raw.boot_id)?.to_str()?)?.to_string();
        Ok(Self {
            pid,
            parent: raw.parent,
            uid: raw.uid,
            boot_id,
            start_marker: raw.start_usec,
            executable,
        })
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(Self::read(self.pid)? == *self, "process lifetime changed");
        Ok(())
    }

    pub fn validate_descendant(&self, mut pid: u32) -> Result<()> {
        self.validate()?;
        for _ in 0..64 {
            if pid == self.pid {
                return self.validate();
            }
            ensure!(pid > 1, "process is outside the expected ancestry");
            let observed = Self::read(pid)?;
            ensure!(observed.parent != pid, "invalid process ancestry");
            pid = observed.parent;
        }
        bail!("process ancestry exceeds validation bound")
    }

    #[cfg(test)]
    fn change_start_marker_for_test(&mut self) {
        self.start_marker = self.start_marker.saturating_add(1);
    }
}

#[cfg(target_os = "linux")]
fn parse_linux_stat(stat: &str) -> Result<(u32, u64)> {
    let (_, fields) = stat.rsplit_once(") ").context("invalid process stat")?;
    let fields: Vec<_> = fields.split_whitespace().collect();
    ensure!(fields.len() >= 20, "incomplete process stat");
    ensure!(!matches!(fields[0], "Z" | "X" | "x"), "process has ended");
    let parent = fields[1].parse()?;
    let start = fields[19].parse()?;
    ensure!(parent > 0 && start > 0, "missing process lifetime evidence");
    Ok((parent, start))
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct RawSnapshot {
    start_usec: u64,
    pid: u32,
    parent: u32,
    uid: u32,
    executable: [u8; 4096],
    boot_id: [u8; 40],
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn cx_process_snapshot(pid: libc::c_int, out: *mut RawSnapshot) -> libc::c_int;
}

#[cfg(target_os = "linux")]
pub fn peer_pid(stream: &UnixStream) -> Result<u32> {
    let mut credential: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credential as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    ensure!(
        result == 0
            && length as usize == std::mem::size_of::<libc::ucred>()
            && credential.pid > 0
            && credential.uid == unsafe { libc::geteuid() },
        "cannot validate relay peer process"
    );
    Ok(u32::try_from(credential.pid)?)
}

#[cfg(target_os = "macos")]
pub fn peer_pid(stream: &UnixStream) -> Result<u32> {
    let mut uid = 0;
    let mut gid = 0;
    ensure!(
        unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0
            && uid == unsafe { libc::geteuid() },
        "cannot validate relay peer owner"
    );
    let mut pid: libc::pid_t = 0;
    let mut length = std::mem::size_of_val(&pid) as libc::socklen_t;
    ensure!(
        unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                0,
                2,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut length,
            )
        } == 0
            && length as usize == std::mem::size_of_val(&pid)
            && pid > 0,
        "cannot validate relay peer process"
    );
    Ok(u32::try_from(pid)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};
    use tokio::net::UnixStream;

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn process_evidence_rejects_unrelated_and_changed_lifetimes() {
        let child = ChildGuard(Command::new("/bin/sleep").arg("30").spawn().unwrap());
        let current = ProcessEvidence::read(std::process::id()).unwrap();
        let child_evidence = ProcessEvidence::read(child.0.id()).unwrap();

        current.validate_descendant(child.0.id()).unwrap();
        assert!(child_evidence
            .validate_descendant(std::process::id())
            .is_err());

        let mut changed = current.clone();
        changed.change_start_marker_for_test();
        assert!(changed.validate().is_err());
    }

    #[tokio::test]
    async fn peer_pid_comes_from_the_kernel_socket() {
        let (first, second) = UnixStream::pair().unwrap();
        assert_eq!(peer_pid(&first).unwrap(), std::process::id());
        assert_eq!(peer_pid(&second).unwrap(), std::process::id());
    }
}
