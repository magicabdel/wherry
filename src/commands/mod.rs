pub mod bridge;
pub mod kubeconfig;
pub mod sso;
pub mod update;

use std::net::TcpListener;

use anyhow::{anyhow, Result};

use crate::aws::bastion::Bastion;
use crate::prompt;

/// Default local SOCKS port used for the bastion bridge.
pub const DEFAULT_BRIDGE_PORT: u16 = 7777;

/// Pick a bastion, prompting only when more than one is discovered.
pub fn select_bastion(mut bastions: Vec<Bastion>) -> Result<Bastion> {
    match bastions.len() {
        0 => Err(anyhow!("no bastion found")),
        1 => Ok(bastions.remove(0)),
        _ => {
            let labels: Vec<String> = bastions
                .iter()
                .map(|b| format!("{} ({})", b.name, b.dns))
                .collect();
            let index = prompt::select("Select a bastion", &labels)?;
            Ok(bastions.remove(index))
        }
    }
}

/// Resolve a bastion pinned by name from the discovered set, with a graceful
/// fallback for when the pinned name is gone (e.g. the bastion was renamed):
///
/// - exact name match wins;
/// - if it is missing but exactly one bastion exists, use that one (with a
///   warning), so a single-bastion setup keeps working through renames;
/// - if it is missing and several exist, fail — the choice is genuinely
///   ambiguous and should be re-made with `wherry update-kubeconfig`.
pub fn resolve_pinned_bastion(mut bastions: Vec<Bastion>, name: &str) -> Result<Bastion> {
    if let Some(index) = bastions.iter().position(|b| b.name == name) {
        return Ok(bastions.remove(index));
    }

    match bastions.len() {
        0 => Err(anyhow!("no bastion found")),
        1 => {
            eprintln!(
                "warning: pinned bastion '{name}' not found; falling back to the only \
                 available bastion '{}'",
                bastions[0].name
            );
            Ok(bastions.remove(0))
        }
        _ => {
            let available: Vec<String> = bastions.iter().map(|b| b.name.clone()).collect();
            Err(anyhow!(
                "pinned bastion '{name}' not found and several bastions exist ({}); \
                 re-run `wherry update-kubeconfig` to reselect",
                available.join(", ")
            ))
        }
    }
}

/// Pick a free TCP port by binding to port 0 and letting the OS choose.
pub fn free_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

/// Validate a user-supplied port, rejecting privileged ports.
pub fn validate_port(port: u16) -> Result<u16> {
    if port < 1024 {
        return Err(anyhow!("invalid port {port}: must be >= 1024"));
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bastion(name: &str) -> Bastion {
        Bastion {
            name: name.to_string(),
            dns: format!("{name}.example.com"),
            instance_id: "i-123".to_string(),
            availability_zone: "eu-west-1a".to_string(),
        }
    }

    #[test]
    fn pinned_exact_match_wins() {
        let bastions = vec![bastion("a"), bastion("b")];
        let chosen = resolve_pinned_bastion(bastions, "b").unwrap();
        assert_eq!(chosen.name, "b");
    }

    #[test]
    fn pinned_missing_falls_back_to_single() {
        let bastions = vec![bastion("only")];
        let chosen = resolve_pinned_bastion(bastions, "gone").unwrap();
        assert_eq!(chosen.name, "only");
    }

    #[test]
    fn pinned_missing_is_ambiguous_with_several() {
        let bastions = vec![bastion("a"), bastion("b")];
        assert!(resolve_pinned_bastion(bastions, "gone").is_err());
    }
}
