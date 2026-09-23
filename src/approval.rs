//! Local, per-request operator approval for the Controller.
//!
//! This is a consent gate, not an OS isolation boundary.  Code running as the
//! same Unix UID can generally inspect or interfere with the Controller, its
//! IPC socket, or this process.  The gate is intended to make accidental or
//! unattended remote execution fail closed, not to defend against that code.

use anyhow::{Context, Result, bail, ensure};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::File,
    future::Future,
    io::{self, Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    pin::Pin,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};
use tokio::{
    io::unix::AsyncFd,
    sync::mpsc::{self, Receiver, Sender},
};

type ConsoleFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type GateSetup = (Arc<Gate>, Option<ConsoleFuture>);

const APPROVAL_TTL: Duration = Duration::from_secs(60);
const MAX_RECORDS: usize = 128;
const MAX_PENDING: usize = 32;
const SEND_QUEUE: usize = MAX_PENDING;
const CODE_LEN: usize = 8;
const MAX_TTY_LINE: usize = 512;
const TTY_READ_CHUNK: usize = 1024;
const MAX_TTY_OUTPUT: usize = 16 * 1024;

// 32 symbols makes every low five-bit value uniform and avoids ambiguous I/O.
const CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// A local approval gate owned by a Controller.
///
/// `check` is deliberately synchronous: a request is either already approved,
/// explicitly allowed by policy, or gets a deterministic error reply.  The
/// returned console future is the only task that touches the TTY.
pub struct Gate {
    session_approved: bool,
    inner: Arc<Mutex<Inner>>,
    prompt_tx: Option<Sender<Vec<u8>>>,
    ttl: Duration,
}

struct Inner {
    records: HashMap<String, Record>,
    // Only pending approval codes are indexed.  Approved/denied records stay
    // in `records` so an old request ID cannot be silently reused.
    by_code: HashMap<String, String>,
}

struct Record {
    digest: [u8; 32],
    state: RecordState,
    code: String,
    expires_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordState {
    Pending,
    Approved,
    Denied,
}

#[derive(Clone, Copy)]
enum ConsoleDecision {
    Allow,
    Deny,
}

impl Gate {
    /// Create a gate and, for manual approval, the not-yet-spawned TTY future.
    ///
    /// The future is returned instead of being spawned here so the Controller
    /// can own it and abort it as part of shutdown.  In manual mode `/dev/tty`
    /// is opened directly with an independent nonblocking descriptor; failure
    /// to obtain a controlling terminal is an error (fail closed).
    pub fn new(session_approved: bool) -> Result<GateSetup> {
        if session_approved {
            return Ok((
                Arc::new(Self::new_inner(session_approved, None, APPROVAL_TTL)),
                None,
            ));
        }

        let tty = open_tty()?;
        let (prompt_tx, prompt_rx) = mpsc::channel(SEND_QUEUE);
        let gate = Arc::new(Self::new_inner(
            session_approved,
            Some(prompt_tx),
            APPROVAL_TTL,
        ));
        let console: Pin<Box<dyn Future<Output = ()> + Send>> =
            Box::pin(console_loop(tty, prompt_rx, Arc::downgrade(&gate)));
        Ok((gate, Some(console)))
    }

    /// Check one request before it is sent to the remote side.
    ///
    /// `None` means that the caller may continue.  `Some(reply)` is already a
    /// protocol-shaped local error and must be returned without sending the
    /// request remotely.  Approval codes and command summaries are never put
    /// into these replies; they are only written to the independent TTY.
    pub fn check(&self, request: &crate::protocol::Request) -> Option<crate::protocol::Reply> {
        if self.session_approved || !requires_approval(&request.op) {
            return None;
        }

        let digest = match request_digest(request) {
            Ok(digest) => digest,
            Err(_) => {
                return Some(crate::protocol::Reply::err(
                    &request.id,
                    "LIMIT",
                    "request could not be represented for approval",
                ));
            }
        };

        // Resolve existing IDs before reserving an output slot.  A duplicate
        // approved request must remain allowed even when the TTY queue is
        // momentarily full.
        {
            let mut inner = lock(&self.inner);
            expire_locked(&mut inner, Instant::now());
            if let Some(record) = inner.records.get(&request.id) {
                return existing_reply(&request.id, &digest, record);
            }
        }

        // Reserve the bounded send queue before creating a record.  This keeps
        // a newly-created pending record from existing without a prompt.
        let permit = match &self.prompt_tx {
            Some(sender) => match sender.clone().try_reserve_owned() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    return Some(crate::protocol::Reply::err(
                        &request.id,
                        "LIMIT",
                        "approval console output queue is full or closed",
                    ));
                }
            },
            // The in-memory constructor is used only by pure logic tests.
            None => None,
        };

