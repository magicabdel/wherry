//! Native SSH SOCKS bridge.
//!
//! Replaces the previous `ssh -D <port>` subprocess with an in-process bridge
//! built on [`russh`]: it authenticates to the bastion with our ed25519 key,
//! runs a minimal SOCKS5 server on the local port, and routes each connection
//! through an SSH `direct-tcpip` channel.
//!
//! The bridge must outlive the `wherry bridge start` invocation (it is
//! started from a kube config `exec` credential plugin), so it daemonizes via
//! the [`daemonize`] crate before serving.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use daemonize::Daemonize;
use russh::client::{self, AuthResult, Handle, Handler};
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Parameters needed to establish the bridge.
pub struct BridgeParams {
    pub key_path: PathBuf,
    pub user: String,
    pub host: String,
}

/// Daemonize, then run the SOCKS bridge forever in the background.
///
/// `listener` is bound by the caller *before* forking so the local port is held
/// open the moment this (foreground) process exits — callers relying on the
/// proxy won't race the daemon coming up. This function only returns (with an
/// error) if the daemon itself fails to start; on success the foreground
/// process has already exited inside [`Daemonize::start`].
pub fn spawn(listener: std::net::TcpListener, params: BridgeParams) -> Result<()> {
    // Capture the port before forking so the daemon can advertise itself via a
    // PID file (used by `wherry bridge stop` and the reuse check).
    let pid_path = listener
        .local_addr()
        .ok()
        .map(|addr| pid_file_path(addr.port()));

    let daemon = Daemonize::new().working_directory("/");

    // Redirect the daemon's stderr to a log file so failures are diagnosable.
    let daemon = match log_file() {
        Some(file) => daemon.stderr(file),
        None => daemon,
    };

    daemon
        .start()
        .map_err(|e| anyhow!("failed to daemonize bridge: {e}"))?;

    // From here on we are the detached daemon process. Record our PID so the
    // bridge can be found and stopped later; best-effort, it serves regardless.
    if let Some(path) = &pid_path {
        if let Err(e) = std::fs::write(path, std::process::id().to_string()) {
            eprintln!("warning: failed to write PID file {}: {e}", path.display());
        }
    }

    // A fresh runtime is built *after* the fork (forking a live multi-threaded
    // runtime is unsound).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build bridge runtime")?;

    let result = runtime.block_on(serve(listener, params));

    // Only reached if serving fails: drop our PID file so it isn't left stale.
    if let Some(path) = &pid_path {
        let _ = std::fs::remove_file(path);
    }

    result
}

/// Path of the PID file advertising a bridge daemon on `port`.
pub fn pid_file_path(port: u16) -> PathBuf {
    std::env::temp_dir().join(format!("wherry-bridge-{port}.pid"))
}

/// Outcome of asking a bridge daemon to stop.
#[derive(Debug, PartialEq, Eq)]
pub enum StopOutcome {
    /// The daemon was signalled to terminate.
    Stopped { pid: u32 },
    /// No PID file was found for the port.
    NotRunning,
    /// A PID file existed but no matching live bridge did; it was cleaned up.
    Stale,
}

/// Whether a live bridge daemon is serving `port`: a PID file is present, the
/// process is alive, and the port is actually held.
pub fn is_running(port: u16) -> bool {
    read_pid(port).is_some_and(process_alive) && port_in_use(port)
}

/// Ports of every bridge that has left a PID file in the temp dir.
pub fn running_ports() -> Vec<u16> {
    let mut ports = Vec::new();
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(port) = name
                .strip_prefix("wherry-bridge-")
                .and_then(|rest| rest.strip_suffix(".pid"))
                .and_then(|port| port.parse::<u16>().ok())
            {
                ports.push(port);
            }
        }
    }
    ports.sort_unstable();
    ports
}

/// Stop the bridge daemon serving `port`, if any.
///
/// Only a process that is both alive *and* still holding the port is
/// terminated; anything else is treated as a stale PID file (a crash, or a
/// reused PID) and simply cleaned up, so we never signal an unrelated process.
pub fn stop(port: u16) -> Result<StopOutcome> {
    let pid = match read_pid(port) {
        Some(pid) => pid,
        None => return Ok(StopOutcome::NotRunning),
    };

    if !process_alive(pid) || !port_in_use(port) {
        let _ = std::fs::remove_file(pid_file_path(port));
        return Ok(StopOutcome::Stale);
    }

    terminate(pid).with_context(|| format!("failed to stop bridge (pid {pid})"))?;
    let _ = std::fs::remove_file(pid_file_path(port));
    Ok(StopOutcome::Stopped { pid })
}

