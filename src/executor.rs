use crate::{
    config::{now, random_id},
    protocol::*,
    state::{Record, Store, boot_id, start_ticks},
    terminal::{self, Master, Pty},
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
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::{mpsc, watch},
    task::JoinHandle,
};

const MAX_EVENTS_PER_READ: usize = 128;
// Retain bounded deduplication tombstones for the entire session; never evict
// an ID and accidentally execute the same input again.
const MAX_MUTATION_REQUESTS: usize = 4096;
// Text bytes are the user-visible cap; this second cap bounds per-event
// metadata when a command emits one byte per pipe read.
const MAX_OUTPUT_EVENTS: usize = 32 * 1024;
const PROCESS_TERM_GRACE: Duration = Duration::from_secs(1);
const PROCESS_REAP_GRACE: Duration = Duration::from_secs(1);
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(1);
const GROUP_POLL_INTERVAL: Duration = Duration::from_millis(10);
const INPUT_QUEUE_CAPACITY: usize = 16;
const MAX_INPUT_BYTES: usize = 4 * 1024;

struct InputCommand {
    data: Vec<u8>,
    eof: bool,
}

struct InputState {
    sender: mpsc::Sender<InputCommand>,
    pty: Option<Master>,
    closed: bool,
}

struct Job {
    record: Record,
    output: VecDeque<Output>,
    bytes: usize,
    next_seq: u64,
    cancel: watch::Sender<bool>,
    input: Option<InputState>,
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
    Write {
        job_id: String,
        bytes: usize,
        eof: bool,
        pty: bool,
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
            Self::Write {
                job_id,
                bytes,
                eof,
                pty,
            } => format!("write job_id={job_id} bytes={bytes} eof={eof} pty={pty}"),
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
            Operation::Write { job_id, data, eof } => self.write(&req.id, job_id, data, eof),
            Operation::Resize { job_id, rows, cols } => self.resize(&req.id, job_id, rows, cols),
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
            Ok(signature) => format!(
                "exec:{:x}",
                <sha2::Sha256 as sha2::Digest>::digest(signature.as_bytes())
            ),
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
        if self.requests.len() >= MAX_MUTATION_REQUESTS {
            return Reply::err(
                id,
                "RESOURCE_LIMIT",
                "session mutation request quota reached; retained IDs remain queryable, but new input requires a new session",
            );
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
        let job_slots = self
            .requests
            .values()
            .filter(|entry| {
                matches!(
                    entry.outcome,
                    RequestOutcome::Job { .. } | RequestOutcome::ExecutionUnknown { .. }
                )
            })
            .count();
        if job_slots >= MAX_JOBS || self.jobs.len() >= MAX_JOBS || running >= MAX_RUNNING {
            return Reply::err(
                id,
                "RESOURCE_LIMIT",
                "maximum 4 running / 16 total jobs per session; create a new session after completion",
            );
        }

        let job_id = random_id();
        let pty = if args.pty {
            match Pty::open() {
                Ok(pty) => Some(pty),
                Err(error) => return Reply::err(id, "SPAWN_FAILED", error.to_string()),
            }
        } else {
            None
        };
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
            .kill_on_drop(true);
        if let Some(pty) = &pty {
            let stdin = match pty.duplicate_slave() {
                Ok(fd) => fd,
                Err(error) => return Reply::err(id, "SPAWN_FAILED", error.to_string()),
            };
            let stdout = match pty.duplicate_slave() {
                Ok(fd) => fd,
                Err(error) => return Reply::err(id, "SPAWN_FAILED", error.to_string()),
            };
            let stderr = match pty.duplicate_slave() {
                Ok(fd) => fd,
                Err(error) => return Reply::err(id, "SPAWN_FAILED", error.to_string()),
            };
            cmd.stdin(Stdio::from(stdin))
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr));
            // PTY setup creates the child's process group through setsid;
            // process_group(0) would make the child a group leader first and
            // cause setsid to fail with EPERM.
            terminal::configure_child(&mut cmd);
        } else {
            cmd.stdin(if args.stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        }
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                let message = format!(
                    "execution status unknown; job_id={job_id} pid=unknown; process spawn failed ({error}); do not repeat this request"
                );
                self.remember_unknown(id, signature, job_id, None, message.clone());
                return Reply::err(id, "EXECUTION_UNKNOWN", message);
            }
        };
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

        let master = pty.as_ref().map(Pty::master);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let pipe_stdin = if args.stdin && pty.is_none() {
            child.stdin.take()
        } else {
            None
        };
        if pty.is_none() && (stdout.is_none() || stderr.is_none()) {
            let message = format!(
                "execution status unknown; job_id={job_id} pid={pid}; process output pipes were unavailable after start; kill requested; do not repeat this request"
            );
            self.remember_unknown(id, signature, job_id, Some(pid), message.clone());
            self.schedule_reap(child, Some(pid));
            return Reply::err(id, "EXECUTION_UNKNOWN", message);
        }
        if args.stdin && pty.is_none() && pipe_stdin.is_none() {
            let message = format!(
                "execution status unknown; job_id={job_id} pid={pid}; process stdin was unavailable after start; kill requested; do not repeat this request"
            );
            self.remember_unknown(id, signature, job_id, Some(pid), message.clone());
            self.schedule_reap(child, Some(pid));
            return Reply::err(id, "EXECUTION_UNKNOWN", message);
        }

        let (cancel, cancelled) = watch::channel(false);
        let (input, input_task) = if let Some(master) = master.clone() {
            let (sender, receiver) = mpsc::channel(INPUT_QUEUE_CAPACITY);
            let task = tokio::spawn(run_pty_input(master.clone(), receiver));
            (
                Some(InputState {
                    sender,
                    pty: Some(master),
                    closed: false,
                }),
                Some(task),
            )
        } else if let Some(stdin) = pipe_stdin {
            let (sender, receiver) = mpsc::channel(INPUT_QUEUE_CAPACITY);
            let task = tokio::spawn(run_pipe_input(stdin, receiver));
            (
                Some(InputState {
                    sender,
                    pty: None,
                    closed: false,
                }),
                Some(task),
            )
        } else {
            (None, None)
        };
        let job = Arc::new(Mutex::new(Job {
            record,
            output: VecDeque::new(),
            bytes: 0,
            next_seq: 0,
            cancel,
            input,
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
        let mut outputs = Vec::new();
        if let Some(master) = master {
            let output_job = job.clone();
            outputs.push(tokio::spawn(async move {
                drain_pty(master, output_job).await;
            }));
        } else {
            let output_job = job.clone();
            let error_job = job.clone();
            // These were checked above for the pipe path.
            let stdout = stdout.expect("pipe stdout checked before job creation");
            let stderr = stderr.expect("pipe stderr checked before job creation");
            outputs.push(tokio::spawn(async move {
                drain(stdout, output_job, "stdout").await;
            }));
            outputs.push(tokio::spawn(async move {
                drain(stderr, error_job, "stderr").await;
            }));
        }
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
                outputs,
                input_task,
            })
            .await;
        }));
        Reply::ok(id, lock_job(&job).view(0))
    }
    fn write(&mut self, id: &str, job_id: String, data: String, eof: bool) -> Reply {
        if data.len() > MAX_INPUT_BYTES {
            return Reply::err(
                id,
                "INVALID_ARGUMENT",
                "write data must be at most 4096 UTF-8 bytes",
            );
        }
        let signature = match serde_json::to_string(&(&job_id, &data, eof)) {
            Ok(signature) => format!(
                "write:{:x}",
                <sha2::Sha256 as sha2::Digest>::digest(signature.as_bytes())
            ),
            Err(_) => {
                return Reply::err(id, "INTERNAL_ERROR", "could not serialize write arguments");
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
        if self.requests.len() >= MAX_MUTATION_REQUESTS {
            return Reply::err(
                id,
                "RESOURCE_LIMIT",
                "session mutation request quota reached; retained IDs remain queryable, but new input requires a new session",
            );
        }

        let Some(job) = self.jobs.get(&job_id).cloned() else {
            return Reply::err(
                id,
                "JOB_NOT_FOUND",
                "job is not owned by this Connector incarnation",
            );
        };
        let bytes = data.len();
        let (pty, send_result) = {
            let mut j = lock_job(&job);
            if j.record.state != "running" {
                return Reply::err(id, "INPUT_CLOSED", "job is no longer running");
            }
            let Some(input) = j.input.as_mut() else {
                return Reply::err(
                    id,
                    "INVALID_ARGUMENT",
                    "stdin was not enabled; set stdin=true or pty=true for Exec",
                );
            };
            if input.closed {
                return Reply::err(id, "INPUT_CLOSED", "pipe stdin is already closed");
            }
            let pty = input.pty.is_some();
            let send_result = input.sender.try_send(InputCommand {
                data: data.into_bytes(),
                eof,
            });
            if send_result.is_ok() && eof && !pty {
                // The worker shuts down its ChildStdin after this command. Do
                // not accept a later write while that close is in flight.
                input.closed = true;
            }
            (pty, send_result)
        };

        match send_result {
            Ok(()) => {
                self.requests.insert(
                    id.into(),
                    RequestEntry {
                        signature,
                        outcome: RequestOutcome::Write {
                            job_id: job_id.clone(),
                            bytes,
                            eof,
                            pty,
                        },
                    },
                );
                write_reply(id, &job_id, bytes, eof, pty)
            }
            Err(mpsc::error::TrySendError::Full(_)) => Reply::err(
                id,
                "RESOURCE_LIMIT",
                "input queue is full; retry the same request ID without changing its arguments",
            ),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Reply::err(id, "INPUT_CLOSED", "job input is closed")
            }
        }
    }
    fn resize(&mut self, id: &str, job_id: String, rows: u16, cols: u16) -> Reply {
        if !(1..=1000).contains(&rows) || !(1..=1000).contains(&cols) {
            return Reply::err(
                id,
                "INVALID_ARGUMENT",
                "PTY rows and cols must be in 1..=1000",
            );
        }
        let Some(job) = self.jobs.get(&job_id) else {
            return Reply::err(
                id,
                "JOB_NOT_FOUND",
                "job is not owned by this Connector incarnation",
            );
        };
        let master = {
            let j = lock_job(job);
            if j.record.state != "running" {
                return Reply::err(id, "INPUT_CLOSED", "job is no longer running");
            }
            let Some(input) = j.input.as_ref() else {
                return Reply::err(id, "INVALID_ARGUMENT", "job was not started with pty=true");
            };
            let Some(master) = input.pty.as_ref() else {
                return Reply::err(id, "INVALID_ARGUMENT", "job was not started with pty=true");
            };
            master.clone()
        };
        if let Err(error) = master.resize(rows, cols) {
            return Reply::err(id, "PTY_RESIZE_FAILED", error.to_string());
        }
        Reply::ok(
            id,
            serde_json::json!({
                "job_id": job_id,
                "rows": rows,
                "cols": cols,
                "resized": true,
            }),
        )
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
            RequestOutcome::Write {
                job_id,
                bytes,
                eof,
                pty,
            } => write_reply(id, job_id, *bytes, *eof, *pty),
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

fn write_reply(id: &str, job_id: &str, bytes: usize, eof: bool, pty: bool) -> Reply {
    let message = if eof && pty {
        "data queued and EOT sent; EOT does not guarantee that the PTY application exits"
    } else if eof {
        "data queued and pipe stdin will be closed"
    } else {
        "data queued"
    };
    Reply::ok(
        id,
        serde_json::json!({
            "job_id": job_id,
            "accepted": true,
            "bytes": bytes,
            "eof": eof,
            "pty": pty,
            "message": message,
        }),
    )
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

async fn finish_outputs(mut outputs: Vec<JoinHandle<()>>) {
    let drained = tokio::time::timeout(OUTPUT_DRAIN_GRACE, async {
        for output in &mut outputs {
            let _ = output.await;
        }
    })
    .await;
    if drained.is_err() {
        for output in &mut outputs {
            output.abort();
        }
        for output in outputs {
            let _ = output.await;
        }
    }
}

fn close_input(job: &Arc<Mutex<Job>>) {
    let input = lock_job(job).input.take();
    drop(input);
}

async fn stop_input(task: &mut Option<JoinHandle<()>>) {
    if let Some(task) = task.take() {
        task.abort();
        let _ = task.await;
    }
}

struct JobRuntime {
    child: Child,
    pid: u32,
    timeout_ms: u64,
    cancelled: watch::Receiver<bool>,
    job: Arc<Mutex<Job>>,
    store: Arc<Store>,
    outputs: Vec<JoinHandle<()>>,
    input_task: Option<JoinHandle<()>>,
}

async fn run_job(runtime: JobRuntime) {
    let JobRuntime {
        mut child,
        pid,
        timeout_ms,
        mut cancelled,
        job,
        store,
        outputs,
        mut input_task,
    } = runtime;
    let mut termination_reason: Option<&'static str> = None;
    let status = tokio::select! {
        result = child.wait() => {
            close_input(&job);
            stop_input(&mut input_task).await;
            Some(result)
        },
        _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
            termination_reason = Some("timed_out");
            close_input(&job);
            stop_input(&mut input_task).await;
            terminate_child_group(&mut child, pid).await
        }
        _ = cancelled.changed() => {
            termination_reason = Some("cancelled");
            close_input(&job);
            stop_input(&mut input_task).await;
            terminate_child_group(&mut child, pid).await
        }
    };
    if termination_reason.is_none() {
        // Normal completion still cleans up ordinary descendants that retained
        // the process group or output pipes. This is not daemon containment.
        cleanup_finished_group(pid).await;
    }
    finish_outputs(outputs).await;

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

async fn run_pipe_input(
    mut stdin: tokio::process::ChildStdin,
    mut receiver: mpsc::Receiver<InputCommand>,
) {
    while let Some(command) = receiver.recv().await {
        if !command.data.is_empty() && stdin.write_all(&command.data).await.is_err() {
            break;
        }
        if command.eof {
            let _ = stdin.shutdown().await;
            break;
        }
    }
}

async fn run_pty_input(master: Master, mut receiver: mpsc::Receiver<InputCommand>) {
    while let Some(command) = receiver.recv().await {
        if !command.data.is_empty() && master.write_all(&command.data).await.is_err() {
            break;
        }
        if command.eof {
            // PTYs do not have a pipe-like half-close.  EOT is an input
            // character interpreted by the line discipline/application and
            // therefore is deliberately not treated as an exit guarantee.
            if master.write_all(&[0x04]).await.is_err() {
                break;
            }
        }
    }
}

async fn drain_pty(master: Master, job: Arc<Mutex<Job>>) {
    let mut buffer = [0; 2048];
    loop {
        match master.read(&mut buffer).await {
            Ok(0) => break,
            Ok(count) => {
                lock_job(&job).push("pty", &buffer[..count]);
                tokio::task::yield_now().await;
            }
            Err(error) if terminal::is_pty_closed(&error) => break,
            Err(_) => break,
        }
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
            stdin: false,
            pty: false,
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
    fn output_text(view: &JobView) -> String {
        view.events
            .iter()
            .map(|event| event.text.as_str())
            .collect()
    }
    async fn wait_for_output(manager: &mut Manager, job_id: &str, needle: &str) -> JobView {
        for _ in 0..300 {
            let current = view(manager.handle(request(
                "read-output",
                Operation::Read {
                    job_id: job_id.into(),
                    cursor: 0,
                },
            )));
            if output_text(&current).contains(needle) {
                return current;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job output did not contain {needle:?}");
    }

    #[test]
    fn mutation_cache_is_bounded_and_existing_ids_remain_replay_safe() {
        let dir = temp_state_dir();
        let info = test_info();
        let mut manager = Manager::new(info.clone(), &dir).unwrap();
        let data = "sensitive-stdin-not-retained-verbatim";
        let encoded = serde_json::to_string(&("job", data, false)).unwrap();
        let digest = format!(
            "write:{:x}",
            <sha2::Sha256 as sha2::Digest>::digest(encoded.as_bytes())
        );
        for i in 0..MAX_MUTATION_REQUESTS {
            manager.requests.insert(
                format!("write-{i}"),
                RequestEntry {
                    signature: digest.clone(),
                    outcome: RequestOutcome::Write {
                        job_id: "job".into(),
                        bytes: data.len(),
                        eof: false,
                        pty: false,
                    },
                },
            );
        }
        let duplicate = manager.handle(request(
            "write-0",
            Operation::Write {
                job_id: "job".into(),
                data: data.into(),
                eof: false,
            },
        ));
        assert!(duplicate.error.is_none());
        let new_write = manager.handle(request(
            "new-write",
            Operation::Write {
                job_id: "job".into(),
                data: data.into(),
                eof: false,
            },
        ));
        assert_eq!(new_write.error.unwrap().code, "RESOURCE_LIMIT");
        let new_exec = manager.handle(request(
            "new-exec",
            Operation::Exec(exec_args(&info, vec!["/bin/true".into()], 1000)),
        ));
        assert_eq!(new_exec.error.unwrap().code, "RESOURCE_LIMIT");
        assert_eq!(manager.requests.len(), MAX_MUTATION_REQUESTS);
        assert!(!manager.requests["write-0"].signature.contains(data));
        drop(manager);
        fs::remove_dir_all(dir).unwrap();
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
            stdin: false,
            pty: false,
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
            input: None,
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
            input: None,
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
    async fn pipe_stdin_is_idempotent_and_eof_closes_the_write_end() {
        let dir = temp_state_dir();
        let info = test_info();
        let mut manager = Manager::new(info.clone(), &dir).unwrap();
        let mut args = exec_args(
            &info,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "count=0; while IFS= read -r line; do count=$((count+1)); printf 'line:%s\\n' \"$line\"; done; printf 'count:%s\\n' \"$count\"".into(),
            ],
            5_000,
        );
        args.stdin = true;
        let initial = view(manager.handle(request("pipe-exec", Operation::Exec(args))));

        let first = manager.handle(request(
            "pipe-write",
            Operation::Write {
                job_id: initial.job_id.clone(),
                data: "alpha\n".into(),
                eof: true,
            },
        ));
        assert!(first.error.is_none());
        let duplicate = manager.handle(request(
            "pipe-write",
            Operation::Write {
                job_id: initial.job_id.clone(),
                data: "alpha\n".into(),
                eof: true,
            },
        ));
        assert_eq!(duplicate.result, first.result);
        let conflict = manager.handle(request(
            "pipe-write",
            Operation::Write {
                job_id: initial.job_id.clone(),
                data: "beta\n".into(),
                eof: true,
            },
        ));
        assert_eq!(
            conflict.error.expect("write conflict").code,
            "REQUEST_CONFLICT"
        );
        let exec_conflict = manager.handle(request(
            "pipe-write",
            Operation::Exec(exec_args(
                &info,
                vec!["/bin/echo".into(), "different".into()],
                5_000,
            )),
        ));
        assert_eq!(
            exec_conflict
                .error
                .expect("exec/write namespace conflict")
                .code,
            "REQUEST_CONFLICT"
        );

        let terminal = wait_for_terminal(&mut manager, &initial.job_id).await;
        let text = output_text(&terminal);
        assert!(text.contains("line:alpha"), "output was {text:?}");
        assert!(
            text.contains("count:1"),
            "duplicate write was not suppressed: {text:?}"
        );
        manager.shutdown().await;
        drop(manager);
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    #[cfg(target_os = "linux")]
    async fn pty_supports_a_continuous_shell_resize_and_cancel() {
        let dir = temp_state_dir();
        let info = test_info();
        let mut manager = Manager::new(info.clone(), &dir).unwrap();
        let mut args = exec_args(&info, vec!["/bin/sh".into(), "-i".into()], 60_000);
        args.pty = true;
        let initial = view(manager.handle(request("pty-exec", Operation::Exec(args))));
        assert_eq!(initial.state, "running");

        let invalid = manager.handle(request(
            "pty-resize-invalid",
            Operation::Resize {
                job_id: initial.job_id.clone(),
                rows: 0,
                cols: 80,
            },
        ));
        assert_eq!(
            invalid.error.expect("invalid resize").code,
            "INVALID_ARGUMENT"
        );
        let resized = manager.handle(request(
            "pty-resize",
            Operation::Resize {
                job_id: initial.job_id.clone(),
                rows: 40,
                cols: 100,
            },
        ));
        assert!(resized.error.is_none());

        let size_request = manager.handle(request(
            "pty-size",
            Operation::Write {
                job_id: initial.job_id.clone(),
                data: "stty size\n".into(),
                eof: false,
            },
        ));
        assert!(size_request.error.is_none());
        let sized = wait_for_output(&mut manager, &initial.job_id, "40 100").await;
        assert_eq!(sized.state, "running");

        let ready = manager.handle(request(
            "pty-ready",
            Operation::Write {
                job_id: initial.job_id.clone(),
                data: "printf READY\\n".into(),
                eof: false,
            },
        ));
        assert!(ready.error.is_none());
        let running = wait_for_output(&mut manager, &initial.job_id, "READY").await;
        assert_eq!(running.state, "running");

        let cancel = manager.handle(request(
            "pty-cancel",
            Operation::Cancel {
                job_id: initial.job_id.clone(),
            },
        ));
        assert!(cancel.error.is_none());
        let terminal = wait_for_terminal(&mut manager, &initial.job_id).await;
        assert_eq!(terminal.state, "cancelled");
        assert_eq!(terminal.termination_reason.as_deref(), Some("cancelled"));
        assert!(!group_exists(initial.pgid));
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
