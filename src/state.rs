//! PID records are evidence, never authority for killing or adopting a process.
use crate::{
    config::{now, private_directory, random_id},
    protocol::valid_id,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::Write,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub session_id: String,
    pub target_id: String,
    pub incarnation: String,
    pub job_id: String,
    pub request_id: String,
    pub pid: u32,
    pub pgid: u32,
    pub boot_id: Option<String>,
    pub process_start_ticks: Option<u64>,
    pub state: String,
    pub exit_code: Option<i32>,
    pub termination_reason: Option<String>,
    pub updated_at: u64,
}
pub struct Store {
    dir: PathBuf,
    // Holding this descriptor keeps the advisory state-directory lock for the
    // lifetime of the Store. PID records are not used to recover that lock.
    _lock: File,
    #[cfg(test)]
    fail_next_write: std::sync::atomic::AtomicBool,
}
impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        if !dir.exists() {
            fs::DirBuilder::new().mode(0o700).create(dir)?;
        }
        private_directory(dir)?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("connector.lock"))?;
        let lock_meta = lock.metadata()?;
        ensure!(
            lock_meta.is_file()
                && lock_meta.uid() == unsafe { libc::geteuid() }
                && lock_meta.mode() & 0o077 == 0,
            "connector lock must be a private file owned by this user",
        );
        // SAFETY: this is an owned valid file descriptor; flock does not access memory.
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "state directory is already in use by another Connector"
        );
        Ok(Self {
            dir: dir.into(),
            _lock: lock,
            #[cfg(test)]
            fail_next_write: std::sync::atomic::AtomicBool::new(false),
        })
    }
    pub fn write(&self, record: &Record) -> Result<()> {
        ensure!(valid_id(&record.job_id), "invalid job ID in PID record");
        #[cfg(test)]
        if self
            .fail_next_write
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            anyhow::bail!("injected PID record write failure");
        }

        let encoded = serde_json::to_vec_pretty(record)?;
        ensure!(encoded.len() <= 16 * 1024, "PID record is too large");
        let tmp = self.dir.join(format!(".{}.tmp", random_id()));
        let result: Result<()> = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(&encoded)?;
            file.sync_all()?;
            fs::rename(&tmp, self.dir.join(format!("{}.json", record.job_id)))?;
            File::open(&self.dir)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    #[cfg(test)]
    pub(crate) fn fail_next_write_for_test(&self) {
        self.fail_next_write
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
pub fn boot_id() -> Option<String> {
    let value = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}
pub fn start_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_start_ticks(&stat)
}
fn parse_start_ticks(stat: &str) -> Option<u64> {
    // comm may contain spaces and ')' characters. Fields after its FINAL ')' start at field 3.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcObservation {
    NotRunning,
    Running(u64),
    Unknown,
}
fn observe_start_ticks(pid: u32) -> ProcObservation {
    #[cfg(target_os = "linux")]
    {
        let path = format!("/proc/{pid}/stat");
        match fs::read_to_string(path) {
            Ok(stat) => {
                parse_start_ticks(&stat).map_or(ProcObservation::Unknown, ProcObservation::Running)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ProcObservation::NotRunning
            }
            Err(_) => ProcObservation::Unknown,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        ProcObservation::Unknown
    }
}

fn identity_status(record: &Record, current_boot: Option<&str>) -> (&'static str, Option<bool>) {
    match observe_start_ticks(record.pid) {
        ProcObservation::NotRunning => ("not_running", Some(false)),
        ProcObservation::Unknown => ("unknown", None),
        ProcObservation::Running(observed_ticks) => match (
            record.boot_id.as_deref(),
            current_boot,
            record.process_start_ticks,
        ) {
            (Some(record_boot), Some(current_boot), Some(record_ticks)) => {
                let matches = record_boot == current_boot && record_ticks == observed_ticks;
                (if matches { "matches" } else { "mismatch" }, Some(matches))
            }
            _ => ("unknown", None),
        },
    }
}

pub fn inspect(dir: &Path) -> Result<serde_json::Value> {
    private_directory(dir)?;
    let current_boot = boot_id();
    let mut records = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_none_or(|s| s != "json") {
            continue;
        }
        let record: Record = crate::config::load(&path).context("read PID record")?;
        let (status, matches) = identity_status(&record, current_boot.as_deref());
        records.push(serde_json::json!({
            "record": record,
            // Kept for compatibility: true/false remains the complete identity
            // comparison, while null means that the comparison was unavailable.
            "process_identity_matches": matches,
            "process_identity_status": status,
            "observed_at": now(),
            "warning": "Records may be stale after a crash. No process is automatically adopted or killed.",
        }));
    }
    Ok(serde_json::json!({"records": records}))
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn temp_state_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!("agent-tunnel-state-{}", random_id()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    fn record(pid: u32, boot: Option<String>, ticks: Option<u64>) -> Record {
        Record {
            session_id: "session".into(),
            target_id: "target".into(),
            incarnation: "incarnation".into(),
            job_id: random_id(),
            request_id: "request".into(),
            pid,
            pgid: pid,
            boot_id: boot,
            process_start_ticks: ticks,
            state: "running".into(),
            exit_code: None,
            termination_reason: None,
            updated_at: now(),
        }
    }
    fn only_inspection(value: serde_json::Value) -> serde_json::Value {
        value["records"]
            .as_array()
            .unwrap()
            .first()
            .unwrap()
            .clone()
    }

    #[test]
    fn parses_proc_comm_with_parentheses() {
        let line = format!(
            "123 (a weird ) name) S {} 98765 0",
            (4..22).map(|_| "1").collect::<Vec<_>>().join(" ")
        );
        assert_eq!(parse_start_ticks(&line), Some(98765));
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn current_process_identity() {
        assert!(start_ticks(std::process::id()).is_some());
        assert!(boot_id().is_some());
    }
    #[test]
    fn store_uses_an_exclusive_directory_lock() {
        let dir = temp_state_dir();
        let first = Store::open(&dir).unwrap();
        assert!(Store::open(&dir).is_err());
        drop(first);
        assert!(Store::open(&dir).is_ok());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn inspect_distinguishes_missing_pid() {
        let dir = temp_state_dir();
        let store = Store::open(&dir).unwrap();
        store.write(&record(u32::MAX, boot_id(), Some(1))).unwrap();
        let inspection = only_inspection(inspect(&dir).unwrap());
        assert_eq!(inspection["process_identity_status"], "not_running");
        assert_eq!(inspection["process_identity_matches"], false);
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn inspect_reports_matching_identity() {
        let dir = temp_state_dir();
        let store = Store::open(&dir).unwrap();
        let pid = std::process::id();
        store
            .write(&record(pid, boot_id(), start_ticks(pid)))
            .unwrap();
        let inspection = only_inspection(inspect(&dir).unwrap());
        assert_eq!(inspection["process_identity_status"], "matches");
        assert_eq!(inspection["process_identity_matches"], true);
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn inspect_reports_unknown_when_identity_data_is_missing() {
        let dir = temp_state_dir();
        let store = Store::open(&dir).unwrap();
        let pid = std::process::id();
        store.write(&record(pid, None, start_ticks(pid))).unwrap();
        let inspection = only_inspection(inspect(&dir).unwrap());
        assert_eq!(inspection["process_identity_status"], "unknown");
        assert!(inspection["process_identity_matches"].is_null());
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }
}
