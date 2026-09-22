use anyhow::{Result, ensure};
use std::{path::PathBuf, time::Duration};
use tokio::time::Instant;
use crate::{config::{self, EndpointConfig}, executor::Manager, protocol::*, transport};

pub async fn run(file: PathBuf, state_dir: Option<PathBuf>, allow_exec: bool, lease_secs: u64) -> Result<()> {
    ensure!(allow_exec, "remote arbitrary execution requires explicit --allow-exec (session-wide approval, test environments only)");
    ensure!((5..=60).contains(&lease_secs), "lease-secs must be 5..60");
    let config: EndpointConfig = config::load(&file)?; config::validate_endpoint(&config, "connector")?;
    let info = TargetInfo { session_id: config.session_id.clone(), target_id: config.target_id.clone(), incarnation: config::random_id(),
        name: config.name.clone(), os: std::env::consts::OS.into(), arch: std::env::consts::ARCH.into(),
        // SAFETY: read-only process identity system calls.
        uid: unsafe { libc::geteuid() }, gid: unsafe { libc::getegid() }, cwd: std::env::current_dir()?.display().to_string(),
        expires_at: config.expires_at, protocol: VERSION, approval: "session-approved".into(), end_to_end_encrypted: false };
    let state_dir = state_dir.unwrap_or_else(|| file.parent().unwrap_or(std::path::Path::new(".")).join("connector-state"));
    let mut manager = Manager::new(info, &state_dir)?;
    eprintln!("connector target={} incarnation={} uid={} state={}", manager.info.target_id, manager.info.incarnation, manager.info.uid, state_dir.display());
    eprintln!("WARNING: arbitrary exec enabled; relay is trusted; no E2EE; PID records do not provide crash recovery");
    let deadline = Instant::now() + Duration::from_secs(config.expires_at.saturating_sub(config::now()));
    let mut last_lease = Instant::now(); let lease = Duration::from_secs(lease_secs);
    let mut stop = crate::shutdown_signal(); let mut attempt = 0u32;
    let result = loop {
        let remaining = deadline.min(last_lease + lease);
        let connect = tokio::select! {
            _ = &mut stop => break Ok(()),
            _ = tokio::time::sleep_until(remaining) => break Err(anyhow::anyhow!("session TTL or controller lease expired")),
            result = transport::connect(&config, &manager.info.incarnation) => result,
        };
        if let Ok(mut ws) = connect {
            attempt = 0; eprintln!("connector transport connected");
            loop {
                let remaining = deadline.min(last_lease + lease);
                tokio::select! {
                    _ = &mut stop => { manager.shutdown().await; return Ok(()); }
                    _ = tokio::time::sleep_until(remaining) => { manager.shutdown().await; anyhow::bail!("session TTL or controller lease expired"); }
                    packet = transport::receive(&mut ws) => {
                        let text = match packet { Ok(Some(text)) => text, _ => break };
                        match serde_json::from_str::<Packet>(&text) {
                            Ok(Packet::Lease { version: VERSION }) => { last_lease = Instant::now(); }
                            Ok(Packet::Request { version: VERSION, request }) => {
                                // The relay role filter is the outer trust boundary in this test-only version.
                                let reply = if last_lease.elapsed() >= lease { Reply::err(&request.id, "LEASE_EXPIRED", "controller lease expired") }
                                    else { manager.handle(request) };
                                if transport::send(&mut ws, &Packet::Reply { version: VERSION, reply }).await.is_err() { break; }
                            }
                            Ok(Packet::RelayError { .. }) => {}
                            _ => break,
                        }
                    }
                }
            }
            eprintln!("connector transport disconnected; jobs retained only until controller lease / TTL");
        } else { eprintln!("connector connection unavailable; retrying within lease"); }
        attempt = (attempt + 1).min(4);
        let backoff = Duration::from_millis(250 * (1u64 << attempt));
        tokio::select! {
            _ = &mut stop => break Ok(()),
            _ = tokio::time::sleep_until(deadline.min(last_lease + lease)) => break Err(anyhow::anyhow!("session TTL or controller lease expired")),
            _ = tokio::time::sleep(backoff) => {}
        }
    };
    manager.shutdown().await; result
}
