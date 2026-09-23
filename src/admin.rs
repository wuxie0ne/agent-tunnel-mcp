//! Relay administration is local-only and never available through the public WS listener.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::BufReader,
    net::{UnixListener, UnixStream},
};

#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Status,
    Revoke { session_id: String },
}
pub struct Socket {
    path: PathBuf,
    inode: u64,
}
impl Drop for Socket {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|m| m.ino() == self.inode) {
            let _ = fs::remove_file(&self.path);
        }
    }
}
pub fn bind(path: &Path) -> Result<(UnixListener, Socket)> {
    crate::config::private_directory(path.parent().unwrap_or(Path::new(".")))?;
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok((
        listener,
        Socket {
            path: path.into(),
            inode: fs::symlink_metadata(path)?.ino(),
        },
    ))
}
pub async fn serve(listener: UnixListener, handler: Arc<dyn Fn(Action) -> Value + Send + Sync>) {
    let mut clients = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            incoming = listener.accept() => {
                let Ok((stream, _)) = incoming else { break; };
                if clients.len() >= 16 { continue; }
                let handler = handler.clone();
                clients.spawn(async move {
                    let _ = tokio::time::timeout(Duration::from_secs(5), async move {
                        ensure!(stream.peer_cred()?.uid() == unsafe { libc::geteuid() }, "admin peer UID mismatch");
                        let (read, mut write) = stream.into_split(); let mut reader = BufReader::new(read);
                        let Some(line) = crate::transport::read_line(&mut reader).await? else { return Ok(()); };
                        let action: Action = serde_json::from_slice(&line)?;
                        crate::transport::write_line(&mut write, &handler(action)).await?;
                        Ok::<_, anyhow::Error>(())
                    }).await;
                });
            }
            _ = clients.join_next(), if !clients.is_empty() => {}
        }
    }
}
pub async fn call(path: &Path, action: Action) -> Result<Value> {
    crate::config::private_directory(path.parent().unwrap_or(Path::new(".")))?;
    let stream = UnixStream::connect(path).await?;
    ensure!(
        stream.peer_cred()?.uid() == unsafe { libc::geteuid() },
        "admin server UID mismatch"
    );
    let (read, mut write) = stream.into_split();
    crate::transport::write_line(&mut write, &action).await?;
    let mut reader = BufReader::new(read);
    let line = tokio::time::timeout(
        Duration::from_secs(6),
        crate::transport::read_line(&mut reader),
    )
    .await??
    .ok_or_else(|| anyhow::anyhow!("admin connection closed"))?;
    Ok(serde_json::from_slice(&line)?)
}