        let mut inner = lock(&self.inner);
        expire_locked(&mut inner, Instant::now());
        if let Some(record) = inner.records.get(&request.id) {
            drop(permit);
            return existing_reply(&request.id, &digest, record);
        }
        if inner.records.len() >= MAX_RECORDS {
            drop(permit);
            return Some(crate::protocol::Reply::err(
                &request.id,
                "LIMIT",
                "approval record limit reached; old request IDs are retained",
            ));
        }
        let pending = inner
            .records
            .values()
            .filter(|record| record.state == RecordState::Pending)
            .count();
        if pending >= MAX_PENDING {
            drop(permit);
            return Some(crate::protocol::Reply::err(
                &request.id,
                "LIMIT",
                "too many requests are waiting for manual approval",
            ));
        }

        let code = match random_code(&inner.by_code) {
            Some(code) => code,
            None => {
                drop(permit);
                return Some(crate::protocol::Reply::err(
                    &request.id,
                    "LIMIT",
                    "could not allocate a unique approval code",
                ));
            }
        };
        let prompt = match permit.is_some() {
            true => match render_prompt(request, &digest, &code, self.ttl) {
                Ok(prompt) => Some(prompt),
                Err(_) => {
                    drop(permit);
                    return Some(crate::protocol::Reply::err(
                        &request.id,
                        "LIMIT",
                        "approval command summary exceeds the TTY output limit",
                    ));
                }
            },
            false => None,
        };

        inner.by_code.insert(code.clone(), request.id.clone());
        inner.records.insert(
            request.id.clone(),
            Record {
                digest,
                state: RecordState::Pending,
                code,
                expires_at: Instant::now() + self.ttl,
            },
        );
        drop(inner);

        if let (Some(permit), Some(prompt)) = (permit, prompt) {
            permit.send(prompt);
        }
        Some(crate::protocol::Reply::err(
            &request.id,
            "APPROVAL_REQUIRED",
            "manual approval required on the Controller TTY; retry the same request ID after approval",
        ))
    }

    fn new_inner(
        session_approved: bool,
        prompt_tx: Option<Sender<Vec<u8>>>,
        ttl: Duration,
    ) -> Self {
        Self {
            session_approved,
            inner: Arc::new(Mutex::new(Inner {
                records: HashMap::new(),
                by_code: HashMap::new(),
            })),
            prompt_tx,
            ttl,
        }
    }

    fn apply_console_line(&self, line: &[u8]) {
        let Some((decision, code)) = parse_console_command(line) else {
            return;
        };
        let mut inner = lock(&self.inner);
        expire_locked(&mut inner, Instant::now());
        let Some(request_id) = inner.by_code.remove(code) else {
            return;
        };
        let Some(record) = inner.records.get_mut(&request_id) else {
            return;
        };
        if record.state != RecordState::Pending || record.code != code {
            return;
        }
        record.state = match decision {
            ConsoleDecision::Allow => RecordState::Approved,
            ConsoleDecision::Deny => RecordState::Denied,
        };
        record.expires_at = Instant::now();
    }

    fn expire(&self) {
        let mut inner = lock(&self.inner);
        expire_locked(&mut inner, Instant::now());
    }

    #[cfg(test)]
    pub(crate) fn new_for_test() -> Arc<Self> {
        Arc::new(Self::new_inner(false, None, APPROVAL_TTL))
    }

    #[cfg(test)]
    pub(crate) fn new_for_test_with_ttl(ttl: Duration) -> Arc<Self> {
        Arc::new(Self::new_inner(false, None, ttl))
    }

    #[cfg(test)]
    fn approval_code_for_test(&self, request_id: &str) -> String {
        lock(&self.inner)
            .records
            .get(request_id)
            .expect("test request record")
            .code
            .clone()
    }
}

