use anyhow::{Context, Result, bail, ensure};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
#[cfg(any(feature = "controller", feature = "relay", test))]
use sha2::{Digest, Sha256};
use std::{fs, path::Path, time::{SystemTime, UNIX_EPOCH}};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
#[cfg(any(feature = "relay", test))]
use subtle::ConstantTimeEq;
use tokio_tungstenite::tungstenite::http::Uri;
use crate::protocol::{VERSION, valid_id};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointConfig {
    pub protocol: u32,
    pub role: String,
    pub relay: String,
    pub session_id: String,
    pub target_id: String,
    pub name: String,
    pub token: String,
    pub expires_at: u64,
}
#[cfg(any(feature = "controller", feature = "relay"))]
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayConfig {
    pub protocol: u32,
    pub session_id: String,
    pub connector_hash: String,
    pub controller_hash: String,
    pub expires_at: u64,
}

pub fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).expect("system time before epoch").as_secs() }
pub fn random_id() -> String {
    let mut bytes = [0u8; 32]; OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
#[cfg(any(feature = "controller", feature = "relay", test))]
pub fn hash(token: &str) -> String { format!("{:x}", Sha256::digest(token.as_bytes())) }
#[cfg(any(feature = "relay", test))]
pub fn token_matches(token: &str, expected: &str) -> bool {
    hash(token).as_bytes().ct_eq(expected.as_bytes()).into()
}

pub fn private_directory(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).context("private directory does not exist")?;
    ensure!(meta.is_dir() && !meta.file_type().is_symlink(), "expected a real directory");
    // SAFETY: geteuid has no arguments and cannot violate memory safety.
    ensure!(meta.uid() == unsafe { libc::geteuid() }, "directory must belong to this user");
    ensure!(meta.mode() & 0o077 == 0, "directory must have mode 0700 (no group/other access)");
    Ok(())
}
pub fn load<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T> {
    let file = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)
        .context("open credential file (symlinks are not allowed)")?;
    let meta = file.metadata()?;
    ensure!(meta.is_file() && meta.len() <= 16 * 1024, "invalid credential file");
    ensure!(meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0, "credential file must be owned by this user with mode 0600");
    serde_json::from_reader(file).context("invalid credential file JSON")
}
#[cfg(feature = "controller")]
fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?; file.sync_all()?;
    Ok(())
}
#[cfg(feature = "controller")]
pub fn init(dir: &Path, relay: &str, name: &str, ttl: u64) -> Result<()> {
    ensure!((10..=3600).contains(&ttl), "TTL must be 10..3600 seconds");
    ensure!(!name.is_empty() && name.len() <= 100 && !name.chars().any(char::is_control), "invalid target name");
    relay_url(relay)?;
    // create_dir is deliberately non-recursive and refuses existing directories.
    fs::DirBuilder::new().mode(0o700).create(dir).context("create new session directory (must not already exist)")?;
    let session_id = random_id(); let target_id = random_id();
    let controller = random_id(); let connector = random_id(); let expires_at = now() + ttl;
    save(&dir.join("relay.json"), &RelayConfig { protocol: VERSION, session_id: session_id.clone(), connector_hash: hash(&connector), controller_hash: hash(&controller), expires_at })?;
    for (role, token) in [("controller", controller), ("connector", connector)] {
        save(&dir.join(format!("{role}.json")), &EndpointConfig { protocol: VERSION, role: role.into(), relay: relay.into(), session_id: session_id.clone(), target_id: target_id.clone(), name: name.into(), token, expires_at })?;
    }
    println!("{}", serde_json::json!({"session_id": session_id, "target_id": target_id, "expires_at": expires_at, "directory": dir}));
    Ok(())
}
#[cfg(feature = "controller")]
use std::os::unix::fs::DirBuilderExt;

pub fn validate_endpoint(config: &EndpointConfig, role: &str) -> Result<()> {
    ensure!(config.protocol == VERSION && config.role == role, "credential protocol or role mismatch");
    ensure!(valid_id(&config.session_id) && valid_id(&config.target_id), "invalid session/target ID");
    ensure!(config.token.len() == 64 && config.token.bytes().all(|b| b.is_ascii_hexdigit()), "invalid token");
    ensure!(config.expires_at > now() && config.expires_at - now() <= 3600, "session expired or exceeds one hour");
    relay_url(&config.relay)?; Ok(())
}
pub fn relay_url(input: &str) -> Result<String> {
    ensure!(input.is_ascii() && !input.contains(['#', '@', '\\']) && !input.chars().any(char::is_whitespace), "URL must be ASCII, without credentials, fragment or whitespace");
    let uri: Uri = input.parse().context("invalid relay URL")?;
    let scheme = match uri.scheme_str() { Some("https" | "wss") => "wss", Some("http" | "ws") => "ws", _ => bail!("relay URL requires https/wss (loopback http/ws allowed)") };
    ensure!(uri.path_and_query().is_none_or(|p| p.as_str() == "/" || p.as_str().is_empty()), "relay URL must not include a path or query");
    let authority = uri.authority().context("relay URL requires a host")?;
    let host = authority.host().trim_matches(['[', ']']);
    let loopback = host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    ensure!(scheme == "wss" || loopback, "unencrypted ws is only permitted on loopback");
    Ok(format!("{scheme}://{authority}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn url_policy() {
        assert!(relay_url("http://127.0.0.1:8787").is_ok());
        assert!(relay_url("http://[::1]:8787").is_ok());
        for u in ["http://example.org", "wss://a/b", "https://a/?token=x", "https://u:p@a", "file:///tmp/a"] { assert!(relay_url(u).is_err(), "{u}"); }
        assert_eq!(relay_url("https://example.org").unwrap(), "wss://example.org");
    }
    #[test] fn separated_tokens() { let t = random_id(); assert!(token_matches(&t, &hash(&t))); assert!(!token_matches(&random_id(), &hash(&t))); }
}
