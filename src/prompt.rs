use anyhow::{anyhow, Context, Result};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};

use crate::aws::profiles;

/// Resolve an AWS profile: use the provided value, otherwise let the user pick
/// from the locally configured profiles (falling back to free-text input).
pub fn resolve_profile(provided: Option<String>) -> Result<String> {
    if let Some(profile) = provided {
        return Ok(profile);
    }

    let profiles = profiles::list_profiles();

    match profiles.len() {
        0 => text_input("AWS profile"),
        1 => Ok(profiles.into_iter().next().unwrap()),
        _ => {
            let index = select("Select an AWS profile", &profiles)?;
            Ok(profiles[index].clone())
        }
    }
}

/// Present a selection menu and return the chosen index.
pub fn select(prompt: &str, items: &[String]) -> Result<usize> {
    if items.is_empty() {
        return Err(anyhow!("nothing to select for '{prompt}'"));
    }

    Select::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .items(items)
        .default(0)
        .interact()
        .context("selection cancelled")
}

/// Prompt for a free-text value.
pub fn text_input(prompt: &str) -> Result<String> {
    Input::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .interact_text()
        .context("input cancelled")
}

/// Prompt for a free-text value pre-filled with a default; pressing Enter
/// accepts the default.
pub fn text_input_with_default(prompt: &str, default: &str) -> Result<String> {
    Input::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .default(default.to_string())
        .interact_text()
        .context("input cancelled")
}

/// Ask a yes/no question with the given default.
pub fn confirm(prompt: &str, default: bool) -> Result<bool> {
    Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .default(default)
        .interact()
        .context("confirmation cancelled")
}