fn requires_approval(op: &crate::protocol::Operation) -> bool {
    matches!(
        op,
        crate::protocol::Operation::Exec(_)
            | crate::protocol::Operation::Write {
                owner_token: None,
                ..
            }
    )
}

fn request_digest(request: &crate::protocol::Request) -> Result<[u8; 32]> {
    let encoded = serde_json::to_vec(request)?;
    let digest = Sha256::digest(encoded);
    let mut result = [0u8; 32];
    result.copy_from_slice(&digest);
    Ok(result)
}

fn existing_reply(
    request_id: &str,
    digest: &[u8; 32],
    record: &Record,
) -> Option<crate::protocol::Reply> {
    if record.digest != *digest {
        return Some(crate::protocol::Reply::err(
            request_id,
            "REQUEST_CONFLICT",
            "request ID was already bound to different request content",
        ));
    }
    match record.state {
        RecordState::Approved => None,
        RecordState::Pending => Some(crate::protocol::Reply::err(
            request_id,
            "APPROVAL_REQUIRED",
            "manual approval is still pending on the Controller TTY; retry the same request ID",
        )),
        RecordState::Denied => Some(crate::protocol::Reply::err(
            request_id,
            "DENIED",
            "request was denied or its manual approval expired; the request ID remains denied",
        )),
    }
}

fn expire_locked(inner: &mut Inner, now: Instant) {
    let expired: Vec<(String, String)> = inner
        .records
        .iter_mut()
        .filter_map(|(request_id, record)| {
            if record.state == RecordState::Pending && record.expires_at <= now {
                record.state = RecordState::Denied;
                Some((request_id.clone(), record.code.clone()))
            } else {
                None
            }
        })
        .collect();
    for (_, code) in expired {
        inner.by_code.remove(&code);
    }
}

fn random_code(existing: &HashMap<String, String>) -> Option<String> {
    let mut rng = rand::rngs::OsRng;
    for _ in 0..16 {
        let mut bytes = [0u8; CODE_LEN];
        rng.fill_bytes(&mut bytes);
        let code: String = bytes
            .iter()
            .map(|byte| CODE_ALPHABET[usize::from(*byte) & 31] as char)
            .collect();
        if !existing.contains_key(&code) {
            return Some(code);
        }
    }
    None
}

