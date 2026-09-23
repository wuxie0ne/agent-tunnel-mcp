use crate::{
    config::{EndpointConfig, relay_url},
    protocol::MAX_FRAME,
};
use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
#[cfg(any(feature = "controller", feature = "relay"))]
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest, protocol::WebSocketConfig},
};

pub type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
pub fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME))
        .max_write_buffer_size(MAX_FRAME * 2)
        .write_buffer_size(0)
}
pub async fn connect(config: &EndpointConfig, instance: &str) -> Result<Ws> {
    let root = relay_url(&config.relay)?;
    let route = if config.role == "connector" {
        "connect"
    } else {
        "control"
    };
    let url = format!("{root}/v1/{route}/{}", config.session_id);
    let mut req = url.as_str().into_client_request()?;
    let mut auth = format!("Bearer {}", config.token)
        .parse::<tokio_tungstenite::tungstenite::http::HeaderValue>()?;
    auth.set_sensitive(true);
    req.headers_mut().insert("Authorization", auth);
    req.headers_mut()
        .insert("X-Agent-Tunnel-Instance", instance.parse()?);
    // A deliberately explicit IP override helps a target whose authorized
    // hostname cannot resolve locally. The WebSocket URL, HTTP Host header,
    // TLS SNI and certificate verification still use the configured hostname.
    // In particular this option MUST NOT permit ws:// on the public network.
    let override_ip = match std::env::var("AGENT_TUNNEL_CONNECT_IP") {
        Ok(text) => {
            ensure!(
                root.starts_with("wss://"),
                "connection IP override requires wss://"
            );
            Some(
                text.parse::<std::net::IpAddr>()
                    .context("invalid AGENT_TUNNEL_CONNECT_IP")?,
            )
        }
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => return Err(error.into()),
    };
    let (ws, _) = if let Some(ip) = override_ip {
        let port = req.uri().port_u16().unwrap_or(443);
        let socket = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::net::TcpStream::connect(std::net::SocketAddr::new(ip, port)),
        )
        .await??;
        socket.set_nodelay(true)?;
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::client_async_tls_with_config(req, socket, Some(ws_config()), None),
        )
        .await??
    } else {
        // No redirect following; use native trust roots and normal certificate validation.
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async_with_config(req, Some(ws_config()), true),
        )
        .await??
    };
    Ok(ws)
}
pub async fn send<S>(ws: &mut WebSocketStream<S>, packet: &impl serde::Serialize) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let text = serde_json::to_string(packet)?;
    ensure!(text.len() <= MAX_FRAME, "outgoing frame exceeds limit");
    tokio::time::timeout(Duration::from_secs(5), ws.send(Message::Text(text.into()))).await??;
    Ok(())
}
#[cfg(any(feature = "controller", feature = "relay"))]
pub async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let count = reader
        .take((MAX_FRAME + 1) as u64)
        .read_until(b'\n', &mut line)
        .await?;
    if count == 0 {
        return Ok(None);
    }
    ensure!(
        count <= MAX_FRAME && line.last() == Some(&b'\n'),
        "oversized or incomplete JSON line"
    );
    Ok(Some(line))
}
#[cfg(any(feature = "controller", feature = "relay"))]
pub async fn write_line<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &impl serde::Serialize,
) -> Result<()> {
    let mut data = serde_json::to_vec(value)?;
    ensure!(data.len() < MAX_FRAME, "oversized JSON line");
    data.push(b'\n');
    writer.write_all(&data).await?;
    writer.flush().await?;
    Ok(())
}
// Kept explicit rather than silently accepting binary frames or another protocol.
pub async fn receive(ws: &mut Ws) -> Result<Option<String>> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return Ok(Some(t.to_string())),
            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {
                ws.flush().await?;
            }
            None | Some(Ok(Message::Close(_))) => return Ok(None),
            Some(Err(e)) => return Err(e.into()),
            _ => bail!("unexpected WebSocket message"),
        }
    }
}

/// Each Noise fragment fits below both Noise's 65535-byte and WS frame limits.
pub async fn send_encrypted(
    ws: &mut Ws,
    channel: &mut crate::crypto::Channel,
    packet: &impl serde::Serialize,
) -> Result<()> {
    for frame in channel.seal(packet)? {
        send(ws, &frame).await?;
    }
    Ok(())
}

/// Keep transport errors diagnostic without disclosing either authentication
/// secret or embedding raw terminal control characters in operator logs.
pub fn safe_connect_error(error: &anyhow::Error, config: &EndpointConfig) -> String {
    error
        .to_string()
        .replace(&config.token, "[role token redacted]")
        .replace(&config.channel_key, "[channel key redacted]")
        .chars()
        .flat_map(char::escape_default)
        .take(320)
        .collect()
}
