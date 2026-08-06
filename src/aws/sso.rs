use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use sha1::{Digest, Sha1};

use crate::aws::config_file::{self, Section, SectionKind};

/// Token is considered expired this many seconds before its real expiry, so a
/// call that starts "just in time" does not race the SSO service.
const EXPIRY_SKEW_SECS: i64 = 60;

/// An SSO login target: either a modern `sso-session` (shared by several
/// profiles) or a legacy profile that embeds its own `sso_start_url` directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Session {
        name: String,
        start_url: String,
        region: String,
    },
    LegacyProfile {
        profile: String,
        start_url: String,
        region: String,
    },
}

impl Target {
    pub fn label(&self) -> String {
        match self {
            Target::Session {
                name, start_url, ..
            } => {
                format!("session '{name}' ({start_url})")
            }
            Target::LegacyProfile {
                profile, start_url, ..
            } => {
                format!("profile '{profile}' ({start_url})")
            }
        }
    }

    fn start_url(&self) -> &str {
        match self {
            Target::Session { start_url, .. } => start_url,
            Target::LegacyProfile { start_url, .. } => start_url,
        }
    }

    /// The name AWS CLI/SDK hash into the cache file name under
    /// `~/.aws/sso/cache`: the `sso-session` name for session-based profiles,
    /// the start URL itself for legacy ones.
    fn cache_seed(&self) -> &str {
        match self {
            Target::Session { name, .. } => name,
            Target::LegacyProfile { start_url, .. } => start_url,
        }
    }