fn digest_hex(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(64);
    for byte in digest {
        result.push(HEX[usize::from(byte >> 4)] as char);
        result.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    result
}

fn render_prompt(
    request: &crate::protocol::Request,
    digest: &[u8; 32],
    code: &str,
    ttl: Duration,
) -> Result<Vec<u8>> {
    let (operation, incarnation, job_id, command) = match &request.op {
        crate::protocol::Operation::Exec(args) => (
            "exec",
            Some(args.expected_incarnation.clone()),
            None,
            serde_json::json!({
                "argv": args.argv,
                "cwd": args.cwd,
                "timeout_ms": args.timeout_ms,
                "stdin": args.stdin,
                "pty": args.pty,
                // The operator must see the actual values they are approving;
                // environment variables can change executable behaviour (e.g. LD_PRELOAD).
                "env": args.env,
            }),
        ),
        crate::protocol::Operation::Write {
            job_id, data, eof, ..
        } => (
            "write",
            None,
            Some(job_id.clone()),
            serde_json::json!({
                "job_id": job_id,
                "data": data,
                "data_bytes": data.len(),
                "eof": eof,
            }),
        ),
        _ => bail!("non-approvable operation"),
    };

    // serde_json is intentional here: every request-controlled string,
    // including command data and control characters, is escaped inside one
    // bounded JSON line.  No request-controlled text is interpolated raw.
    let mut output = serde_json::to_vec(&serde_json::json!({
        "event": "approval_required",
        "request_id": request.id,
        "operation": operation,
        "incarnation": incarnation,
        "job_id": job_id,
        "request_sha256": digest_hex(digest),
        "command": command,
        "approval_code": code,
        "expires_in_seconds": ttl.as_secs().max(1),
        "operator": "type allow <approval_code> or deny <approval_code>",
    }))?;
    ensure!(output.len() < MAX_TTY_OUTPUT, "TTY output line too large");
    output.push(b'\n');
    Ok(output)
}

fn parse_console_command(line: &[u8]) -> Option<(ConsoleDecision, &str)> {
    let mut line = line;
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line = &line[..line.len() - 1];
    }
    if line.len() > MAX_TTY_LINE || line.iter().any(u8::is_ascii_control) {
        return None;
    }
    let line = std::str::from_utf8(line).ok()?.trim();
    let mut words = line.split_ascii_whitespace();
    let decision = match words.next()? {
        "allow" => ConsoleDecision::Allow,
        "deny" => ConsoleDecision::Deny,
        _ => return None,
    };
    let code = words.next()?;
    if words.next().is_some()
        || !(6..=CODE_LEN).contains(&code.len())
        || !code.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return None;
    }
    Some((decision, code))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn open_tty() -> Result<AsyncFd<File>> {
    let path = b"/dev/tty\0";
    let flags = libc::O_RDWR | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC;
    let raw_fd = unsafe { libc::open(path.as_ptr().cast(), flags) };
    ensure!(
        raw_fd >= 0,
        "cannot open /dev/tty for manual approval: {}",
        io::Error::last_os_error()
    );
    // SAFETY: raw_fd is a fresh descriptor returned by open and is transferred
    // exactly once into OwnedFd.
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    ensure!(
        unsafe { libc::isatty(fd.as_raw_fd()) } == 1,
        "/dev/tty is not an interactive terminal"
    );
    AsyncFd::new(File::from(fd)).context("register /dev/tty with Tokio")
}

async fn console_loop(tty: AsyncFd<File>, mut prompt_rx: Receiver<Vec<u8>>, gate: Weak<Gate>) {
    let mut input = Vec::with_capacity(MAX_TTY_LINE);
    let mut discard_until_newline = false;
    let mut output: Option<(Vec<u8>, usize)> = None;
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let Some(gate_ref) = gate.upgrade() else {
            return;
        };
        gate_ref.expire();

        if output.is_some() {
            tokio::select! {
                result = write_pending(&tty, output.as_mut().expect("output exists")) => {
                    match result {
                        Ok(true) => output = None,
                        Ok(false) => {},
                        Err(_) => return,
                    }
                }
                result = read_chunk(&tty) => {
                    match result {
                        Ok(Some(chunk)) => feed_input(&gate_ref, &mut input, &mut discard_until_newline, &chunk),
                        Ok(None) | Err(_) => return,
                    }
                }
                _ = ticker.tick() => gate_ref.expire(),
            }
        } else {
            tokio::select! {
                prompt = prompt_rx.recv() => {
                    match prompt {
                        Some(prompt) => output = Some((prompt, 0)),
                        None => return,
                    }
                }
                result = read_chunk(&tty) => {
                    match result {
                        Ok(Some(chunk)) => feed_input(&gate_ref, &mut input, &mut discard_until_newline, &chunk),
                        Ok(None) | Err(_) => return,
                    }
                }
                _ = ticker.tick() => gate_ref.expire(),
            }
        }
    }
}

async fn read_chunk(tty: &AsyncFd<File>) -> io::Result<Option<Vec<u8>>> {
    loop {
        let mut ready = tty.readable().await?;
        match ready.try_io(|inner| {
            let mut bytes = [0u8; TTY_READ_CHUNK];
            let count = inner.get_ref().read(&mut bytes)?;
            Ok(bytes[..count].to_vec())
        }) {
            Ok(Ok(bytes)) if bytes.is_empty() => return Ok(None),
            Ok(Ok(bytes)) => return Ok(Some(bytes)),
            Ok(Err(error)) => return Err(error),
            Err(_) => continue,
        }
    }
}

