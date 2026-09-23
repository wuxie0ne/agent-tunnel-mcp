use crate::{
    config::{self, RelayConfig},
    crypto::Frame,
    protocol::{VERSION, valid_id},
    transport::ws_config,
};
use anyhow::{Result, ensure};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::{Semaphore, mpsc},
    time::Instant,
};
use tokio_tungstenite::{
    accept_hdr_async_with_config,
    tungstenite::{
        Message,
        handshake::server::{ErrorResponse, Request, Response},
        http::StatusCode,
    },
};

struct Peer {
    instance: Option<String>,
    tx: Option<mpsc::Sender<Message>>,
    connection: u64,
    connected: bool,
}
struct Session {
    config: RelayConfig,
    source: PathBuf,
    revoked: bool,
    peers: [Peer; 2],
}
type Registry = Arc<Mutex<HashMap<String, Session>>>;

pub async fn run(
    listen: SocketAddr,
    files: Vec<PathBuf>,
    admin_socket: Option<PathBuf>,
) -> Result<()> {
    ensure!(
        listen.ip().is_loopback(),
        "Relay must bind loopback; expose it through a TLS tunnel/reverse proxy"
    );
    ensure!(
        !files.is_empty() && files.len() <= 16,
        "provide 1..16 session files"
    );
    let admin_socket = admin_socket.unwrap_or_else(|| {
        files[0]
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("relay-admin.sock")
    });
    let mut map = HashMap::new();
    for file in files {
        let c: RelayConfig = config::load(&file)?;
        ensure!(
            c.protocol == VERSION && valid_id(&c.session_id) && c.expires_at > config::now(),
            "invalid/expired relay session"
        );
        ensure!(
            c.expires_at - config::now() <= 3600
                && c.connector_hash.len() == 64
                && c.controller_hash.len() == 64,
            "invalid session policy"
        );
        ensure!(
            !revoked_path(&file).try_exists()?,
            "session has a durable revocation marker; create a fresh session"
        );
        let id = c.session_id.clone();
        ensure!(
            map.insert(
                id,
                Session {
                    config: c,
                    source: file,
                    revoked: false,
                    peers: std::array::from_fn(|_| Peer {
                        instance: None,
                        tx: None,
                        connection: 0,
                        connected: false
                    })
                }
            )
            .is_none(),
            "duplicate session"
        );
    }
    let registry = Arc::new(Mutex::new(map));
    let listener = TcpListener::bind(listen).await?;
    let limit = Arc::new(Semaphore::new(64));
    let mut workers = tokio::task::JoinSet::new();
    let (admin_listener, _admin_guard) = crate::admin::bind(&admin_socket)?;
    let admin_registry = registry.clone();
    workers.spawn(async move {
        crate::admin::serve(
            admin_listener,
            Arc::new(move |action| administer(&admin_registry, action)),
        )
        .await;
    });
    eprintln!("relay admin socket {}", admin_socket.display());
    eprintln!(
        "relay listening on {}; opaque Noise relay; end-to-end keys never belong on this host",
        listener.local_addr()?
    );
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
    workers.shutdown().await;
    Ok(())
}
fn reject(code: StatusCode, text: &str) -> ErrorResponse {
    let mut r = ErrorResponse::new(Some(text.to_owned()));
    *r.status_mut() = code;
    r.headers_mut()
        .insert("Content-Length", text.len().to_string().parse().unwrap());
    r
}
struct Reservation {
    registry: Registry,
    id: String,
    role: usize,
    connection: u64,
    committed: bool,
    had_identity: bool,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut map = self.registry.lock().unwrap();
        if let Some(s) = map.get_mut(&self.id) {
            let peer = &mut s.peers[self.role];
            if peer.connection == self.connection {
                peer.tx = None;
                peer.connected = false;
                if !self.committed && !self.had_identity {
                    peer.instance = None;
                }
                if self.committed
                    && let Some(other) = &s.peers[1 - self.role].tx
                {
                    let event = serde_json::to_string(&Frame::RelayEvent {
                        code: "PEER_DISCONNECTED".into(),
                    })
                    .expect("relay event serialization");
                    let _ = other.try_send(Message::Text(event.into()));
                }
            }
        }
    }
}
// Tungstenite requires its concrete HTTP ErrorResponse in the upgrade callback.
#[allow(clippy::result_large_err)]
async fn peer(mut stream: TcpStream, registry: Registry) -> Result<()> {
    // A normal health probe is not a WebSocket handshake. Peek without consuming
    // the upgrade bytes; fragmented first lines have a bounded read deadline.
    let mut line = [0u8; 1024];
    let len = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let n = stream.peek(&mut line).await?;
            if n == 0 || n == line.len() || line[..n].contains(&b'\n') {
                return std::io::Result::Ok(n);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await??;
    if line[..len].starts_with(b"GET /healthz HTTP/1.1\r\n")
        || line[..len].starts_with(b"GET /healthz HTTP/1.0\r\n")
    {
        tokio::time::timeout(Duration::from_secs(2), stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nok\n")).await??;
        return Ok(());
    }
    let (tx, mut rx) = mpsc::channel::<Message>(8);
    let mut reservation = None;
    let callback = |req: &Request, response: Response| {
        if req.uri().path() == "/healthz" {
            return Err(reject(StatusCode::OK, "ok\n"));
        }
        if req.headers().contains_key("origin") || req.uri().query().is_some() {
            return Err(reject(StatusCode::FORBIDDEN, "native clients only"));
        }
        let parts: Vec<_> = req.uri().path().split('/').collect();
        if parts.len() != 4 || parts[1] != "v1" {
            return Err(reject(StatusCode::NOT_FOUND, "not found"));
        }
        let role = match parts[2] {
            "connect" => 0,
            "control" => 1,
            _ => return Err(reject(StatusCode::NOT_FOUND, "not found")),
        };
        let token = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        let instance = req
            .headers()
            .get("x-agent-tunnel-instance")
            .and_then(|v| v.to_str().ok())
            .filter(|v| valid_id(v));
        let (Some(token), Some(instance)) = (token, instance) else {
            return Err(reject(StatusCode::UNAUTHORIZED, "unauthorized"));
        };
        let mut map = registry.lock().unwrap();
        let Some(s) = map.get_mut(parts[3]) else {
            return Err(reject(StatusCode::UNAUTHORIZED, "unauthorized"));
        };
        let expected = if role == 0 {
            &s.config.connector_hash
        } else {
            &s.config.controller_hash
        };
        if s.revoked
            || s.config.expires_at <= config::now()
            || !config::token_matches(token, expected)
        {
            return Err(reject(StatusCode::UNAUTHORIZED, "unauthorized"));
        }
        let p = &mut s.peers[role];
        if p.tx.is_some() || p.instance.as_deref().is_some_and(|old| old != instance) {
            return Err(reject(
                StatusCode::CONFLICT,
                "role already bound; restart requires a new session",
            ));
        }
        let had_identity = p.instance.is_some();
        p.instance = Some(instance.into());
        p.connection += 1;
        p.tx = Some(tx.clone());
        reservation = Some(Reservation {
            registry: registry.clone(),
            id: parts[3].into(),
            role,
            connection: p.connection,
            committed: false,
            had_identity,
        });
        Ok(response)
    };
    let mut ws = tokio::time::timeout(
        Duration::from_secs(5),
        accept_hdr_async_with_config(stream, callback, Some(ws_config())),
    )
    .await??;
    let mut reservation = reservation.expect("authenticated upgrade");
    reservation.committed = true;
    {
        let mut map = registry.lock().unwrap();
        let s = map.get_mut(&reservation.id).unwrap();
        s.peers[reservation.role].connected = true;
        if s.peers.iter().all(|p| p.connected) {
            for p in &s.peers {
                let event = serde_json::to_string(&Frame::RelayEvent {
                    code: "PEER_CONNECTED".into(),
                })
                .expect("relay event serialization");
                let _ = p.tx.as_ref().unwrap().try_send(Message::Text(event.into()));
            }
        }
    }
    let expiry = registry.lock().unwrap()[&reservation.id].config.expires_at;
    let deadline = Instant::now() + Duration::from_secs(expiry.saturating_sub(config::now()));
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    let mut last = Instant::now();
    loop {
        if registry.lock().unwrap()[&reservation.id].revoked {
            break;
        }
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
                        let packet: Frame = serde_json::from_str(&text)?;
                        let valid = matches!((&packet, reservation.role),
                            (Frame::Response { .. }, 0) | (Frame::Init { .. }, 1) | (Frame::Data { .. }, _));
                        ensure!(valid, "role/protocol violation");
                        let dest = { let map = registry.lock().unwrap(); let peer = &map[&reservation.id].peers[1 - reservation.role]; if peer.connected { peer.tx.clone() } else { None } };
                        if let Some(dest) = dest {
                            // A saturated destination is disconnected rather than consuming unlimited memory.
                            ensure!(dest.try_send(Message::Text(text)).is_ok(), "destination overloaded/offline");
                        } else {
                            let error = Frame::RelayEvent { code: "PEER_OFFLINE".into() };
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

fn revoked_path(path: &std::path::Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".revoked");
    name.into()
}
fn administer(registry: &Registry, action: crate::admin::Action) -> serde_json::Value {
    use serde_json::json;
    let mut map = registry.lock().unwrap();
    match action {
        crate::admin::Action::Status => json!({"sessions": map.values().map(|s| json!({
            "session_id": s.config.session_id, "expires_at": s.config.expires_at, "revoked": s.revoked,
            "connector_connected": s.peers[0].connected, "controller_connected": s.peers[1].connected
        })).collect::<Vec<_>>()}),
        crate::admin::Action::Revoke { session_id } => {
            let Some(s) = map.get_mut(&session_id) else {
                return json!({"revoked":false,"error":"SESSION_NOT_FOUND"});
            };
            // Mark in memory even if the durable write fails: never keep a live authorization because disk is full.
            s.revoked = true;
            let path = revoked_path(&s.source);
            let persisted = (|| -> Result<()> {
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                if path.try_exists()? {
                    return Ok(());
                }
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)?;
                file.write_all(session_id.as_bytes())?;
                file.sync_all()?;
                std::fs::File::open(path.parent().unwrap_or(std::path::Path::new(".")))?
                    .sync_all()?;
                Ok(())
            })();
            for p in &s.peers {
                if let Some(tx) = &p.tx {
                    let _ = tx.try_send(Message::Close(None));
                }
            }
            json!({"session_id":session_id,"revoked":true,"persisted":persisted.is_ok(),
                "warning": if persisted.is_ok() { "Remote processes stop no later than their remaining controller lease (at most 60s), not instantly." } else { "Durable marker write failed. DO NOT restart this relay with the old session file. Remove/rotate credentials." }})
        }
    }
}