    fn cache_key(&self) -> String {
        let mut hasher = Sha1::new();
        hasher.update(self.cache_seed().as_bytes());
        hex(&hasher.finalize())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Result of [`ensure_fresh`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureOutcome {
    /// The profile does not use SSO; nothing to do.
    NotSso,
    /// A cached token already covers the call that is about to happen.
    AlreadyValid,
    /// The token was expired (or missing) and `aws sso login` refreshed it.
    LoggedIn,
}

/// Freshness of a cached SSO token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenStatus {
    Valid { remaining_secs: i64 },
    Expired,
    Missing,
}

/// Make sure `profile`'s SSO token (if any) is valid, running
/// `aws sso login` when it has expired or is missing.
///
/// `announce_valid` controls whether an already-valid token is reported; the
/// expiry/login/relogin messages are always printed to stderr since they
/// explain why a browser window might be about to open.
pub fn ensure_fresh(profile: &str, announce_valid: bool) -> Result<EnsureOutcome> {
    match resolve_for_profile(profile)? {
        Some(target) => ensure_fresh_target(&target, announce_valid),
        None => Ok(EnsureOutcome::NotSso),
    }
}

/// Same as [`ensure_fresh`] but for an already-resolved [`Target`].
pub fn ensure_fresh_target(target: &Target, announce_valid: bool) -> Result<EnsureOutcome> {
    match token_status(target) {
        TokenStatus::Valid { remaining_secs } => {
            if announce_valid {
                eprintln!(
                    "SSO token for {} is valid for {}.",
                    target.label(),
                    format_duration(remaining_secs)
                );
            }
            Ok(EnsureOutcome::AlreadyValid)
        }
        TokenStatus::Expired | TokenStatus::Missing => {
            eprintln!(
                "SSO token for {} has expired or is missing; running `aws sso login`...",
                target.label()
            );
            login(target)?;
            match token_status(target) {
                TokenStatus::Valid { .. } => {
                    eprintln!("Logged in to {}.", target.label());
                    Ok(EnsureOutcome::LoggedIn)
                }
                _ => bail!(
                    "`aws sso login` completed but no valid token was found for {} afterwards",
                    target.label()
                ),
            }
        }
    }
}

/// Run `aws sso login` for the given target, letting it drive the interactive
/// device-authorization flow (opening a browser, printing a code, ...) on the
/// inherited stdio.
pub fn login(target: &Target) -> Result<()> {
    let mut command = Command::new("aws");
    command.arg("sso").arg("login");
    match target {
        Target::Session { name, .. } => {
            command.arg("--sso-session").arg(name);
        }
        Target::LegacyProfile { profile, .. } => {
            command.arg("--profile").arg(profile);
        }
    }
    // Never paginate: there is nothing worth paging and a pager can eat the
    // device-authorization prompt in non-interactive terminals.
    command.env("AWS_PAGER", "");

    let status = command
        .status()
        .context("failed to run `aws sso login` (is the AWS CLI installed and on PATH?)")?;

    if !status.success() {
        bail!("`aws sso login` exited with {status}");
    }
    Ok(())
}

/// Every SSO login target configured locally: `sso-session` blocks first,
/// then legacy profiles with an inline `sso_start_url`. Suitable for an
/// interactive picker.
pub fn list_targets() -> Vec<Target> {
    let sections = config_file::load_config();
    let mut targets = Vec::new();

    for section in sections
        .iter()
        .filter(|s| s.kind == SectionKind::SsoSession)
    {
        if let Some(start_url) = section.get("sso_start_url") {
            targets.push(Target::Session {
                name: section.name.clone(),
                start_url: start_url.to_string(),
                region: section.get("sso_region").unwrap_or("us-east-1").to_string(),
            });
        }
    }

    for section in sections.iter().filter(|s| s.kind == SectionKind::Profile) {
        if section.get("sso_session").is_some() {
            continue; // Already represented by its `sso-session` above.
        }
        if let Some(start_url) = section.get("sso_start_url") {
            targets.push(Target::LegacyProfile {
                profile: section.name.clone(),
                start_url: start_url.to_string(),
                region: section.get("sso_region").unwrap_or("us-east-1").to_string(),
            });
        }
    }

    targets
}

/// Resolve an SSO login target by session or profile name.
///
/// An `sso-session` name is tried first (sessions and profiles do not share a
/// namespace, but this makes `wherry sso login <name>`-style lookups do the
/// intuitive thing); otherwise `name` is resolved as a profile, following
/// `source_profile` chains.
pub fn resolve_by_name(name: &str) -> Result<Target> {
    let sections = config_file::load_config();

    if let Some(section) = find_section(&sections, SectionKind::SsoSession, name) {
        return session_target(section);
    }

    match resolve_for_profile_in(&sections, name, &mut HashSet::new())? {
        Some(target) => Ok(target),
        None => bail!("'{name}' is neither an sso-session nor a profile with SSO configuration"),
    }
}

/// Resolve the SSO login target that backs `profile`, following
/// `source_profile` chains (for assumed-role profiles built on top of an SSO
/// profile). Returns `None` when the profile does not use SSO at all.
pub fn resolve_for_profile(profile: &str) -> Result<Option<Target>> {
    let sections = config_file::load_config();
    resolve_for_profile_in(&sections, profile, &mut HashSet::new())
}

fn resolve_for_profile_in(
    sections: &[Section],
    profile: &str,
    seen: &mut HashSet<String>,
) -> Result<Option<Target>> {
    if !seen.insert(profile.to_string()) {
        bail!("circular source_profile chain involving '{profile}'");
    }

    let Some(section) = find_section(sections, SectionKind::Profile, profile) else {
        // Unknown profile: let the AWS SDK report the real error later.
        return Ok(None);
    };

    if let Some(session_name) = section.get("sso_session") {
        let session =
            find_section(sections, SectionKind::SsoSession, session_name).ok_or_else(|| {
                anyhow!(
                    "profile '{profile}' references sso-session '{session_name}', which is \
                     not defined in ~/.aws/config"
                )
            })?;
        return session_target(session).map(Some);
    }

    if let Some(start_url) = section.get("sso_start_url") {
        return Ok(Some(Target::LegacyProfile {
            profile: profile.to_string(),
            start_url: start_url.to_string(),
            region: section.get("sso_region").unwrap_or("us-east-1").to_string(),
        }));
    }

    if let Some(source) = section.get("source_profile") {
        let source = source.to_string();
        return resolve_for_profile_in(sections, &source, seen);
    }

    Ok(None)
}

fn session_target(session: &Section) -> Result<Target> {
    let start_url = session
        .get("sso_start_url")
        .ok_or_else(|| anyhow!("sso-session '{}' has no sso_start_url", session.name))?
        .to_string();
    Ok(Target::Session {
        name: session.name.clone(),
        start_url,
        region: session.get("sso_region").unwrap_or("us-east-1").to_string(),
    })
}

fn find_section<'a>(sections: &'a [Section], kind: SectionKind, name: &str) -> Option<&'a Section> {
    sections.iter().find(|s| s.kind == kind && s.name == name)
}

