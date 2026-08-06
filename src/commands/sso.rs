use anyhow::{anyhow, Result};

use crate::aws::sso::{self, EnsureOutcome, Target};
use crate::prompt;

/// `wherry sso login`: refresh (or establish) an SSO token, prompting for a
/// session or profile to target when neither is given explicitly.
///
/// `quiet` suppresses the "already valid" status line; the kube config
/// `exec` plugin passes it so a healthy token produces no stdout noise (its
/// invocation is itself only reached reactively, after an AWS call already
/// failed there).
pub fn login(profile: Option<String>, session: Option<String>, quiet: bool) -> Result<()> {
    match (profile, session) {
        (Some(_), Some(_)) => unreachable!("clap enforces --profile/--session are exclusive"),
        (Some(profile), None) => match sso::ensure_fresh(&profile, !quiet)? {
            EnsureOutcome::NotSso if !quiet => {
                println!("Profile '{profile}' does not use AWS SSO; nothing to refresh.");
            }
            _ => {}
        },
        (None, Some(session)) => {
            let target = sso::resolve_by_name(&session)?;
            sso::ensure_fresh_target(&target, !quiet)?;
        }
        (None, None) => {
            let target = pick_target()?;
            sso::ensure_fresh_target(&target, !quiet)?;
        }
    }
    Ok(())
}

/// `wherry sso status`: report the freshness of every configured SSO session
/// / legacy profile, or just the one asked for. Never triggers a login.
pub fn status(profile: Option<String>, session: Option<String>) -> Result<()> {
    let targets = match (profile, session) {
        (Some(_), Some(_)) => unreachable!("clap enforces --profile/--session are exclusive"),
        (Some(profile), None) => match sso::resolve_for_profile(&profile)? {
            Some(target) => vec![target],
            None => {
                println!("Profile '{profile}' does not use AWS SSO.");
                return Ok(());
            }
        },
        (None, Some(session)) => vec![sso::resolve_by_name(&session)?],
        (None, None) => sso::list_targets(),
    };

    if targets.is_empty() {
        println!("No SSO session or profile found in ~/.aws/config.");
        return Ok(());
    }

    for target in &targets {
        let line = match sso::token_status(target) {
            sso::TokenStatus::Valid { remaining_secs } => {
                format!("valid for {}", sso::format_duration(remaining_secs))
            }
            sso::TokenStatus::Expired => "expired".to_string(),
            sso::TokenStatus::Missing => "not logged in".to_string(),
        };
        println!("{}: {}", target.label(), line);
    }

    Ok(())
}

fn pick_target() -> Result<Target> {
    let mut targets = sso::list_targets();
    if targets.is_empty() {
        return Err(anyhow!(
            "no sso-session or SSO profile found in ~/.aws/config"
        ));
    }
    if targets.len() == 1 {
        return Ok(targets.remove(0));
    }

    let labels: Vec<String> = targets.iter().map(Target::label).collect();
    let index = prompt::select("Select an SSO session or profile to log in to", &labels)?;
    Ok(targets.remove(index))
}
