//! Self-update from the project's GitHub releases.
//!
//! Mirrors what `install.sh` does — download the release tarball for this
//! platform and verify its checksum — but replaces the currently running
//! executable in place, wherever it was installed (installer, cargo, ...).

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::Archive;

const REPO: &str = "magicabdel/wherry";
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Update the running `wherry` binary to the latest GitHub release.
pub fn update() -> Result<()> {
    let agent = ureq::builder()
        .timeout(Duration::from_secs(120))
        .user_agent(concat!("wherry/", env!("CARGO_PKG_VERSION")))
        .build();

    let latest = latest_version(&agent)?;
    if !is_newer(&latest, CURRENT) {
        println!("wherry v{CURRENT} is already up to date (latest release: v{latest}).");
        return Ok(());
    }

    let target = release_target()?;
    println!("Updating wherry v{CURRENT} -> v{latest} ({target})...");

    let url =
        format!("https://github.com/{REPO}/releases/download/v{latest}/wherry-{target}.tar.gz");
    let archive = download(&agent, &url)?;
    let checksum_file = String::from_utf8(download(&agent, &format!("{url}.sha256"))?)
        .context("checksum file is not valid UTF-8")?;
    verify_checksum(&archive, &checksum_file)?;

    let binary = extract_binary(&archive)?;
    replace_current_exe(&binary)?;

    println!("Updated to v{latest}.");
    Ok(())
}

/// Latest released version (without the `v` prefix), discovered by following
/// the `releases/latest` redirect to its `releases/tag/v<version>` page. This
/// avoids the GitHub API and its rate limits entirely.
fn latest_version(agent: &ureq::Agent) -> Result<String> {
    let url = format!("https://github.com/{REPO}/releases/latest");
    let response = agent
        .get(&url)
        .call()
        .with_context(|| format!("failed to query {url}"))?;
    let final_url = response.get_url().to_string();
    let tag = final_url
        .rsplit_once("/tag/")
        .map(|(_, tag)| tag)
        .ok_or_else(|| anyhow!("no release found at {url} (landed on {final_url})"))?;
    Ok(tag.trim_start_matches('v').to_string())
}

/// Whether `latest` is strictly newer than `current`. Falls back to a plain
/// inequality check when either side is not a `major.minor.patch` triple.
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => latest != current,
    }
}

fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// The release artifact target triple for the running platform, matching the
/// names produced by the release workflow (and expected by `install.sh`).
fn release_target() -> Result<String> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    match (os, arch) {
        ("linux", "x86_64" | "aarch64") => Ok(format!("{arch}-unknown-linux-musl")),
        // Only Apple Silicon builds are published for macOS.
        ("macos", "aarch64") => Ok(format!("{arch}-apple-darwin")),
        _ => Err(anyhow!(
            "no prebuilt wherry release for {os}/{arch}; try 'cargo install wherry' instead"
        )),
    }
}

fn download(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
    let response = agent
        .get(url)
        .call()
        .with_context(|| format!("failed to download {url}"))?;
    let mut buffer = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut buffer)
        .with_context(|| format!("failed to read {url}"))?;
    Ok(buffer)
}

/// Check `archive` against a `shasum -a 256` style file (`<hex>  <name>`).
fn verify_checksum(archive: &[u8], checksum_file: &str) -> Result<()> {
    let expected = checksum_file
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("release checksum file is empty"))?
        .to_lowercase();
    let actual = hex::encode(Sha256::digest(archive));
    if actual != expected {
        bail!("checksum mismatch for the downloaded release: expected {expected}, got {actual}");
    }
    Ok(())
}

/// Pull the `wherry` binary out of the release tarball.
fn extract_binary(archive: &[u8]) -> Result<Vec<u8>> {
    let mut tar = Archive::new(GzDecoder::new(archive));
    for entry in tar.entries().context("failed to read release archive")? {
        let mut entry = entry.context("failed to read release archive entry")?;
        let is_wherry = entry
            .path()
            .map(|path| path.as_ref() == Path::new("wherry"))
            .unwrap_or(false);
        if is_wherry {
            let mut binary = Vec::new();
            entry
                .read_to_end(&mut binary)
                .context("failed to extract the wherry binary")?;
            return Ok(binary);
        }
    }
    bail!("release archive does not contain a 'wherry' binary")
}

/// Swap the running executable for `binary`.
///
/// The new file is written next to the current one (same filesystem) so the
/// final rename is atomic; the running process keeps executing its old,
/// now-unlinked image and the new binary is picked up on the next invocation.
fn replace_current_exe(binary: &[u8]) -> Result<()> {
    let exe = std::env::current_exe().context("could not locate the running wherry binary")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("executable path {} has no parent", exe.display()))?;
    let tmp = dir.join(".wherry-update.tmp");

    let result = (|| {
        std::fs::write(&tmp, binary).with_context(|| {
            format!(
                "failed to write to {} — is the install directory writable?",
                dir.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
                .context("failed to mark the new binary executable")?;
        }
        std::fs::rename(&tmp, &exe).with_context(|| format!("failed to replace {}", exe.display()))
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_compares_semver_triples() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn is_newer_falls_back_to_inequality_for_odd_versions() {
        assert!(is_newer("0.2.0-rc1", "0.1.0"));
        // Non-semver versions can't be ordered; any difference means "update".
        assert!(is_newer("0.1.0", "0.1.0-dev"));
        assert!(!is_newer("0.1.0-dev", "0.1.0-dev"));
    }

    #[test]
    fn verify_checksum_accepts_matching_digest() {
        let data = b"hello";
        let file = format!("{}  wherry-x.tar.gz\n", hex::encode(Sha256::digest(data)));
        assert!(verify_checksum(data, &file).is_ok());
    }

    #[test]
    fn verify_checksum_rejects_mismatch() {
        let file = format!(
            "{}  wherry-x.tar.gz\n",
            hex::encode(Sha256::digest(b"other"))
        );
        assert!(verify_checksum(b"hello", &file).is_err());
    }

    #[test]
    fn extract_binary_finds_wherry_in_tarball() {
        // Build a tiny tar.gz containing a `wherry` file.
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        let payload = b"#!/bin/sh\n";
        let mut header = tar::Header::new_gnu();
        header.set_path("wherry").unwrap();
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append(&header, payload.as_slice()).unwrap();
        let archive = builder.into_inner().unwrap().finish().unwrap();

        assert_eq!(extract_binary(&archive).unwrap(), payload);
    }

    #[test]
    fn extract_binary_rejects_archive_without_wherry() {
        let builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        let archive = builder.into_inner().unwrap().finish().unwrap();
        assert!(extract_binary(&archive).is_err());
    }
}