/// Current freshness of the cached token for `target`.
pub fn token_status(target: &Target) -> TokenStatus {
    let Some(info) = find_token(target) else {
        return TokenStatus::Missing;
    };
    let Some(expires_at) = info.expires_at else {
        return TokenStatus::Missing;
    };

    let remaining = expires_at - now_epoch() - EXPIRY_SKEW_SECS;
    if remaining > 0 {
        TokenStatus::Valid {
            remaining_secs: remaining,
        }
    } else {
        TokenStatus::Expired
    }
}

struct CachedToken {
    start_url: Option<String>,
    expires_at: Option<i64>,
}

#[derive(Deserialize)]
struct CacheFile {
    #[serde(rename = "startUrl")]
    start_url: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<String>,
}

fn cache_dir() -> Option<PathBuf> {
    config_file::home_dir().map(|home| home.join(".aws/sso/cache"))
}

fn read_cache_file(path: &std::path::Path) -> Option<CachedToken> {
    let contents = fs::read_to_string(path).ok()?;
    // The SSO cache files are plain JSON; YAML is a superset so this reuses
    // the `serde_yaml` dependency already pulled in for the kube config.
    let parsed: CacheFile = serde_yaml::from_str(&contents).ok()?;
    Some(CachedToken {
        start_url: parsed.start_url,
        expires_at: parsed.expires_at.as_deref().and_then(parse_rfc3339),
    })
}

/// Locate the cached token for `target`: first by the exact cache file name
/// the AWS CLI/SDK would use, then (for cache layouts we don't predict
/// perfectly) by scanning the cache directory for a token whose start URL
/// matches, keeping the one that expires furthest in the future.
fn find_token(target: &Target) -> Option<CachedToken> {
    let dir = cache_dir()?;

    let direct = dir.join(format!("{}.json", target.cache_key()));
    if let Some(token) = read_cache_file(&direct) {
        if token.expires_at.is_some() {
            return Some(token);
        }
    }

    let target_url = normalize_url(target.start_url());
    let mut best: Option<CachedToken> = None;

    for entry in fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(token) = read_cache_file(&path) else {
            continue;
        };
        let Some(url) = &token.start_url else {
            continue;
        };
        if normalize_url(url) != target_url {
            continue;
        }

        let is_better = match &best {
            None => true,
            Some(current) => token.expires_at.unwrap_or(0) > current.expires_at.unwrap_or(0),
        };
        if is_better {
            best = Some(token);
        }
    }

    best
}

/// Normalize a start URL for comparison: lowercase, and without a trailing
/// `/` and/or `#` (the SSO portal URL is commonly stored with a trailing
/// `#/` fragment, but not always consistently so across files).
fn normalize_url(url: &str) -> String {
    let mut s = url.trim().to_ascii_lowercase();
    while s.ends_with('/') || s.ends_with('#') {
        s.pop();
    }
    s
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Format a positive duration in seconds as e.g. `3d2h`, `7h52m`, `45m`.
pub fn format_duration(total_secs: i64) -> String {
    let total_secs = total_secs.max(0);
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3600;
    let minutes = (total_secs % 3600) / 60;

    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        "less than a minute".to_string()
    }
}

/// Parse an RFC 3339 timestamp (as used by the SSO token cache, e.g.
/// `2026-08-04T16:51:18Z`) into seconds since the Unix epoch. Implemented by
/// hand to avoid pulling in a datetime crate for a single call site.
fn parse_rfc3339(input: &str) -> Option<i64> {
    let s = input.trim();
    let (date_part, rest) = s.split_once('T')?;

    let mut ymd = date_part.split('-');
    let year: i64 = ymd.next()?.parse().ok()?;
    let month: i64 = ymd.next()?.parse().ok()?;
    let day: i64 = ymd.next()?.parse().ok()?;

    let (time_part, offset_secs) = split_offset(rest)?;

    let mut hms = time_part.split(':');
    let hour: i64 = hms.next()?.parse().ok()?;
    let minute: i64 = hms.next()?.parse().ok()?;
    // Seconds may carry a fractional part (e.g. `18.123`); whole seconds are
    // all that matter for an expiry check.
    let second: i64 = hms.next()?.split('.').next()?.parse().ok()?;

    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs)
}

