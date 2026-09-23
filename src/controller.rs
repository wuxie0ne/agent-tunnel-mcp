use crate::{
    config::{self, EndpointConfig},
    protocol::*,
    transport,
};
use anyhow::{Result, ensure};
use std::{
    collections::HashMap,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::BufReader,
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot, watch},
    time::Instant,
};

type Work = (Request, oneshot::Sender<Reply>);
struct SocketGuard {
    path: PathBuf,
    inode: u64,
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|m| m.ino() == self.inode) {
            let _ = fs::remove_file(&self.path);
        }
    }
}
pub async fn run(file: PathBuf, socket: PathBuf, accept_risk: bool) -> Result<()> {
    let (gate, console) = crate::approval::Gate::new(accept_risk)?;
    let config: EndpointConfig = config::load(&file)?;
    config::validate_endpoint(&config, "controller")?;
    config::private_directory(socket.parent().unwrap_or(Path::new(".")))?;
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let _guard = SocketGuard {
        inode: fs::symlink_metadata(&socket)?.ino(),
        path: socket.clone(),
    };
    let (tx, rx) = mpsc::channel::<Work>(32);
    let (stop_tx, stop_rx) = watch::channel(false);
    let expires =
        Instant::now() + Duration::from_secs(config.expires_at.saturating_sub(config::now()));
    let mut network = tokio::spawn(network(config, rx, stop_rx));
    let mut clients = tokio::task::JoinSet::new();
    if let Some(console) = console {
        clients.spawn(console);
    }
    let mut stop = crate::shutdown_signal();
    eprintln!(
        "controller IPC {}; use info to confirm the current target incarnation",
        socket.display()
    );
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = tokio::time::sleep_until(expires) => break,
            _ = &mut network => { break; }
            incoming = listener.accept() => {
                let (stream, _) = incoming?;
                if clients.len() >= 32 { continue; }
                let tx = tx.clone(); let gate = gate.clone();
                clients.spawn(async move { let _ = serve(stream, tx, gate).await; });
            }
            _ = clients.join_next(), if !clients.is_empty() => {}
        }
    }
    let _ = stop_tx.send(true);
    if !network.is_finished() {
        let _ = network.await;
    }
    clients.shutdown().await;
    Ok(())
}
async fn serve(
    stream: UnixStream,
    tx: mpsc::Sender<Work>,
    gate: std::sync::Arc<crate::approval::Gate>,
) -> Result<()> {
    ensure!(
        stream.peer_cred()?.uid() == unsafe { libc::geteuid() },
        "IPC peer UID mismatch"
    );
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let Some(line) =
        tokio::time::timeout(Duration::from_secs(5), transport::read_line(&mut reader)).await??
    else {
        return Ok(());
    };
    let request: Request = serde_json::from_slice(&line)?;
    let id = request.id.clone();
    ensure!(valid_id(&id), "invalid request ID");
    if let Some(denied) = gate.check(&request) {
        tokio::time::timeout(
            Duration::from_secs(5),
            transport::write_line(&mut write, &denied),
        )
        .await??;
        return Ok(());
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    let reply = if tx.try_send((request, reply_tx)).is_err() {
        Reply::err(&id, "RESOURCE_LIMIT", "controller overloaded")
    } else {
        match tokio::time::timeout(Duration::from_secs(12), reply_rx).await {
            Ok(Ok(reply)) => reply,
            _ => Reply::err(
                &id,
                "EXECUTION_UNKNOWN",
                "controller response unavailable; do not repeat an exec with a new request ID",
            ),
        }
    };
    tokio::time::timeout(
        Duration::from_secs(5),
        transport::write_line(&mut write, &reply),
    )
    .await??;
    Ok(())
}
struct Pending {
    reply: oneshot::Sender<Reply>,
    deadline: Instant,
}
fn fail_pending(pending: &mut HashMap<String, Pending>, message: &str) {
    for (id, p) in pending.drain() {
        let _ = p.reply.send(Reply::err(&id, "EXECUTION_UNKNOWN", message));
    }
}
async fn network(
    config: EndpointConfig,
    mut work: mpsc::Receiver<Work>,
    mut stop: watch::Receiver<bool>,
) {
    use crate::crypto::{Channel, Frame, Initiator};
    let instance = config::random_id();
    let mut pending = HashMap::<String, Pending>::new();
    let prologue = config::prologue(&config);
    let mut attempt = 0u32;
    loop {
        let connecting = transport::connect(&config, &instance);
        tokio::pin!(connecting);
        let connected = loop {
            tokio::select! {
                _ = stop.changed() => return,
                Some((request, reply)) = work.recv() => { let _ = reply.send(Reply::err(&request.id, "TARGET_OFFLINE", "controller transport reconnecting; request was not sent")); }
                result = &mut connecting => break result,
            }
        };
        if let Ok(mut ws) = connected {
            attempt = 0;
            eprintln!("controller transport connected; authenticating end-to-end channel");
            let mut channel: Option<Channel> = None;
            let mut handshake: Option<Initiator> = None;
            let mut handshake_at = Instant::now() - Duration::from_secs(10);
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = stop.changed() => { fail_pending(&mut pending, "controller stopping"); return; }
                    _ = tick.tick() => {
                        let expired: Vec<_> = pending.iter().filter(|(_,p)| p.deadline <= Instant::now()).map(|(id,_)| id.clone()).collect();
                        for id in expired { let p = pending.remove(&id).unwrap(); let _ = p.reply.send(Reply::err(&id, "EXECUTION_UNKNOWN", "response timeout; reuse the SAME request ID to query an exec")); }
                        if let Some(channel) = &mut channel {
                            if transport::send_encrypted(&mut ws, channel, &Packet::Lease { version: VERSION }).await.is_err() { break; }
                        } else if handshake_at.elapsed() >= Duration::from_secs(3) {
                            let Ok((state, frame)) = Initiator::new(&config.channel_key, &prologue) else { break; };
                            handshake = Some(state); handshake_at = Instant::now();
                            if transport::send(&mut ws, &frame).await.is_err() { break; }
                        }
                    }
                    Some((request, reply)) = work.recv() => {
                        if reply.is_closed() { continue; }
                        let Some(channel) = &mut channel else { let _ = reply.send(Reply::err(&request.id, "TARGET_OFFLINE", "end-to-end channel not established; request was not sent")); continue; };
                        if pending.contains_key(&request.id) { let _ = reply.send(Reply::err(&request.id, "REQUEST_IN_FLIGHT", "same request ID already in flight")); continue; }
                        if pending.len() >= 32 { let _ = reply.send(Reply::err(&request.id, "RESOURCE_LIMIT", "too many pending requests")); continue; }
                        let id = request.id.clone(); pending.insert(id, Pending { reply, deadline: Instant::now() + Duration::from_secs(8) });
                        if transport::send_encrypted(&mut ws, channel, &Packet::Request { version: VERSION, request }).await.is_err() { break; }
                    }
                    message = transport::receive(&mut ws) => {
                        let text = match message { Ok(Some(text)) => text, _ => break };
                        let frame = match serde_json::from_str::<Frame>(&text) { Ok(f) => f, Err(_) => break };
                        match frame {
                            Frame::Response { .. } => {
                                let Some(initiator) = handshake.take() else { break; };
                                match initiator.finish(frame) {
                                    Ok(mut ready) => {
                                        if transport::send_encrypted(&mut ws, &mut ready, &Packet::Lease { version: VERSION }).await.is_err() { break; }
                                        channel = Some(ready); eprintln!("controller end-to-end channel authenticated");
                                    }
                                    Err(_) => { eprintln!("end-to-end authentication failed; refusing plaintext fallback"); break; }
                                }
                            }
                            Frame::Data { .. } => {
                                let Some(channel) = &mut channel else { break; };
                                match channel.open(frame) {
                                    Ok(Some(bytes)) => match serde_json::from_slice::<Packet>(&bytes) {
                                        Ok(Packet::Reply { version: VERSION, reply }) => {
                                            if let Some(p) = pending.remove(&reply.id) { let _ = p.reply.send(reply); }
                                        }
                                        _ => break,
                                    },
                                    Ok(None) => {},
                                    Err(_) => { eprintln!("end-to-end frame rejected"); break; },
                                }
                            }
                            Frame::RelayEvent { code } => {
                                if code == "PEER_CONNECTED" || code == "PEER_DISCONNECTED" || code == "PEER_OFFLINE" {
                                    fail_pending(&mut pending, "peer connection changed; execution may be unknown; no automatic retry");
                                    channel = None; handshake = None;
                                    // Pace retries even when the relay says the peer is offline.
                                    if code == "PEER_CONNECTED" { handshake_at = Instant::now() - Duration::from_secs(10); }
                                } else { break; }
                            }
                            _ => break,
                        }
                    }
                }
            }
            fail_pending(
                &mut pending,
                "transport disconnected; execution may have occurred; no automatic retry",
            );
            eprintln!("controller transport disconnected; reconnecting without replaying requests");
        }
        attempt = (attempt + 1).min(4);
        let pause = tokio::time::sleep(Duration::from_millis(250 * (1u64 << attempt)));
        tokio::pin!(pause);
        loop {
            tokio::select! {
                _ = stop.changed() => return,
                _ = &mut pause => break,
                Some((request, reply)) = work.recv() => { let _ = reply.send(Reply::err(&request.id, "TARGET_OFFLINE", "transport unavailable; request was not sent")); }
            }
        }
    }
}

pub async fn call(socket: &Path, request: Request) -> Result<Reply> {
    config::private_directory(socket.parent().unwrap_or(Path::new(".")))?;
    let stream = UnixStream::connect(socket).await?;
    ensure!(
        stream.peer_cred()?.uid() == unsafe { libc::geteuid() },
        "IPC server UID mismatch"
    );
    let (read, mut write) = stream.into_split();
    transport::write_line(&mut write, &request).await?;
    let mut reader = BufReader::new(read);
    let line = tokio::time::timeout(Duration::from_secs(15), transport::read_line(&mut reader))
        .await??
        .ok_or_else(|| {
            anyhow::anyhow!("controller closed before reply; execution status unknown")
        })?;
    let reply: Reply = serde_json::from_slice(&line)?;
    ensure!(reply.id == request.id, "response request ID mismatch");
    Ok(reply)
}
