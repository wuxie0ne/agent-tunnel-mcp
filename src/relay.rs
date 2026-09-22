use anyhow::{Result, ensure};
use futures_util::{SinkExt, StreamExt};
use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::{Arc, Mutex}, time::Duration};
use tokio::{io::AsyncWriteExt, net::{TcpListener, TcpStream}, sync::{Semaphore, mpsc}, time::Instant};
use tokio_tungstenite::{accept_hdr_async_with_config, tungstenite::{Message, handshake::server::{Request, Response, ErrorResponse}, http::StatusCode}};
use crate::{config::{self, RelayConfig}, protocol::{Packet, VERSION, valid_id}, transport::ws_config};

struct Peer { instance: Option<String>, tx: Option<mpsc::Sender<Message>>, connection: u64 }
struct Session { config: RelayConfig, peers: [Peer; 2] }
type Registry = Arc<Mutex<HashMap<String, Session>>>;

pub async fn run(listen: SocketAddr, files: Vec<PathBuf>) -> Result<()> {
    ensure!(listen.ip().is_loopback(), "Relay must bind loopback; expose it through a TLS tunnel/reverse proxy");
    ensure!(!files.is_empty() && files.len() <= 16, "provide 1..16 session files");
    let mut map = HashMap::new();
    for file in files {
        let c: RelayConfig = config::load(&file)?;
        ensure!(c.protocol == VERSION && valid_id(&c.session_id) && c.expires_at > config::now(), "invalid/expired relay session");
        ensure!(c.expires_at - config::now() <= 3600 && c.connector_hash.len() == 64 && c.controller_hash.len() == 64, "invalid session policy");
        let id = c.session_id.clone();
        ensure!(map.insert(id, Session { config: c, peers: std::array::from_fn(|_| Peer { instance: None, tx: None, connection: 0 }) }).is_none(), "duplicate session");
    }
    let registry = Arc::new(Mutex::new(map));
    let listener = TcpListener::bind(listen).await?;
    let limit = Arc::new(Semaphore::new(64));
    let mut workers = tokio::task::JoinSet::new();
    eprintln!("relay listening on {}; test-only trusted relay (no E2EE)", listener.local_addr()?);
    let mut stop = crate::shutdown_signal();
    loop {
        tokio::select! {
            _ = &mut stop => break,
            incoming = listener.accept() => {
                let (stream, _) = incoming?;
                let Ok(permit) = limit.clone().try_acquire_owned() else { continue; };
                let registry = registry.clone();
                workers.spawn(async move { let _permit = permit; if let Err(e) = peer(stream, registry).await { eprintln!("relay connection closed: {e}"); } });
            }
            _ = workers.join_next(), if !workers.is_empty() => {}
        }
    }
    workers.shutdown().await; Ok(())
}
fn reject(code: StatusCode, text: &str) -> ErrorResponse {
    let mut r = ErrorResponse::new(Some(text.to_owned())); *r.status_mut() = code;
    r.headers_mut().insert("Content-Length", text.len().to_string().parse().unwrap()); r
}
struct Reservation { registry: Registry, id: String, role: usize, connection: u64, committed: bool, had_identity: bool }
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut map = self.registry.lock().unwrap();
        if let Some(s) = map.get_mut(&self.id) {
            let peer = &mut s.peers[self.role];
            if peer.connection == self.connection { peer.tx = None; if !self.committed && !self.had_identity { peer.instance = None; } }
        }
    }
}
async fn peer(mut stream: TcpStream, registry: Registry) -> Result<()> {
    // A normal health probe is not a WebSocket handshake. Peek without consuming
    // the upgrade bytes; fragmented first lines have a bounded read deadline.
    let mut line = [0u8; 1024];
    let len = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let n = stream.peek(&mut line).await?;
            if n == 0 || n == line.len() || line[..n].contains(&b'\n') { return std::io::Result::Ok(n); }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }).await??;
    if line[..len].starts_with(b"GET /healthz HTTP/1.1\r\n") || line[..len].starts_with(b"GET /healthz HTTP/1.0\r\n") {
        tokio::time::timeout(Duration::from_secs(2), stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nok\n")).await??;
        return Ok(());
    }
    let (tx, mut rx) = mpsc::channel::<Message>(8);
    let mut reservation = None;
    let callback = |req: &Request, response: Response| {
        if req.uri().path() == "/healthz" { return Err(reject(StatusCode::OK, "ok\n")); }
        if req.headers().contains_key("origin") || req.uri().query().is_some() { return Err(reject(StatusCode::FORBIDDEN, "native clients only")); }
        let parts: Vec<_> = req.uri().path().split('/').collect();
        if parts.len() != 4 || parts[1] != "v1" { return Err(reject(StatusCode::NOT_FOUND, "not found")); }
        let role = match parts[2] { "connect" => 0, "control" => 1, _ => return Err(reject(StatusCode::NOT_FOUND, "not found")) };
        let token = req.headers().get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
        let instance = req.headers().get("x-agent-tunnel-instance").and_then(|v| v.to_str().ok()).filter(|v| valid_id(v));
        let (Some(token), Some(instance)) = (token, instance) else { return Err(reject(StatusCode::UNAUTHORIZED, "unauthorized")); };
        let mut map = registry.lock().unwrap();
        let Some(s) = map.get_mut(parts[3]) else { return Err(reject(StatusCode::UNAUTHORIZED, "unauthorized")); };
        let expected = if role == 0 { &s.config.connector_hash } else { &s.config.controller_hash };
        if s.config.expires_at <= config::now() || !config::token_matches(token, expected) { return Err(reject(StatusCode::UNAUTHORIZED, "unauthorized")); }
        let p = &mut s.peers[role];
        if p.tx.is_some() || p.instance.as_deref().is_some_and(|old| old != instance) { return Err(reject(StatusCode::CONFLICT, "role already bound; restart requires a new session")); }
        let had_identity = p.instance.is_some();
        p.instance = Some(instance.into()); p.connection += 1; p.tx = Some(tx.clone());
        reservation = Some(Reservation { registry: registry.clone(), id: parts[3].into(), role, connection: p.connection, committed: false, had_identity });
        Ok(response)
    };
    let mut ws = tokio::time::timeout(Duration::from_secs(5), accept_hdr_async_with_config(stream, callback, Some(ws_config()))).await??;
    let mut reservation = reservation.expect("authenticated upgrade"); reservation.committed = true;
    let expiry = registry.lock().unwrap()[&reservation.id].config.expires_at;
    let deadline = Instant::now() + Duration::from_secs(expiry.saturating_sub(config::now()));
    let mut tick = tokio::time::interval(Duration::from_secs(10)); let mut last = Instant::now();
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            _ = tick.tick() => {
                ensure!(last.elapsed() < Duration::from_secs(30), "peer heartbeat expired");
                tokio::time::timeout(Duration::from_secs(5), ws.send(Message::Ping(Vec::new().into()))).await??;
            }
            Some(message) = rx.recv() => { tokio::time::timeout(Duration::from_secs(5), ws.send(message)).await??; }
            incoming = ws.next() => {
                let Some(message) = incoming else { break; }; last = Instant::now();
                match message? {
                    Message::Text(text) => {
                        let packet: Packet = serde_json::from_str(&text)?;
                        let valid = matches!((&packet, reservation.role),
                            (Packet::Reply { version: VERSION, .. }, 0) |
                            (Packet::Request { version: VERSION, .. }, 1) |
                            (Packet::Lease { version: VERSION }, 1));
                        ensure!(valid, "role/protocol violation");
                        let dest = registry.lock().unwrap()[&reservation.id].peers[1 - reservation.role].tx.clone();
                        if let Some(dest) = dest {
                            // A saturated destination is disconnected rather than consuming unlimited memory.
                            ensure!(dest.try_send(Message::Text(text)).is_ok(), "destination overloaded/offline");
                        } else {
                            let error = Packet::RelayError { code: "PEER_OFFLINE".into() };
                            crate::transport::send(&mut ws, &error).await?;
                        }
                    }
                    Message::Ping(_) | Message::Pong(_) => { tokio::time::timeout(Duration::from_secs(5), ws.flush()).await??; }
                    Message::Close(_) => break,
                    _ => anyhow::bail!("text protocol required"),
                }
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), ws.close(None)).await;
    Ok(())
}