/// Read the PID recorded for `port`, if the file exists and is valid.
fn read_pid(port: u16) -> Option<u32> {
    std::fs::read_to_string(pid_file_path(port))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Whether `pid` names a live process we may signal (`kill -0`).
fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Whether the local `port` is currently bound (i.e. a bridge is serving it).
fn port_in_use(port: u16) -> bool {
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(_) => false,
        Err(e) => e.kind() == std::io::ErrorKind::AddrInUse,
    }
}

/// Send SIGTERM to `pid` via the `kill` utility.
fn terminate(pid: u32) -> Result<()> {
    let status = Command::new("kill")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to invoke kill")?;
    if !status.success() {
        bail!("kill {pid} failed with {status}");
    }
    Ok(())
}

/// Open a log file for the daemon in the system temp dir, best-effort.
fn log_file() -> Option<std::fs::File> {
    let path = std::env::temp_dir().join("wherry-bridge.log");
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

/// How often the serve loop verifies that the SSH session is still alive.
const SESSION_WATCHDOG_INTERVAL: Duration = Duration::from_secs(2);

/// Connect to the bastion and serve SOCKS5 connections until the process is
/// killed or the SSH session to the bastion is lost.
///
/// Losing the session is fatal on purpose: keys pushed via EC2 Instance
/// Connect expire after 60 seconds, so the daemon cannot silently reconnect
/// on its own. Exiting releases the port and PID file, and the next
/// `wherry bridge start` (run automatically by the kube config exec plugin)
/// re-pushes the key and brings up a fresh bridge.
async fn serve(listener: std::net::TcpListener, params: BridgeParams) -> Result<()> {
    listener
        .set_nonblocking(true)
        .context("failed to set listener non-blocking")?;
    let listener = TcpListener::from_std(listener).context("failed to adopt listening socket")?;

    let key = load_secret_key(&params.key_path, None)
        .with_context(|| format!("failed to load key {}", params.key_path.display()))?;

    // Aggressive keepalives so a dead connection (bastion idle timeout, NAT
    // expiry, suspend/resume) is noticed within about a minute instead of
    // leaving a zombie bridge behind. russh gives up after `keepalive_max`
    // (default 3) unanswered probes and tears the session down.
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(15)),
        ..Default::default()
    });

    let mut handle = client::connect(config, (params.host.as_str(), 22), Client)
        .await
        .with_context(|| format!("failed to connect to bastion {}", params.host))?;

    let auth = handle
        .authenticate_publickey(
            params.user.clone(),
            PrivateKeyWithHashAlg::new(Arc::new(key), None),
        )
        .await
        .context("SSH authentication failed")?;

    if !matches!(auth, AuthResult::Success) {
        bail!("SSH public key authentication was rejected by the bastion");
    }

    let handle = Arc::new(handle);

    // Signalled by connection handlers that fail because the session is gone,
    // so the daemon exits immediately instead of waiting for the watchdog.
    let session_lost = Arc::new(tokio::sync::Notify::new());
    let mut watchdog = tokio::time::interval(SESSION_WATCHDOG_INTERVAL);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (inbound, peer) = accepted.context("failed to accept SOCKS connection")?;
                let session = handle.clone();
                let session_lost = session_lost.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_socks(inbound, peer, session.clone()).await {
                        eprintln!("socks connection error: {e:#}");
                        if session.is_closed() {
                            session_lost.notify_one();
                        }
                    }
                });
            }
            _ = watchdog.tick() => {
                if handle.is_closed() {
                    bail!(session_lost_error(&params.host));
                }
            }
            _ = session_lost.notified() => {
                bail!(session_lost_error(&params.host));
            }
        }
    }
}

/// Error message for a lost SSH session; the daemon exits with it so the
/// exec plugin can transparently start a replacement bridge.
fn session_lost_error(host: &str) -> String {
    format!(
        "SSH session to bastion {host} was lost; exiting so the next \
         `wherry bridge start` (or kubectl call) starts a fresh bridge"
    )
}