/// Split off the `Z` or `+HH:MM`/`-HH:MM` timezone designator, returning the
/// remaining `HH:MM:SS[.fff]` and the offset in seconds east of UTC.
fn split_offset(time_part: &str) -> Option<(&str, i64)> {
    if let Some(stripped) = time_part.strip_suffix('Z') {
        return Some((stripped, 0));
    }

    // The offset sign never appears at index 0, so this cannot mistake the
    // (sign-less) time-of-day for one.
    for (index, ch) in time_part.char_indices() {
        if index == 0 || (ch != '+' && ch != '-') {
            continue;
        }
        let (main, offset) = time_part.split_at(index);
        let sign = if ch == '+' { 1 } else { -1 };
        let mut parts = offset[1..].split(':');
        let hh: i64 = parts.next()?.parse().ok()?;
        let mm: i64 = parts.next().unwrap_or("0").parse().ok()?;
        return Some((main, sign * (hh * 3600 + mm * 60)));
    }

    Some((time_part, 0))
}

/// Days since 1970-01-01 for a proleptic-Gregorian civil date. Standard
/// algorithm (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (month + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sections(contents: &str) -> Vec<Section> {
        config_file::parse(contents, true)
    }

    #[test]
    fn resolves_session_based_profile() {
        let sections = sections(
            "[profile llm]\nsso_session = sia\nsso_account_id = 1\nsso_role_name = Admin\nregion = eu-west-1\n\n\
             [sso-session sia]\nsso_start_url = https://example.awsapps.com/start\nsso_region = eu-west-3\n",
        );
        let target = resolve_for_profile_in(&sections, "llm", &mut HashSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(
            target,
            Target::Session {
                name: "sia".into(),
                start_url: "https://example.awsapps.com/start".into(),
                region: "eu-west-3".into(),
            }
        );
    }

    #[test]
    fn resolves_legacy_sso_profile() {
        let sections = sections(
            "[profile old]\nsso_start_url = https://example.awsapps.com/start\nsso_region = eu-west-1\nsso_account_id = 1\nsso_role_name = Admin\n",
        );
        let target = resolve_for_profile_in(&sections, "old", &mut HashSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(
            target,
            Target::LegacyProfile {
                profile: "old".into(),
                start_url: "https://example.awsapps.com/start".into(),
                region: "eu-west-1".into(),
            }
        );
    }

    #[test]
    fn follows_source_profile_chain() {
        let sections = sections(
            "[profile base]\nsso_session = sia\n\n\
             [profile assumed]\nsource_profile = base\nrole_arn = arn:aws:iam::1:role/x\n\n\
             [sso-session sia]\nsso_start_url = https://example.awsapps.com/start\nsso_region = eu-west-3\n",
        );
        let target = resolve_for_profile_in(&sections, "assumed", &mut HashSet::new())
            .unwrap()
            .unwrap();
        assert!(matches!(target, Target::Session { name, .. } if name == "sia"));
    }

    #[test]
    fn non_sso_profile_resolves_to_none() {
        let sections = sections("[profile static]\naws_access_key_id = AKIA\n");
        let target = resolve_for_profile_in(&sections, "static", &mut HashSet::new()).unwrap();
        assert!(target.is_none());
    }

    #[test]
    fn detects_circular_source_profile_chain() {
        let sections =
            sections("[profile a]\nsource_profile = b\n\n[profile b]\nsource_profile = a\n");
        assert!(resolve_for_profile_in(&sections, "a", &mut HashSet::new()).is_err());
    }

    #[test]
    fn parses_rfc3339_basic_and_offset() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2021-01-01T00:00:00Z"), Some(1_609_459_200));
        assert_eq!(
            parse_rfc3339("2021-01-01T02:00:00+02:00"),
            Some(1_609_459_200)
        );
        assert_eq!(
            parse_rfc3339("2021-01-01T00:00:00.500Z"),
            Some(1_609_459_200)
        );
    }

    #[test]
    fn normalizes_start_urls_for_comparison() {
        assert_eq!(
            normalize_url("https://example.awsapps.com/start/#/"),
            normalize_url("https://EXAMPLE.awsapps.com/start")
        );
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(30), "less than a minute");
        assert_eq!(format_duration(120), "2m");
        assert_eq!(format_duration(3 * 3600 + 5 * 60), "3h5m");
        assert_eq!(format_duration(2 * 86_400 + 3600), "2d1h");
    }
}