async fn write_pending(tty: &AsyncFd<File>, pending: &mut (Vec<u8>, usize)) -> io::Result<bool> {
    let mut ready = tty.writable().await?;
    match ready.try_io(|inner| inner.get_ref().write(&pending.0[pending.1..])) {
        Ok(Ok(0)) => Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "TTY write made no progress",
        )),
        Ok(Ok(count)) => {
            pending.1 += count;
            Ok(pending.1 == pending.0.len())
        }
        Ok(Err(error)) => Err(error),
        Err(_) => Ok(false),
    }
}

fn feed_input(gate: &Gate, input: &mut Vec<u8>, discard_until_newline: &mut bool, chunk: &[u8]) {
    for &byte in chunk {
        if *discard_until_newline {
            if byte == b'\n' {
                *discard_until_newline = false;
                input.clear();
            }
            continue;
        }
        if byte == b'\n' {
            let mut line = std::mem::take(input);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            gate.apply_console_line(&line);
            continue;
        }
        if input.len() >= MAX_TTY_LINE {
            input.clear();
            *discard_until_newline = true;
            continue;
        }
        input.push(byte);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Exec, Operation, Request};
    use std::{collections::BTreeMap, thread, time::Duration};

    fn exec(id: &str, argv: &[&str]) -> Request {
        Request {
            id: id.into(),
            op: Operation::Exec(Exec {
                expected_incarnation: "inc-1".into(),
                argv: argv.iter().map(|arg| (*arg).into()).collect(),
                cwd: "/tmp".into(),
                env: BTreeMap::new(),
                timeout_ms: 60_000,
                stdin: false,
                pty: false,
            }),
        }
    }

    fn write_request(id: &str, data: &str) -> Request {
        Request {
            id: id.into(),
            op: Operation::Write {
                job_id: "job-1".into(),
                data: data.into(),
                eof: false,
                owner_token: None,
            },
        }
    }

    fn code(gate: &Gate, request_id: &str) -> String {
        gate.approval_code_for_test(request_id)
    }

    fn error_code(reply: crate::protocol::Reply) -> String {
        reply.error.expect("error reply").code
    }

    #[test]
    fn session_approval_bypasses_tty() {
        let (gate, console) = Gate::new(true).expect("session approval does not need a TTY");
        assert!(console.is_none());
        assert!(gate.check(&exec("session", &["echo", "ok"])).is_none());
        assert!(gate.check(&write_request("write", "echo ok\n")).is_none());
    }

    #[test]
    fn operator_token_bypasses_per_input_prompt_only() {
        let gate = Gate::new_for_test();
        let mut operator = write_request("owner-write", "typed by operator\n");
        if let Operation::Write { owner_token, .. } = &mut operator.op {
            *owner_token = Some("test-owner-capability".into());
        }
        assert!(gate.check(&operator).is_none());
        assert!(lock(&gate.inner).records.is_empty());
        assert_eq!(
            error_code(
                gate.check(&write_request("model-write", "model\n"))
                    .unwrap()
            ),
            "APPROVAL_REQUIRED"
        );
        // The Connector, not this local Gate, verifies the operator token
        // against a currently owned PTY before accepting its Write.
    }

    #[test]
    fn approval_is_bound_to_the_whole_request_and_duplicate_is_allowed_after_approval() {
        let gate = Gate::new_for_test();
        let request = exec("same-id", &["echo", "one"]);
        let reply = gate.check(&request).expect("new exec needs approval");
        assert_eq!(error_code(reply.clone()), "APPROVAL_REQUIRED");
        assert!(
            !serde_json::to_string(&reply)
                .expect("reply serializes")
                .contains(&code(&gate, "same-id"))
        );

        let conflict = gate
            .check(&exec("same-id", &["echo", "two"]))
            .expect("different content must conflict");
        assert_eq!(error_code(conflict), "REQUEST_CONFLICT");

        let approval_code = code(&gate, "same-id");
        gate.apply_console_line(format!("allow {approval_code}\n").as_bytes());
        assert!(gate.check(&request).is_none());
        assert!(gate.check(&request).is_none());
        // Replaying the consumed console command must not change the result.
        gate.apply_console_line(format!("allow {approval_code}\n").as_bytes());
        assert!(gate.check(&request).is_none());
    }

    #[test]
    fn deny_is_cached_and_does_not_reopen_the_request() {
        let gate = Gate::new_for_test();
        let request = exec("denied", &["touch", "/tmp/x"]);
        assert_eq!(
            error_code(gate.check(&request).unwrap()),
            "APPROVAL_REQUIRED"
        );
        let approval_code = code(&gate, "denied");
        gate.apply_console_line(format!("deny {approval_code}\n").as_bytes());
        assert_eq!(error_code(gate.check(&request).unwrap()), "DENIED");
        assert_eq!(error_code(gate.check(&request).unwrap()), "DENIED");
    }

    #[test]
    fn pending_approval_expires_to_denied() {
        let gate = Gate::new_for_test_with_ttl(Duration::from_millis(5));
        let request = exec("expires", &["echo", "later"]);
        assert_eq!(
            error_code(gate.check(&request).unwrap()),
            "APPROVAL_REQUIRED"
        );
        thread::sleep(Duration::from_millis(15));
        assert_eq!(error_code(gate.check(&request).unwrap()), "DENIED");
    }

    #[test]
    fn record_and_pending_limits_are_bounded_without_eviction() {
        let gate = Gate::new_for_test();
        for index in 0..MAX_PENDING {
            let id = format!("pending-{index}");
            assert_eq!(
                error_code(gate.check(&exec(&id, &["true"])).unwrap()),
                "APPROVAL_REQUIRED"
            );
        }
        assert_eq!(
            error_code(gate.check(&exec("pending-over", &["true"])).unwrap()),
            "LIMIT"
        );

        let first_code = code(&gate, "pending-0");
        gate.apply_console_line(format!("allow {first_code}\n").as_bytes());
        assert_eq!(
            error_code(gate.check(&exec("record-32", &["true"])).unwrap()),
            "APPROVAL_REQUIRED"
        );
        let record_32_code = code(&gate, "record-32");
        gate.apply_console_line(format!("deny {record_32_code}\n").as_bytes());

        // Move each additional record out of Pending so the pending cap does
        // not mask the record cap.  No old record is evicted.
        for index in 33..MAX_RECORDS {
            let id = format!("record-{index}");
            assert_eq!(
                error_code(gate.check(&exec(&id, &["true"])).unwrap()),
                "APPROVAL_REQUIRED"
            );
            let approval_code = code(&gate, &id);
            gate.apply_console_line(format!("deny {approval_code}\n").as_bytes());
        }
        assert_eq!(lock(&gate.inner).records.len(), MAX_RECORDS);
        assert_eq!(
            error_code(gate.check(&exec("record-over", &["true"])).unwrap()),
            "LIMIT"
        );
        assert_eq!(lock(&gate.inner).records.len(), MAX_RECORDS);
    }

    #[test]
    fn prompt_json_escapes_control_characters_and_keeps_code_out_of_reply() {
        let request = write_request("tty-json", "echo \"quoted\"\n\u{1b}[31mred\r\n");
        let digest = request_digest(&request).expect("digest");
        let prompt = render_prompt(&request, &digest, "ABCD2345", APPROVAL_TTL).expect("prompt");
        let line = &prompt[..prompt.len() - 1];
        assert!(line.iter().all(|byte| *byte >= 0x20 || *byte == b'\t'));
        assert!(
            std::str::from_utf8(line)
                .expect("JSON UTF-8")
                .contains("\\n")
        );

        let gate = Gate::new_for_test();
        let reply = gate.check(&request).expect("write needs approval");
        assert!(
            !serde_json::to_string(&reply)
                .expect("reply serializes")
                .contains("ABCD2345")
        );
    }
}
