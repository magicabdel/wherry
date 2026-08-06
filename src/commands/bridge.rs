use std::io::ErrorKind;

use anyhow::{bail, Context, Result};

use crate::aws::bastion::Bastion;
use crate::aws::{bastion as aws_bastion, session};
use crate::bridge::{self, BridgeParams, StopOutcome};
use crate::commands::{resolve_pinned_bastion, select_bastion, DEFAULT_BRIDGE_PORT};
use crate::prompt;
use crate::ssh;

/// Start (or reuse) a native SOCKS bridge to a bastion host.
///
/// This is intentionally synchronous: it owns its Tokio runtimes explicitly so
/// that the AWS SDK work is fully torn down before the bridge daemonizes
/// (forking a live multi-threaded runtime is unsound).
pub fn start(
    profile: Option<String>,
    region: String,
    port: u16,
    bastion_name: Option<String>,
) -> Result<()> {
    let profile = prompt::resolve_profile(profile)?;

    // Bind the local port up front. Success reserves it for the daemon (which
    // inherits the socket across the fork); an "address in use" error means a
    // bridge is already running on this port.
    let listener = match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => listener,
        Err(e) if e.kind() == ErrorKind::AddrInUse => {
            // The port is taken. Confirm via the PID file that it is genuinely a
            // wherry bridge, rather than blindly assuming so.
            if bridge::is_running(port) {
                println!("Bridge already running on port {port}.");
            } else {
                println!(
                    "Port {port} is already in use but no wherry bridge was found there. \
                     Assuming a compatible proxy is present; use a different --port otherwise."
                );
            }
            return Ok(());
        }
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!("failed to bind SOCKS port {port}")))
        }
    };

    let key_name = format!("{profile}-bridge-key");
    let public_key = ssh::ensure_key_pair(&key_name)?;
    let key_path = ssh::key_path(&key_name)?;

    // Phase 1: AWS work (and any interactive bastion selection) on a temporary
    // runtime that is dropped before we fork.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build runtime")?;
    let bastion = runtime.block_on(prepare(&profile, &region, &public_key, bastion_name))?;
    drop(runtime);

    println!(
        "Starting SOCKS bridge on port {port} via bastion '{}' ({})...",
        bastion.name, bastion.dns
    );

    // Phase 2: daemonize and serve. Returns only on daemon startup failure.
    bridge::spawn(
        listener,
        BridgeParams {
            key_path,
            user: bastion.os_user().to_string(),
            host: bastion.dns,
        },
    )
}

/// Stop one or all running bastion bridges.
///
/// With `--all`, every bridge that left a PID file is stopped; otherwise the
/// bridge on `port` (defaulting to the standard bridge port) is targeted.
pub fn stop(port: Option<u16>, all: bool) -> Result<()> {
    let ports = if all {
        let ports = bridge::running_ports();
        if ports.is_empty() {
            println!("No wherry bridges found.");
            return Ok(());
        }
        ports
    } else {
        vec![port.unwrap_or(DEFAULT_BRIDGE_PORT)]
    };

    for port in ports {
        match bridge::stop(port)? {
            StopOutcome::Stopped { pid } => {
                println!("Stopped bridge on port {port} (pid {pid}).")
            }
            StopOutcome::NotRunning => println!("No bridge running on port {port}."),
            StopOutcome::Stale => {
                println!("Cleared a stale bridge PID file for port {port}.")
            }
        }
    }

    Ok(())
}

/// Resolve the target bastion and push our public key to it.
async fn prepare(
    profile: &str,
    region: &str,
    public_key: &str,
    bastion_name: Option<String>,
) -> Result<Bastion> {
    session::with_sso_retry(profile, || async {
        let config = session::load_config(profile, region).await;

        let bastions = aws_bastion::find_bastions(&config).await?;
        let bastion = match bastion_name.clone() {
            Some(name) => resolve_pinned_bastion(bastions, &name)?,
            None => select_bastion(bastions)?,
        };

        if !aws_bastion::send_public_key(&config, &bastion, public_key).await? {
            bail!("EC2 Instance Connect rejected the SSH public key");
        }

        Ok(bastion)
    })
    .await
}
