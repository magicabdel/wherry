use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use ssh_key::rand_core::OsRng;
use ssh_key::{Algorithm, LineEnding, PrivateKey};

/// Directory where bridge keys live (`~/.ssh`).
fn ssh_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".ssh"))
}

/// Path to the private key for the given key name.
pub fn key_path(name: &str) -> Result<PathBuf> {
    Ok(ssh_dir()?.join(name))
}

/// Read the public key contents for `name`, if it exists.
pub fn public_key(name: &str) -> Option<String> {
    let path = ssh_dir().ok()?.join(format!("{name}.pub"));
    fs::read_to_string(path).ok()
}

/// Return the public key for `name`, generating an ed25519 key pair natively
/// (via the `ssh-key` crate) if it does not already exist.
///
/// The private key is written in OpenSSH format with `0600` permissions and the
/// public key alongside it as `<name>.pub`, mirroring `ssh-keygen`'s output.
pub fn ensure_key_pair(name: &str) -> Result<String> {
    if let Some(key) = public_key(name) {
        return Ok(key);
    }

    let dir = ssh_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;

    generate_key_pair(&dir, name)
}

/// Generate an ed25519 key pair named `name` inside `dir` and return the public
/// key contents.
fn generate_key_pair(dir: &Path, name: &str) -> Result<String> {
    let mut private_key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)
        .context("failed to generate ed25519 key")?;
    private_key.set_comment(name);

    let private_path = dir.join(name);
    private_key
        .write_openssh_file(&private_path, LineEnding::LF)
        .with_context(|| format!("failed to write {}", private_path.display()))?;
    restrict_permissions(&private_path)?;

    let public_openssh = private_key
        .public_key()
        .to_openssh()
        .context("failed to encode public key")?;
    let public_contents = format!("{public_openssh}\n");

    let public_path = dir.join(format!("{name}.pub"));
    fs::write(&public_path, &public_contents)
        .with_context(|| format!("failed to write {}", public_path.display()))?;

    Ok(public_contents)
}

/// Restrict a private key file to owner read/write (`0600`) on Unix.
fn restrict_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_openssh_ed25519_pair() {
        let dir = std::env::temp_dir().join(format!("wherry-ssh-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();

        let public = generate_key_pair(&dir, "unit-test-key").unwrap();
        assert!(public.starts_with("ssh-ed25519 "));
        assert!(public.trim_end().ends_with("unit-test-key"));

        // Private key must be parseable back as OpenSSH.
        let contents = fs::read_to_string(dir.join("unit-test-key")).unwrap();
        let parsed = PrivateKey::from_openssh(&contents).unwrap();
        assert_eq!(parsed.algorithm(), Algorithm::Ed25519);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join("unit-test-key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        fs::remove_dir_all(&dir).ok();
    }
}
