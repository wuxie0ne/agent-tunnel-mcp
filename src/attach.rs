//! A deliberately small, human-only PTY view. This is not a general TUI:
//! terminal escape sequences from the target are printed as inert text.
use crate::{
    config::random_id,
    controller,
    protocol::{JobView, Operation, Reply, Request, valid_id},
    terminal::OperatorTty,
};
use anyhow::{Context, Result, bail, ensure};
use std::{future::Future, path::Path, time::Duration};

async fn call_or_stop<F: Future<Output = ()> + Unpin>(
    socket: &Path,
    op: Operation,
    id: String,
    stop: &mut F,
) -> Result<Option<Reply>> {
    tokio::select! {
        _ = stop => Ok(None),
        response = controller::call(socket, Request { id, op }) => response.map(Some),
    }
}

async fn tty_write_or_stop<F: Future<Output = ()> + Unpin>(
    tty: &OperatorTty,
    bytes: &[u8],
    stop: &mut F,
) -> Result<bool> {
    tokio::select! {
        biased;
        _ = stop => Ok(false),
        result = tty.write_all(bytes) => { result?; Ok(true) },
    }
}

fn accepted(reply: Reply) -> Result<serde_json::Value> {
    if let Some(fault) = reply.error {
        // Neither token nor remote output is ever included in these errors.
        bail!("operator request rejected: {}", fault.code);
    }
    reply.result.context("operator request returned no result")
}

fn inert(text: &str) -> String {
    let mut safe = String::new();
    for c in text.chars() {
        match c {
            '\n' => safe.push_str("\r\n"),
            '\t' => safe.push_str("\\t"),
            c if c.is_control() => safe.extend(c.escape_default()),
            c => safe.push(c),
        }
    }
    safe
}

pub(crate) async fn run(socket: &Path, job: &str, incarnation: &str) -> Result<()> {
    ensure!(
        valid_id(job) && valid_id(incarnation),
        "invalid job or incarnation ID"
    );
    // Install signal handlers before entering raw mode; the generic shutdown
    // future only registers them when first polled, which is too late here.
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut stop = Box::pin(async move {
        tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
    });
    // Check operator presence before any remote Takeover request is sent.
    let tty = OperatorTty::open().context("attach needs a real operator TTY")?;
    let take_id = random_id();
    let (token, mut cursor) = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        loop {
            let request = Operation::Takeover {
                job_id: job.into(),
                expected_incarnation: incarnation.into(),
                owner_token: None,
            };
            let Some(reply) = call_or_stop(socket, request, take_id.clone(), &mut stop).await?
            else {
                return Ok(());
            };
            if reply
                .error
                .as_ref()
                .is_some_and(|e| e.code == "INPUT_BUSY" || e.code == "HANDOFF_DRAINING")
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            let data = accepted(reply)?;
            let token = data
                .get("owner_token")
                .and_then(serde_json::Value::as_str)
                .filter(|token| token.len() == 64)
                .context("takeover returned no valid operator token")?
                .to_owned();
            let cursor = data
                .get("next_cursor")
                .and_then(serde_json::Value::as_u64)
                .context("takeover returned no cursor")?;
            break (token, cursor);
        }
    };
    let result = match tty_write_or_stop(
        &tty,
        b"[operator attach: Ctrl+] detaches; terminal escapes shown literally]\r\n",
        &mut stop,
    )
    .await
    {
        Ok(true) => {
            interact(
                &tty,
                socket,
                job,
                incarnation,
                &token,
                &mut cursor,
                &mut stop,
            )
            .await
        }
        Ok(false) => Ok(()),
        Err(error) => Err(error),
    };
    // Interrupted IPC or SIGINT cannot authorize a replay. Release is best
    // effort; the Connector's independent 30s sweeper is the fallback.
    let release = controller::call(
        socket,
        Request {
            id: random_id(),
            op: Operation::Release {
                job_id: job.into(),
                expected_incarnation: incarnation.into(),
                owner_token: token,
            },
        },
    );
    let _ = tokio::time::timeout(Duration::from_secs(2), release).await;
    result
}

async fn interact<F: Future<Output = ()> + Unpin>(
    tty: &OperatorTty,
    socket: &Path,
    job: &str,
    incarnation: &str,
    token: &str,
    cursor: &mut u64,
    stop: &mut F,
) -> Result<()> {
    let mut reading = tokio::time::interval(Duration::from_millis(150));
    let mut renewing = tokio::time::interval(Duration::from_secs(5));
    renewing.tick().await; // first renewal after five seconds, not immediately
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut utf8_pending = Vec::new();
    loop {
        let mut input = [0u8; 512];
        tokio::select! {
            biased;
            _ = &mut *stop => break,
            _ = renewing.tick() => {
                let op = Operation::Takeover {
                    job_id: job.into(), expected_incarnation: incarnation.into(), owner_token: Some(token.into()),
                };
                let Some(reply) = call_or_stop(socket, op, random_id(), stop).await? else { break; };
                accepted(reply)?;
            }
            _ = reading.tick() => {
                let op = Operation::Read { job_id: job.into(), cursor: *cursor, owner_token: Some(token.into()) };
                let Some(reply) = call_or_stop(socket, op, random_id(), stop).await? else { break; };
                let view: JobView = serde_json::from_value(accepted(reply)?)?;
                if let Some(gap) = view.dropped_before_cursor
                    && !tty_write_or_stop(tty, format!("\r\n[output gap: cursor advanced to {gap}]\r\n").as_bytes(), stop).await? {
                    break;
                }
                for output in view.events {
                    if !tty_write_or_stop(tty, inert(&output.text).as_bytes(), stop).await? { return Ok(()); }
                }
                *cursor = view.next_cursor;
                if view.state != "running" { break; }
            }
            _ = winch.recv() => {
                if let Some((rows, cols)) = tty.size() {
                    let op = Operation::Resize { job_id: job.into(), rows, cols, owner_token: Some(token.into()) };
                    let Some(reply) = call_or_stop(socket, op, random_id(), stop).await? else { break; };
                    accepted(reply)?;
                }
            }
            received = tty.read(&mut input) => {
                let count = received?;
                if count == 0 { break; }
                let detach = input[..count].iter().position(|b| *b == 0x1d);
                utf8_pending.extend_from_slice(&input[..detach.unwrap_or(count)]);
                ensure!(utf8_pending.len() <= 4096, "TTY input exceeded write limit");
                match std::str::from_utf8(&utf8_pending) {
                    Ok(data) if !data.is_empty() => {
                        let op = Operation::Write {
                            job_id: job.into(), data: data.into(), eof: false, owner_token: Some(token.into()),
                        };
                        let request_id = random_id();
                        // Never generate another request ID for the same bytes
                        // after an uncertain IPC result. No automatic replay.
                        let Some(reply) = call_or_stop(socket, op, request_id, stop).await? else { break; };
                        accepted(reply)?;
                        utf8_pending.clear();
                    }
                    Ok(_) => {}
                    Err(err) if err.error_len().is_none() => {} // incomplete UTF-8 across reads
                    Err(_) => bail!("operator input must be UTF-8; bytes were not sent"),
                }
                if detach.is_some() { break; }
            }
        }
    }
    Ok(())
}
