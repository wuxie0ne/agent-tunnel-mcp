use crate::{
    config::{self, EndpointConfig},
    executor::Manager,
    protocol::*,
    transport,
};
use anyhow::{Result, ensure};
use std::{path::PathBuf, time::Duration};
use tokio::time::Instant;

/// A lease renewal received after the old deadline cannot resurrect the
/// authorization, even if a socket receive and timeout were both ready.
fn expired(now: Instant, deadline: Instant, last_lease: Instant, lease: Duration) -> bool {
    now >= deadline || now >= last_lease + lease
}

pub async fn run(
    file: PathBuf,
    state_dir: Option<PathBuf>,
    allow_exec: bool,
    lease_secs: u64,
) -> Result<()> {
    ensure!(
        allow_exec,
        "remote arbitrary execution requires explicit --allow-exec (session-wide approval, test environments only)"
    );
    ensure!((5..=60).contains(&lease_secs), "lease-secs must be 5..60");
    let config: EndpointConfig = config::load(&file)?;
    config::validate_endpoint(&config, "connector")?;
    let info = TargetInfo {
        session_id: config.session_id.clone(),
        target_id: config.target_id.clone(),
        incarnation: config::random_id(),
        name: config.name.clone(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        // SAFETY: read-only process identity system calls.
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
        cwd: std::env::current_dir()?.display().to_string(),
        expires_at: config.expires_at,
        protocol: VERSION,
        approval: "session-approved".into(),
        end_to_end_encrypted: true,
    };
    let state_dir = state_dir.unwrap_or_else(|| {
        file.parent()
            .unwrap_or(std::path::Path::new("."))
            .join("connector-state")
    });
    let mut manager = Manager::new(info, &state_dir)?;
    let prologue = config::prologue(&config);
    eprintln!(
        "connector target={} incarnation={} uid={} state={}",
        manager.info.target_id,
        manager.info.incarnation,
        manager.info.uid,
        state_dir.display()
    );
    eprintln!(
        "WARNING: arbitrary exec enabled; end-to-end Noise required; PID records do not provide crash recovery"
    );
    let deadline =
        Instant::now() + Duration::from_secs(config.expires_at.saturating_sub(config::now()));
    let mut last_lease = Instant::now();
    let lease = Duration::from_secs(lease_secs);
    let mut stop = crate::shutdown_signal();
    let mut attempt = 0u32;
    let result = loop {
        if expired(Instant::now(), deadline, last_lease, lease) {
            break Err(anyhow::anyhow!("session TTL or controller lease expired"));
        }
        let remaining = deadline.min(last_lease + lease);
        let connect = tokio::select! {
            _ = &mut stop => break Ok(()),
            _ = tokio::time::sleep_until(remaining) => break Err(anyhow::anyhow!("session TTL or controller lease expired")),
            result = transport::connect(&config, &manager.info.incarnation) => result,
        };
        if expired(Instant::now(), deadline, last_lease, lease) {
            break Err(anyhow::anyhow!("session TTL or controller lease expired"));
        }
        if let Ok(mut ws) = connect {
            attempt = 0;
            eprintln!("connector transport connected; waiting for authenticated controller");
            let mut channel: Option<crate::crypto::Channel> = None;
            let mut authorized_lease = false;
            loop {
                if expired(Instant::now(), deadline, last_lease, lease) {
                    manager.shutdown().await;
                    anyhow::bail!("session TTL or controller lease expired");
                }
                let remaining = deadline.min(last_lease + lease);
                tokio::select! {
                    biased;
                    _ = &mut stop => { manager.shutdown().await; return Ok(()); }
                    _ = tokio::time::sleep_until(remaining) => { manager.shutdown().await; anyhow::bail!("session TTL or controller lease expired"); }
                    packet = transport::receive(&mut ws) => {
                        let text = match packet { Ok(Some(text)) => text, _ => break };
                        // A ready read must never win against either expiration.
                        // In particular an already expired lease cannot be renewed.
                        if expired(Instant::now(), deadline, last_lease, lease) {
                            manager.shutdown().await;
                            anyhow::bail!("session TTL or controller lease expired");
                        }
                        let frame = match serde_json::from_str::<crate::crypto::Frame>(&text) { Ok(f) => f, Err(_) => break };
                        match frame {
                            crate::crypto::Frame::Init { .. } => {
                                match crate::crypto::respond(&config.channel_key, &prologue, frame) {
                                    Ok((ready, response)) => {
                                        channel = Some(ready); authorized_lease = false;
                                        if transport::send(&mut ws, &response).await.is_err() { break; }
                                    }
                                    Err(_) => { eprintln!("end-to-end authentication failed"); break; }
                                }
                            }
                            crate::crypto::Frame::Data { .. } => {
                                let Some(channel) = &mut channel else { break; };
                                let bytes = match channel.open(frame) { Ok(Some(bytes)) => bytes, Ok(None) => continue, Err(_) => break };
                                match serde_json::from_slice::<Packet>(&bytes) {
                                    Ok(Packet::Lease { version: VERSION }) => {
                                        if expired(Instant::now(), deadline, last_lease, lease) {
                                            manager.shutdown().await;
                                            anyhow::bail!("session TTL or controller lease expired");
                                        }
                                        last_lease = Instant::now(); authorized_lease = true;
                                    }
                                    Ok(Packet::Request { version: VERSION, request }) => {
                                        let reply = if !authorized_lease || expired(Instant::now(), deadline, last_lease, lease) {
                                            Reply::err(&request.id, "LEASE_EXPIRED", "authenticated controller lease required")
                                        } else { manager.handle(request) };
                                        if transport::send_encrypted(&mut ws, channel, &Packet::Reply { version: VERSION, reply }).await.is_err() { break; }
                                    }
                                    _ => break,
                                }
                            }
                            crate::crypto::Frame::RelayEvent { .. } => { channel = None; authorized_lease = false; }
                            _ => break,
                        }
                    }
                }
            }
            eprintln!(
                "connector transport disconnected; jobs retained only until controller lease / TTL"
            );
        } else {
            eprintln!("connector connection unavailable; retrying within lease");
        }
        attempt = (attempt + 1).min(4);
        let backoff = Duration::from_millis(250 * (1u64 << attempt));
        tokio::select! {
            _ = &mut stop => break Ok(()),
            _ = tokio::time::sleep_until(deadline.min(last_lease + lease)) => break Err(anyhow::anyhow!("session TTL or controller lease expired")),
            _ = tokio::time::sleep(backoff) => {}
        }
    };
    manager.shutdown().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deadline_is_strict_and_cannot_be_renewed_after_expiry() {
        let start = Instant::now();
        let ttl = start + Duration::from_secs(10);
        let lease = Duration::from_secs(5);
        assert!(!expired(start + Duration::from_secs(4), ttl, start, lease));
        assert!(expired(start + Duration::from_secs(5), ttl, start, lease));
        // Receiving a queued renewal at exactly five seconds is too late.
        assert!(expired(start + Duration::from_secs(6), ttl, start, lease));
        let last_lease = start + Duration::from_secs(8);
        assert!(!expired(
            start + Duration::from_secs(9),
            ttl,
            last_lease,
            lease
        ));
        assert!(expired(ttl, ttl, last_lease, lease));
    }
}
