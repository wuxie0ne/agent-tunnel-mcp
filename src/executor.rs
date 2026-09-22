use crate::{
    config::{now, random_id},
    protocol::*,
    state::{Record, Store, boot_id, start_ticks},
};
use anyhow::{Result, ensure};
use std::{
    collections::{HashMap, VecDeque},
    io,
    path::Path,
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::watch,
    task::JoinHandle,
};

const MAX_EVENTS_PER_READ: usize = 128;
// Text bytes are the user-visible cap; this second cap bounds per-event
// metadata when a command emits one byte per pipe read.
const MAX_OUTPUT_EVENTS: usize = 32 * 1024;
const PROCESS_TERM_GRACE: Duration = Duration::from_secs(1);
const PROCESS_REAP_GRACE: Duration = Duration::from_secs(1);
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(1);
const GROUP_POLL_INTERVAL: Duration = Duration::from_millis(10);

struct Job {
    record: Record,
    output: VecDeque<Output>,
    bytes: usize,
    next_seq: u64,
    cancel: watch::Sender<bool>,
}
impl Job {
    fn view(&self, cursor: u64) -> JobView {
        let first = self.output.front().map_or(self.next_seq, |e| e.seq);
        let mut events = Vec::new();
        let mut bytes = 0;
        let mut next = cursor.max(first);
        for e in self.output.iter().filter(|e| e.seq >= cursor) {
            // The byte limit alone is not enough: a stream of one-byte events
            // can otherwise exceed MAX_FRAME through JSON object overhead.
            if events.len() >= MAX_EVENTS_PER_READ || bytes + e.text.len() > MAX_READ {
                break;
            }
            bytes += e.text.len();
            next = e.seq + 1;
            events.push(e.clone());
        }
        JobView {
            target_id: self.record.target_id.clone(),
            incarnation: self.record.incarnation.clone(),
            job_id: self.record.job_id.clone(),
            pid: self.record.pid,
            pgid: self.record.pgid,
            process_start_ticks: self.record.process_start_ticks,
            state: self.record.state.clone(),
            exit_code: self.record.exit_code,
            termination_reason: self.record.termination_reason.clone(),
            events,
            next_cursor: next,
            output_truncated: first > 0,
            dropped_before_cursor: (cursor < first).then_some(first),
            output_encoding: "utf8-lossy-control-escaped".into(),
        }
    }
    fn push(&mut self, stream: &str, bytes: &[u8]) {
        let text = safe_text(bytes);
        // Keep every stored event small enough for one read. The normal pipe
        // buffer is much smaller, but this also keeps an internal caller from
        // creating a cursor that cannot advance because one event is huge.
        let mut chunk = String::new();
        for character in text.chars() {
            if !chunk.is_empty() && chunk.len() + character.len_utf8() > MAX_READ {
                self.append_event(stream, std::mem::take(&mut chunk));
            }
            chunk.push(character);
        }
        if !chunk.is_empty() {
            self.append_event(stream, chunk);
        }
    }
    fn append_event(&mut self, stream: &str, text: String) {
        self.bytes = self.bytes.saturating_add(text.len());
        self.output.push_back(Output {
            seq: self.next_seq,
            stream: stream.into(),
            text,
        });
        self.next_seq = self.next_seq.saturating_add(1);
        while self.bytes > MAX_OUTPUT || self.output.len() > MAX_OUTPUT_EVENTS {
            let Some(old) = self.output.pop_front() else {
                self.bytes = 0;
                break;
            };
            self.bytes = self.bytes.saturating_sub(old.text.len());
        }
    }
}
fn safe_text(bytes: &[u8]) -> String {
    let mut out = String::new();
    for c in String::from_utf8_lossy(bytes).chars() {
        if c.is_control() && c != '\n' && c != '\t' {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

struct RequestEntry {
    signature: String,
    outcome: RequestOutcome,
}
enum RequestOutcome {
    Job {
        job_id: String,
        pid: u32,
    },
    ExecutionUnknown {
        job_id: String,
        pid: Option<u32>,
        message: String,
    },
}
impl RequestOutcome {
    fn identity(&self) -> String {
        match self {
            Self::Job { job_id, pid } => format!("job_id={job_id} pid={pid}"),
            Self::ExecutionUnknown { job_id, pid, .. } => {
                format!(
                    "job_id={job_id} pid={}",
                    pid.map_or_else(|| "unknown".into(), |pid| pid.to_string())
                )
            }
        }
    }
}

pub struct Manager {
    pub info: TargetInfo,
    jobs: HashMap<String, Arc<Mutex<Job>>>,
    requests: HashMap<String, RequestEntry>,
    handles: Vec<JoinHandle<()>>,
    store: Arc<Store>,
}
impl Manager {
    pub fn new(info: TargetInfo, dir: &Path) -> Result<Self> {
        Ok(Self {
            info,
            jobs: HashMap::new(),
            requests: HashMap::new(),
            handles: Vec::new(),
            store: Arc::new(Store::open(dir)?),
        })
    }
    pub fn handle(&mut self, req: Request) -> Reply {
        if !valid_id(&req.id) {
            return Reply::err(&req.id, "INVALID_ARGUMENT", "invalid request ID");
        }
        match req.op {
            Operation::Info => Reply::ok(&req.id, &self.info),
            Operation::Exec(args) => self.exec(&req.id, args),
            Operation::Read { job_id, cursor } => match self.jobs.get(&job_id) {
                Some(job) => Reply::ok(&req.id, lock_job(job).view(cursor)),
                None => Reply::err(
                    &req.id,
                    "JOB_NOT_FOUND",
                    "job is not owned by this Connector incarnation",
                ),
            },
            Operation::Cancel { job_id } => match self.jobs.get(&job_id) {
                Some(job) => {
                    let j = lock_job(job);
                    let running = j.record.state == "running";
                    if running {
                        let _ = j.cancel.send(true);
                    }
                    Reply::ok(
                        &req.id,
                        serde_json::json!({
                            "job_id": job_id,
                            "target_id": j.record.target_id,
                            "incarnation": j.record.incarnation,
                            "cancellation_requested": running,
                            "state": j.record.state,
                            "confirmed_terminated": !running,
                        }),
                    )
                }
                None => Reply::err(
                    &req.id,
                    "JOB_NOT_FOUND",
                    "job is not owned by this Connector incarnation",
                ),
            },
        }
    }
    fn exec(&mut self, id: &str, args: Exec) -> Reply {
        let signature = match serde_json::to_string(&args) {
            Ok(signature) => signature,
            Err(_) => {
                return Reply::err(id, "INTERNAL_ERROR", "could not serialize exec arguments");
            }
        };
        if let Some(previous) = self.requests.get(id) {
            if previous.signature != signature {
                return Reply::err(
                    id,
                    "REQUEST_CONFLICT",
                    format!(
                        "request ID already used with different arguments; existing {}",
                        previous.outcome.identity()
                    ),
                );
            }
            return self.duplicate_reply(id, previous);
        }
        if args.expected_incarnation != self.info.incarnation {
            return Reply::err(
                id,
                "TARGET_MISMATCH",
                "call remote_info and confirm this Connector incarnation",
            );
        }
        if let Err(e) = validate(&args) {
            return Reply::err(id, "INVALID_ARGUMENT", e.to_string());
        }
        let running = self
            .jobs
            .values()
            .filter(|j| lock_job(j).record.state == "running")
            .count();
        // requests includes successful jobs and execution-unknown tombstones.
        // A failed persistence attempt therefore reserves the same one-shot
        // slot and cannot grow unboundedly outside the job quota.
        if self.requests.len() >= MAX_JOBS || self.jobs.len() >= MAX_JOBS || running >= MAX_RUNNING
        {
            return Reply::err(
                id,
                "RESOURCE_LIMIT",
                "maximum 4 running / 16 total jobs per session; create a new session after completion",
            );
        }

        let mut cmd = Command::new(&args.argv[0]);
        cmd.args(&args.argv[1..])
            .current_dir(&args.cwd)
            .env_clear()
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("LANG", "C.UTF-8")
            .envs(&args.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => return Reply::err(id, "SPAWN_FAILED", e.to_string()),
        };
        let job_id = random_id();
        let Some(pid) = child.id() else {
            let message = format!(
                "execution status unknown; job_id={job_id} pid=unknown; process started but no PID was reported; kill requested; do not repeat this request"
            );
            self.remember_unknown(id, signature, job_id, None, message.clone());
            self.schedule_reap(child, None);
            return Reply::err(id, "EXECUTION_UNKNOWN", message);
        };
        let record = Record {
            session_id: self.info.session_id.clone(),
            target_id: self.info.target_id.clone(),
            incarnation: self.info.incarnation.clone(),
            job_id: job_id.clone(),
            request_id: id.into(),
            pid,
            pgid: pid,
            boot_id: boot_id(),
            process_start_ticks: start_ticks(pid),
            state: "running".into(),
            exit_code: None,
            termination_reason: None,
            updated_at: now(),
        };
        if let Err(e) = self.store.write(&record) {
            let message = format!(
                "execution status unknown; job_id={job_id} pid={pid}; PID record persistence failed ({e}); kill requested; do not repeat this request"
            );
            // This tombstone is deliberately retained even though the record
            // could not be persisted. Reusing the request ID must not rerun a
            // command whose side effects are already possible.
            self.remember_unknown(id, signature, job_id, Some(pid), message.clone());
            self.schedule_reap(child, Some(pid));
            return Reply::err(id, "EXECUTION_UNKNOWN", message);
        }

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
            let message = format!(
                "execution status unknown; job_id={job_id} pid={pid}; process output pipes were unavailable after start; kill requested; do not repeat this request"
            );
            self.remember_unknown(id, signature, job_id, Some(pid), message.clone());
            self.schedule_reap(child, Some(pid));
            return Reply::err(id, "EXECUTION_UNKNOWN", message);
        };

        let (cancel, cancelled) = watch::channel(false);
        let job = Arc::new(Mutex::new(Job {
            record,
            output: VecDeque::new(),
            bytes: 0,
            next_seq: 0,
            cancel,
        }));
        self.jobs.insert(job_id.clone(), job.clone());
        self.requests.insert(
            id.into(),
            RequestEntry {
                signature,
                outcome: RequestOutcome::Job {
                    job_id: job_id.clone(),
                    pid,
                },
            },
        );
        let output_job = job.clone();
        let error_job = job.clone();
        let stdout = tokio::spawn(async move { drain(stdout, output_job, "stdout").await });
        let stderr = tokio::spawn(async move { drain(stderr, error_job, "stderr").await });
        let worker_job = job.clone();
        let store = self.store.clone();
        let timeout_ms = args.timeout_ms;
        self.handles.push(tokio::spawn(async move {
            run_job(JobRuntime {
                child,
                pid,
                timeout_ms,
                cancelled,
                job: worker_job,
                store,
                stdout,
                stderr,
            })
            .await;
        }));
        Reply::ok(id, lock_job(&job).view(0))
    }
    fn duplicate_reply(&self, id: &str, entry: &RequestEntry) -> Reply {
        match &entry.outcome {
            RequestOutcome::Job { job_id, pid } => match self.jobs.get(job_id) {
                Some(job) => Reply::ok(id, lock_job(job).view(0)),
                None => Reply::err(
                    id,
                    "EXECUTION_UNKNOWN",
                    format!(
                        "{}; in-memory job state is unavailable; do not repeat this request",
                        RequestOutcome::Job {
                            job_id: job_id.clone(),
                            pid: *pid,
                        }
                        .identity()
                    ),
                ),
            },
            RequestOutcome::ExecutionUnknown { message, .. } => {
                Reply::err(id, "EXECUTION_UNKNOWN", message.clone())
            }
        }
    }
    fn remember_unknown(
        &mut self,
        id: &str,
        signature: String,
        job_id: String,
        pid: Option<u32>,
        message: String,
    ) {
        self.requests.insert(
            id.into(),
            RequestEntry {
                signature,
                outcome: RequestOutcome::ExecutionUnknown {
                    job_id,
                    pid,
                    message,
                },
            },
        );
    }
    fn schedule_reap(&mut self, mut child: Child, pgid: Option<u32>) {
        if let Some(pgid) = pgid {
            let _ = kill_group(pgid, libc::SIGKILL);
        }
        self.handles.push(tokio::spawn(async move {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(PROCESS_REAP_GRACE, child.wait()).await;
        }));
    }
    pub async fn shutdown(&mut self) {
        for job in self.jobs.values() {
            let j = lock_job(job);
            if j.record.state == "running" {
                let _ = j.cancel.send(true);
            }
        }
        for handle in self.handles.drain(..) {
            let _ = handle.await;
        }
    }
}

fn lock_job(job: &Mutex<Job>) -> MutexGuard<'_, Job> {
    match job.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn validate(args: &Exec) -> Result<()> {
    ensure!(
        !args.argv.is_empty() && !args.argv[0].is_empty() && args.argv.len() <= 128,
        "argv must contain 1..128 arguments"
    );
    ensure!(
        args.argv.iter().map(String::len).sum::<usize>() <= 16 * 1024
            && args.argv.iter().all(|a| !a.contains('\0')),
        "invalid or oversized argv"
    );
    ensure!(
        Path::new(&args.cwd).is_absolute(),
        "cwd must be an absolute remote path"
    );
    ensure!(
        (1..=600_000).contains(&args.timeout_ms),
        "timeout_ms must be 1..600000"
    );
    ensure!(
        args.env.len() <= 32
            && args.env.iter().all(|(k, v)| {
                !k.is_empty()
                    && !k.contains(['=', '\0'])
                    && !v.contains('\0')
                    && k.len() + v.len() <= 4096
            }),
        "invalid environment overrides"
    );
    Ok(())
}
fn kill_group(pgid: u32, signal: i32) -> bool {
    let Ok(pgid) = i32::try_from(pgid) else {
        return false;
    };
    if pgid <= 0 {
        return false;
    }
    // SAFETY: only positive, active process-group IDs created by this Manager
    // are passed here; the negative value targets that group, not a recovered
    // PID record.
    unsafe { libc::kill(-pgid, signal) == 0 }
}
fn group_exists(pgid: u32) -> bool {
    let Ok(pgid) = i32::try_from(pgid) else {
        return false;
    };
    if pgid <= 0 {
        return false;
    }
    // SAFETY: kill with signal zero only probes the active process group.
    let result = unsafe { libc::kill(-pgid, 0) };
    result == 0 || (result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

async fn terminate_child_group(child: &mut Child, pgid: u32) -> Option<io::Result<ExitStatus>> {
    let _ = kill_group(pgid, libc::SIGTERM);
    let mut status = tokio::time::timeout(PROCESS_TERM_GRACE, child.wait())
        .await
        .ok();
    if status.is_none() {
        let _ = kill_group(pgid, libc::SIGKILL);
        let _ = child.start_kill();
        status = tokio::time::timeout(PROCESS_REAP_GRACE, child.wait())
            .await
            .ok();
    }
    // A direct child can be reaped before ordinary descendants have closed
    // their inherited pipes, so always finish the active process group with
    // KILL. Detached/setsid daemons are outside this ordinary-group guarantee.
    let _ = kill_group(pgid, libc::SIGKILL);
    if status.is_none() {
        let _ = child.start_kill();
    }
    status
}

async fn cleanup_finished_group(pgid: u32) {
    if !kill_group(pgid, libc::SIGTERM) {
        return;
    }
    let deadline = Instant::now() + PROCESS_TERM_GRACE;
    while Instant::now() < deadline {
        if !group_exists(pgid) {
            return;
        }
        tokio::time::sleep(GROUP_POLL_INTERVAL).await;
    }
    let _ = kill_group(pgid, libc::SIGKILL);
}

async fn finish_drains(mut stdout: JoinHandle<()>, mut stderr: JoinHandle<()>) {
    let drained = tokio::time::timeout(OUTPUT_DRAIN_GRACE, async {
        let _ = tokio::join!(&mut stdout, &mut stderr);
    })
    .await;
    if drained.is_err() {
        stdout.abort();
        stderr.abort();
        let _ = stdout.await;
        let _ = stderr.await;
    }
}

struct JobRuntime {
    child: Child,
    pid: u32,
    timeout_ms: u64,
    cancelled: watch::Receiver<bool>,
    job: Arc<Mutex<Job>>,
    store: Arc<Store>,
    stdout: JoinHandle<()>,
    stderr: JoinHandle<()>,
}

async fn run_job(runtime: JobRuntime) {
    let JobRuntime {
        mut child,
        pid,
        timeout_ms,
        mut cancelled,
        job,
        store,
        stdout,
        stderr,
    } = runtime;
    let mut termination_reason: Option<&'static str> = None;
    let status = tokio::select! {
        result = child.wait() => Some(result),
        _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
            termination_reason = Some("timed_out");
            terminate_child_group(&mut child, pid).await
        }
        _ = cancelled.changed() => {
            termination_reason = Some("cancelled");
            terminate_child_group(&mut child, pid).await
        }
    };
    if termination_reason.is_none() {
        // Normal completion still cleans up ordinary descendants that retained
        // the process group or output pipes. This is not daemon containment.
        cleanup_finished_group(pid).await;
    }
    finish_drains(stdout, stderr).await;

    let final_record = {
        let mut j = lock_job(&job);
        j.record.state = termination_reason
            .unwrap_or(if status.as_ref().is_some_and(|status| status.is_ok()) {
                "exited"
            } else {
                "failed"
            })
            .into();
        j.record.exit_code = status
            .as_ref()
            .and_then(|status| status.as_ref().ok())
            .and_then(ExitStatus::code);
        j.record.termination_reason = termination_reason.map(str::to_owned);
        j.record.updated_at = now();
        j.record.clone()
    };
    if store.write(&final_record).is_err() {
        // Do not log command arguments, environment, or filesystem errors.
        // The in-memory job result remains authoritative for this incarnation;
        // inspect() will expose a stale/not-running record if persistence failed.
        eprintln!(
            "warning: final PID record update failed for job_id={} pid={}",
            final_record.job_id, final_record.pid
        );
    }
}

async fn drain<R: AsyncRead + Unpin>(mut reader: R, job: Arc<Mutex<Job>>, stream: &str) {
    let mut buf = [0; 2048];
    while let Ok(n) = reader.read(&mut buf).await {
        if n == 0 {
            break;
        }
        lock_job(&job).push(stream, &buf[..n]);
        tokio::task::yield_now().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn temp_state_dir() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("agent-tunnel-executor-{}", random_id()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    fn test_info() -> TargetInfo {
        TargetInfo {
            session_id: "session".into(),
            target_id: "target".into(),
            incarnation: "incarnation".into(),
            name: "test".into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            uid: 0,
            gid: 0,
            cwd: "/tmp".into(),
            expires_at: u64::MAX,
            protocol: VERSION,
            approval: "test".into(),
            end_to_end_encrypted: false,
        }
    }
    fn exec_args(info: &TargetInfo, argv: Vec<String>, timeout_ms: u64) -> Exec {
        Exec {
            expected_incarnation: info.incarnation.clone(),
            argv,
            cwd: "/tmp".into(),
            env: Default::default(),
            timeout_ms,
        }
    }
    fn request(id: &str, op: Operation) -> Request {
        Request { id: id.into(), op }
    }
    fn view(reply: Reply) -> JobView {
        serde_json::from_value(reply.result.expect("test reply result")).unwrap()
    }
    async fn wait_for_terminal(manager: &mut Manager, job_id: &str) -> JobView {
        for _ in 0..300 {
            let current = view(manager.handle(request(
                "read-job",
                Operation::Read {
                    job_id: job_id.into(),
                    cursor: 0,
                },
            )));
            if current.state != "running" {
                return current;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job did not become terminal");
    }

    #[test]
    fn terminal_escapes_are_not_forwarded() {
        assert_eq!(
            safe_text(b"\x1b]52;payload\x07\r\n"),
            "\\u{1b}]52;payload\\u{7}\\r\n"
        );
    }
    #[test]
    fn validation_is_explicit() {
        let mut e = Exec {
            expected_incarnation: "x".into(),
            argv: vec!["echo".into()],
            cwd: "/tmp".into(),
            env: Default::default(),
            timeout_ms: 100,
        };
        assert!(validate(&e).is_ok());
        e.cwd = ".".into();
        assert!(validate(&e).is_err());
    }
    #[test]
    fn read_limits_events_and_cursor_progress() {
        let (cancel, _) = watch::channel(false);
        let record = Record {
            session_id: "session".into(),
            target_id: "target".into(),
            incarnation: "incarnation".into(),
            job_id: "job".into(),
            request_id: "request".into(),
            pid: 1,
            pgid: 1,
            boot_id: None,
            process_start_ticks: None,
            state: "running".into(),
            exit_code: None,
            termination_reason: None,
            updated_at: now(),
        };
        let mut job = Job {
            record,
            output: VecDeque::new(),
            bytes: 0,
            next_seq: 0,
            cancel,
        };
        for _ in 0..300 {
            job.push("stdout", b"x");
        }
        let first = job.view(0);
        assert_eq!(first.events.len(), MAX_EVENTS_PER_READ);
        assert_eq!(first.next_cursor, 128);
        assert!(serde_json::to_vec(&first).unwrap().len() < MAX_FRAME);
        let second = job.view(first.next_cursor);
        assert_eq!(second.events.len(), MAX_EVENTS_PER_READ);
        assert_eq!(second.next_cursor, 256);
        let third = job.view(second.next_cursor);
        assert_eq!(third.events.len(), 44);
        assert_eq!(third.next_cursor, 300);
    }
    #[test]
    fn output_buffer_is_bounded_without_panicking_on_large_input() {
        let (cancel, _) = watch::channel(false);
        let record = Record {
            session_id: "session".into(),
            target_id: "target".into(),
            incarnation: "incarnation".into(),
            job_id: "job".into(),
            request_id: "request".into(),
            pid: 1,
            pgid: 1,
            boot_id: None,
            process_start_ticks: None,
            state: "running".into(),
            exit_code: None,
            termination_reason: None,
            updated_at: now(),
        };
        let mut job = Job {
            record,
            output: VecDeque::new(),
            bytes: 0,
            next_seq: 0,
            cancel,
        };
        job.push("stdout", &vec![b"x"[0]; MAX_OUTPUT + MAX_READ]);
        assert!(job.bytes <= MAX_OUTPUT);
        assert!(
            job.output
                .iter()
                .map(|event| event.text.len())
                .sum::<usize>()
                <= MAX_OUTPUT
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn normal_process_captures_output_and_exit_status() {
        let dir = temp_state_dir();
        let info = test_info();
        let mut manager = Manager::new(info.clone(), &dir).unwrap();
        let reply = manager.handle(request(
            "normal-exec",
            Operation::Exec(exec_args(
                &info,
                vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "printf out; printf err >&2; exit 7".into(),
                ],
                5_000,
            )),
        ));
        let initial = view(reply);
        let terminal = wait_for_terminal(&mut manager, &initial.job_id).await;
        assert_eq!(terminal.state, "exited");
        assert_eq!(terminal.exit_code, Some(7));
        assert!(terminal.events.iter().any(|event| event.text == "out"));
        assert!(terminal.events.iter().any(|event| event.text == "err"));
        manager.shutdown().await;
        drop(manager);
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_terminates_the_owned_process_group() {
        let dir = temp_state_dir();
        let info = test_info();
        let mut manager = Manager::new(info.clone(), &dir).unwrap();
        let initial = view(manager.handle(request(
            "cancel-exec",
            Operation::Exec(exec_args(
                &info,
                vec!["/bin/sh".into(), "-c".into(), "sleep 30 & wait".into()],
                60_000,
            )),
        )));
        let pgid = initial.pgid;
        let cancel = manager.handle(request(
            "cancel-request",
            Operation::Cancel {
                job_id: initial.job_id.clone(),
            },
        ));
        assert!(cancel.error.is_none());
        let terminal = wait_for_terminal(&mut manager, &initial.job_id).await;
        assert_eq!(terminal.state, "cancelled");
        assert_eq!(terminal.termination_reason.as_deref(), Some("cancelled"));
        assert!(!group_exists(pgid));
        manager.shutdown().await;
        drop(manager);
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_initial_record_write_leaves_an_execution_unknown_tombstone() {
        let dir = temp_state_dir();
        let info = test_info();
        let mut manager = Manager::new(info.clone(), &dir).unwrap();
        manager.store.fail_next_write_for_test();
        let args = exec_args(
            &info,
            vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            60_000,
        );
        let first = manager.handle(request("unknown-exec", Operation::Exec(args.clone())));
        let first_error = first.error.expect("unknown execution error");
        assert_eq!(first_error.code, "EXECUTION_UNKNOWN");
        assert!(first_error.message.contains("job_id="));
        assert!(first_error.message.contains("pid="));

        let second = manager.handle(request("unknown-exec", Operation::Exec(args.clone())));
        let second_error = second.error.expect("tombstone must reject replay");
        assert_eq!(second_error.code, "EXECUTION_UNKNOWN");
        assert_eq!(second_error.message, first_error.message);

        let conflict = manager.handle(request(
            "unknown-exec",
            Operation::Exec(exec_args(
                &info,
                vec!["/bin/echo".into(), "different".into()],
                60_000,
            )),
        ));
        assert_eq!(
            conflict.error.expect("request conflict").code,
            "REQUEST_CONFLICT"
        );
        manager.shutdown().await;
        drop(manager);
        fs::remove_dir_all(dir).unwrap();
    }
}