/// Handle a single SOCKS5 client: negotiate, then splice it to a `direct-tcpip`
/// channel on the SSH session.
async fn handle_socks(
    mut inbound: TcpStream,
    peer: SocketAddr,
    session: Arc<Handle<Client>>,
) -> Result<()> {
    let (host, port) = socks_handshake(&mut inbound).await?;

    let channel = session
        .channel_open_direct_tcpip(
            host.clone(),
            u32::from(port),
            peer.ip().to_string(),
            u32::from(peer.port()),
        )
        .await
        .with_context(|| format!("failed to open channel to {host}:{port}"))?;

    // SOCKS5 success reply with a dummy bound address (0.0.0.0:0).
    inbound
        .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await?;

    let mut stream = channel.into_stream();
    tokio::io::copy_bidirectional(&mut inbound, &mut stream)
        .await
        .ok();

    Ok(())
}

/// Perform the SOCKS5 no-auth + CONNECT negotiation (RFC 1928) and return the
/// requested target host and port.
async fn socks_handshake<S: AsyncRead + AsyncWrite + Unpin>(
    inbound: &mut S,
) -> Result<(String, u16)> {
    // Greeting: VER, NMETHODS, METHODS...
    let mut greeting = [0u8; 2];
    inbound.read_exact(&mut greeting).await?;
    if greeting[0] != 0x05 {
        bail!("unsupported SOCKS version {}", greeting[0]);
    }
    let mut methods = vec![0u8; greeting[1] as usize];
    inbound.read_exact(&mut methods).await?;

    // Select the "no authentication" method.
    inbound.write_all(&[0x05, 0x00]).await?;

    // Request: VER, CMD, RSV, ATYP, ADDR, PORT
    let mut request = [0u8; 4];
    inbound.read_exact(&mut request).await?;
    if request[0] != 0x05 {
        bail!("bad SOCKS request version {}", request[0]);
    }
    if request[1] != 0x01 {
        // Only CONNECT is supported; reply "command not supported".
        inbound
            .write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await?;
        bail!("unsupported SOCKS command {}", request[1]);
    }

    let host = match request[3] {
        0x01 => {
            let mut addr = [0u8; 4];
            inbound.read_exact(&mut addr).await?;
            std::net::Ipv4Addr::from(addr).to_string()
        }
        0x04 => {
            let mut addr = [0u8; 16];
            inbound.read_exact(&mut addr).await?;
            std::net::Ipv6Addr::from(addr).to_string()
        }
        0x03 => {
            let mut len = [0u8; 1];
            inbound.read_exact(&mut len).await?;
            let mut domain = vec![0u8; len[0] as usize];
            inbound.read_exact(&mut domain).await?;
            String::from_utf8(domain).context("invalid SOCKS domain name")?
        }
        other => bail!("unsupported SOCKS address type {other}"),
    };

    let mut port = [0u8; 2];
    inbound.read_exact(&mut port).await?;

    Ok((host, u16::from_be_bytes(port)))
}

/// SSH client handler that accepts any host key.
///
/// This mirrors the previous behaviour (`StrictHostKeyChecking=no`): the bastion
/// is reached over the AWS network and its key is not pre-provisioned.
struct Client;

impl Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socks_handshake_parses_domain_connect() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (mut client, mut server) = tokio::io::duplex(256);

            let client_fut = async {
                // Greeting: VER=5, one method, "no auth".
                client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
                let mut selection = [0u8; 2];
                client.read_exact(&mut selection).await.unwrap();
                assert_eq!(selection, [0x05, 0x00]);

                // CONNECT to example.com:443 via a domain-name address.
                let mut request = vec![0x05, 0x01, 0x00, 0x03, 11];
                request.extend_from_slice(b"example.com");
                request.extend_from_slice(&443u16.to_be_bytes());
                client.write_all(&request).await.unwrap();
            };

            let (_, parsed) = tokio::join!(client_fut, socks_handshake(&mut server));
            let (host, port) = parsed.unwrap();
            assert_eq!(host, "example.com");
            assert_eq!(port, 443);
        });
    }

    #[test]
    fn socks_handshake_rejects_wrong_version() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (mut client, mut server) = tokio::io::duplex(16);
            let client_fut = async {
                client.write_all(&[0x04, 0x01]).await.ok();
            };
            let (_, parsed) = tokio::join!(client_fut, socks_handshake(&mut server));
            assert!(parsed.is_err());
        });
    }
}
