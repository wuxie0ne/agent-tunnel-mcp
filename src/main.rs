#![cfg_attr(not(unix), allow(unused))]
#[cfg(not(unix))]
compile_error!("agent-tunnel currently supports Unix targets only");

mod config;
mod connector;
mod executor;
mod protocol;
mod state;
mod transport;
#[cfg(feature = "relay")] mod relay;
#[cfg(feature = "controller")] mod controller;
#[cfg(feature = "mcp")] mod mcp;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "Temporary remote execution for terminal agents (test environments only)")]
struct Cli { #[command(subcommand)] command: Commands }
#[derive(Subcommand)]
enum Commands {
    /// Generate separated, expiring role credentials. Never prints tokens.
    #[cfg(feature = "controller")]
    Init { #[arg(long)] dir: PathBuf, #[arg(long)] relay: String, #[arg(long, default_value = "test-target")] name: String, #[arg(long, default_value_t = 3600)] ttl_secs: u64 },
    /// Run a loopback-only relay. Use a TLS tunnel to expose it.
    #[cfg(feature = "relay")]
    Relay { #[arg(long, default_value = "127.0.0.1:8787")] listen: std::net::SocketAddr, #[arg(long, required = true)] session_file: Vec<PathBuf> },
    /// Connect this target; explicit whole-session permission is required.
    Connect { #[arg(long)] config: PathBuf, #[arg(long)] state_dir: Option<PathBuf>, #[arg(long)] allow_exec: bool, #[arg(long, default_value_t = 60)] lease_secs: u64 },
    /// Keep a local controller alive across CLI calls and network reconnects.
    #[cfg(feature = "controller")]
    Local { #[arg(long)] config: PathBuf, #[arg(long)] socket: PathBuf, #[arg(long)] accept_session_risk: bool },
    /// Inspect the remote identity before executing commands.
    #[cfg(feature = "controller")]
    Info { #[arg(long)] socket: PathBuf },
    /// Start a remote argv command (use -- /bin/sh -c '...' for shell syntax).
    #[cfg(feature = "controller")]
    Exec { #[arg(long)] socket: PathBuf, #[arg(long)] incarnation: String, #[arg(long)] cwd: String, #[arg(long)] request_id: String,
        #[arg(long, default_value_t = 60000)] timeout_ms: u64, #[arg(long = "env", value_parser = parse_env)] env: Vec<(String, String)>,
        #[arg(required = true, last = true)] argv: Vec<String> },
    /// Read a remote job's bounded output and PID/status.
    #[cfg(feature = "controller")]
    Read { #[arg(long)] socket: PathBuf, #[arg(long)] job: String, #[arg(long, default_value_t = 0)] cursor: u64 },
    /// Request cancellation; use read to confirm that the job stopped.
    #[cfg(feature = "controller")]
    Cancel { #[arg(long)] socket: PathBuf, #[arg(long)] job: String },
    /// Serve MCP over stdio; stdout is reserved for protocol messages.
    #[cfg(feature = "mcp")]
    Mcp { #[arg(long)] socket: PathBuf },
    /// Inspect durable PID records without adopting or killing any process.
    Inspect { #[arg(long)] state_dir: PathBuf },
}
#[cfg(feature = "controller")]
fn parse_env(value: &str) -> std::result::Result<(String, String), String> {
    value.split_once('=').map(|(k,v)| (k.into(),v.into())).ok_or_else(|| "expected KEY=VALUE".into())
}

// SIGTERM is as important as Ctrl+C for container and service shutdown.
fn shutdown_signal() -> std::pin::Pin<Box<impl std::future::Future<Output = ()>>> {
    Box::pin(async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
    })
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    match run(Cli::parse()).await { Ok(()) => std::process::ExitCode::SUCCESS, Err(e) => { eprintln!("error: {e:#}"); std::process::ExitCode::FAILURE } }
}
async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        #[cfg(feature = "controller")]
        Commands::Init { dir, relay, name, ttl_secs } => config::init(&dir, &relay, &name, ttl_secs),
        #[cfg(feature = "relay")]
        Commands::Relay { listen, session_file } => relay::run(listen, session_file).await,
        Commands::Connect { config, state_dir, allow_exec, lease_secs } => connector::run(config, state_dir, allow_exec, lease_secs).await,
        #[cfg(feature = "controller")]
        Commands::Local { config, socket, accept_session_risk } => controller::run(config, socket, accept_session_risk).await,
        #[cfg(feature = "controller")]
        Commands::Info { socket } => print_reply(socket, None, protocol::Operation::Info).await,
        #[cfg(feature = "controller")]
        Commands::Exec { socket, incarnation, cwd, request_id, timeout_ms, env, argv } => print_reply(socket, Some(request_id), protocol::Operation::Exec(protocol::Exec { expected_incarnation: incarnation, argv, cwd, env: env.into_iter().collect(), timeout_ms })).await,
        #[cfg(feature = "controller")]
        Commands::Read { socket, job, cursor } => print_reply(socket, None, protocol::Operation::Read { job_id: job, cursor }).await,
        #[cfg(feature = "controller")]
        Commands::Cancel { socket, job } => print_reply(socket, None, protocol::Operation::Cancel { job_id: job }).await,
        #[cfg(feature = "mcp")]
        Commands::Mcp { socket } => mcp::run(socket).await,
        Commands::Inspect { state_dir } => { println!("{}", serde_json::to_string_pretty(&state::inspect(&state_dir)?)?); Ok(()) },
    }
}
#[cfg(feature = "controller")]
async fn print_reply(socket: PathBuf, id: Option<String>, op: protocol::Operation) -> Result<()> {
    let reply = controller::call(&socket, protocol::Request { id: id.unwrap_or_else(config::random_id), op }).await?;
    println!("{}", serde_json::to_string(&reply)?);
    if reply.error.is_some() { anyhow::bail!("remote request failed (see JSON error)"); } Ok(())
}
