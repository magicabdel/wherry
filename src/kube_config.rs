use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use serde_yaml::Value;

use crate::prompt;

/// Whether `add_entry` actually wrote the config or the user backed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Written,
    Aborted,
}

/// Parameters needed to add a cluster to the kube config.
pub struct KubeEntry {
    pub alias: String,
    pub cluster_name: String,
    pub region: String,
    pub profile: String,
    pub endpoint: String,
    pub certificate_authority: String,
    pub use_bastion: bool,
    pub bastion_name: Option<String>,
    pub port: u16,
}

/// Append cluster, user and context entries for `entry` to `~/.kube/config`
/// and make it the current context. If an entry with the same alias already
/// exists, the user is warned and asked to confirm the overwrite.
pub fn add_entry(entry: &KubeEntry) -> Result<WriteOutcome> {
    let path = config_path()?;

    // Start from the existing config, or a fresh skeleton if there isn't one yet
    // (e.g. a machine that has never run kubectl).
    let mut config: Value = match fs::read_to_string(&path) {
        Ok(contents) => {
            let parsed: Value =
                serde_yaml::from_str(&contents).context("failed to parse kube config")?;
            // An empty file parses to `Null`; treat it as a fresh config.
            if parsed.is_null() {
                default_config()
            } else {
                parsed
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => default_config(),
        Err(e) => {
            return Err(anyhow::Error::new(e)
                .context(format!("failed to read kube config at {}", path.display())))
        }
    };

    let mapping = config
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("kube config is not a mapping"))?;

    // Warn before clobbering an existing entry, so re-running for a cluster is a
    // deliberate choice rather than a silent overwrite.
    if ["clusters", "users", "contexts"]
        .iter()
        .any(|key| name_exists(mapping, key, &entry.alias))
    {
        eprintln!(
            "warning: an entry named '{}' already exists in {} and will be overwritten.",
            entry.alias,
            path.display()
        );
        if !prompt::confirm("Overwrite the existing entry?", false)? {
            println!("Left kube config unchanged.");
            return Ok(WriteOutcome::Aborted);
        }
    }

    upsert_into(mapping, "clusters", to_value(cluster_section(entry))?)?;
    upsert_into(mapping, "users", to_value(user_section(entry))?)?;
    upsert_into(mapping, "contexts", to_value(context_section(entry))?)?;

    mapping.insert(
        Value::String("current-context".into()),
        Value::String(entry.alias.clone()),
    );

    let serialized = serde_yaml::to_string(&config).context("failed to serialize kube config")?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    }
    fs::write(&path, serialized).with_context(|| format!("failed to write {}", path.display()))?;

    Ok(WriteOutcome::Written)
}

/// A minimal, valid kube config skeleton used when no config exists yet.
fn default_config() -> Value {
    let mut mapping = serde_yaml::Mapping::new();
    mapping.insert(
        Value::String("apiVersion".into()),
        Value::String("v1".into()),
    );
    mapping.insert(Value::String("kind".into()), Value::String("Config".into()));
    mapping.insert(
        Value::String("preferences".into()),
        Value::Mapping(serde_yaml::Mapping::new()),
    );
    mapping.insert(
        Value::String("clusters".into()),
        Value::Sequence(Vec::new()),
    );
    mapping.insert(
        Value::String("contexts".into()),
        Value::Sequence(Vec::new()),
    );
    mapping.insert(Value::String("users".into()), Value::Sequence(Vec::new()));
    Value::Mapping(mapping)
}

fn config_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".kube/config"))
}

/// Insert `value` into the sequence stored under `key`, replacing any existing
/// entry that shares the same `name` (so re-running for a cluster updates it in
/// place instead of creating duplicates). If several entries already share that
/// name — e.g. a config polluted by an earlier version — the first is updated
/// and the rest are dropped, repairing the config on the next run. Creates the
/// sequence if needed.
fn upsert_into(mapping: &mut serde_yaml::Mapping, key: &str, value: Value) -> Result<()> {
    let entry = mapping
        .entry(Value::String(key.to_string()))
        .or_insert_with(|| Value::Sequence(Vec::new()));

    let seq = match entry {
        Value::Sequence(seq) => seq,
        // `kubectl` writes `users: null` for an empty config; treat it as empty.
        Value::Null => {
            *entry = Value::Sequence(Vec::new());
            entry
                .as_sequence_mut()
                .expect("just set entry to a sequence")
        }
        _ => return Err(anyhow!("kube config field '{key}' is not a list")),
    };

    // Replace the first entry with the same name and remove any further
    // duplicates; append when it is genuinely new.
    if let Some(name) = value.get("name").cloned() {
        let mut replaced = false;
        seq.retain_mut(|item| {
            if item.get("name") != Some(&name) {
                return true;
            }
            if replaced {
                // A duplicate of an entry we already updated: drop it.
                false
            } else {
                *item = value.clone();
                replaced = true;
                true
            }
        });
        if replaced {
            return Ok(());
        }
    }

    seq.push(value);
    Ok(())
}

/// Whether a sequence under `key` contains an entry with the given `name`.
fn name_exists(mapping: &serde_yaml::Mapping, key: &str, name: &str) -> bool {
    let target = Value::String(name.to_string());
    mapping
        .get(Value::String(key.to_string()))
        .and_then(Value::as_sequence)
        .is_some_and(|seq| seq.iter().any(|item| item.get("name") == Some(&target)))
}

fn to_value<T: Serialize>(value: T) -> Result<Value> {
    serde_yaml::to_value(value).context("failed to build kube config section")
}

#[derive(Serialize)]
struct ClusterEntry {
    name: String,
    cluster: ClusterSpec,
}

#[derive(Serialize)]
struct ClusterSpec {
    #[serde(rename = "certificate-authority-data")]
    certificate_authority_data: String,
    server: String,
    #[serde(rename = "proxy-url", skip_serializing_if = "Option::is_none")]
    proxy_url: Option<String>,
}

fn cluster_section(entry: &KubeEntry) -> ClusterEntry {
    ClusterEntry {
        name: entry.alias.clone(),
        cluster: ClusterSpec {
            certificate_authority_data: entry.certificate_authority.clone(),
            server: entry.endpoint.clone(),
            proxy_url: entry
                .use_bastion
                .then(|| format!("socks5://localhost:{}", entry.port)),
        },
    }
}

#[derive(Serialize)]
struct UserEntry {
    name: String,
    user: UserSpec,
}

#[derive(Serialize)]
struct UserSpec {
    exec: ExecSpec,
}

#[derive(Serialize)]
struct ExecSpec {
    #[serde(rename = "apiVersion")]
    api_version: String,
    command: String,
    args: Vec<String>,
    env: Option<()>,
    #[serde(rename = "interactiveMode")]
    interactive_mode: String,
    #[serde(rename = "provideClusterInfo")]
    provide_cluster_info: bool,
}

fn user_section(entry: &KubeEntry) -> UserEntry {
    let auth_command = format!(
        "aws --region {region} eks get-token --cluster-name {cluster} --profile {profile}",
        region = entry.region,
        cluster = entry.cluster_name,
        profile = entry.profile,
    );

    // When routing through a bastion, bring the SOCKS bridge up before
    // requesting a token. A specific bastion is pinned when one was selected.
    // Each step only reaches for `wherry sso login` reactively, after it has
    // actually failed, so a healthy token costs nothing extra on every
    // `kubectl` call; its output goes to stderr so stdout stays clean JSON.
    let script = if entry.use_bastion {
        let bastion_arg = entry
            .bastion_name
            .as_deref()
            .map(|name| format!(" --bastion {name}"))
            .unwrap_or_default();
        let bridge_start = format!(
            "wherry bridge start --profile {profile} --region {region} --port {port}{bastion_arg} > /dev/null",
            profile = entry.profile,
            region = entry.region,
            port = entry.port,
        );
        format!(
            "{bridge};\n{token};",
            bridge = retry_after_sso_login(&bridge_start, &entry.profile),
            token = retry_after_sso_login(&auth_command, &entry.profile),
        )
    } else {
        format!("{};", retry_after_sso_login(&auth_command, &entry.profile))
    };

    UserEntry {
        name: entry.alias.clone(),
        user: UserSpec {
            exec: ExecSpec {
                api_version: "client.authentication.k8s.io/v1beta1".into(),
                command: "/bin/sh".into(),
                args: vec!["-c".into(), script],
                env: None,
                interactive_mode: "IfAvailable".into(),
                provide_cluster_info: false,
            },
        },
    }
}

/// Wrap `command` so that, if it fails, `wherry sso login` is run for
/// `profile` (a no-op unless the profile's SSO token has actually expired)
/// and `command` is retried exactly once. `wherry sso login`'s own output
/// goes to stderr, so nothing but `command`'s stdout ever appears on stdout.
fn retry_after_sso_login(command: &str, profile: &str) -> String {
    format!("{command} || (wherry sso login --profile {profile} --quiet 1>&2 && {command})")
}

#[derive(Serialize)]
struct ContextEntry {
    name: String,
    context: ContextSpec,
}

#[derive(Serialize)]
struct ContextSpec {
    cluster: String,
    namespace: String,
    user: String,
}

fn context_section(entry: &KubeEntry) -> ContextEntry {
    ContextEntry {
        name: entry.alias.clone(),
        context: ContextSpec {
            cluster: entry.alias.clone(),
            namespace: "default".into(),
            user: entry.alias.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(use_bastion: bool) -> KubeEntry {
        KubeEntry {
            alias: "my-cluster".into(),
            cluster_name: "eks-1234".into(),
            region: "eu-west-1".into(),
            profile: "profile-1234".into(),
            endpoint: "https://example.eks.amazonaws.com".into(),
            certificate_authority: "CA_DATA".into(),
            use_bastion,
            bastion_name: use_bastion.then(|| "bastion-lb".to_string()),
            port: 7777,
        }
    }

    #[test]
    fn private_cluster_gets_proxy_and_bridge() {
        let e = entry(true);
        let cluster = cluster_section(&e);
        assert_eq!(
            cluster.cluster.proxy_url.as_deref(),
            Some("socks5://localhost:7777")
        );

        let script = &user_section(&e).user.exec.args[1];
        assert!(script.contains("wherry bridge start"));
        assert!(script.contains("--bastion bastion-lb"));
        assert!(script.contains("aws --region eu-west-1 eks get-token"));
        // The refresh only happens reactively, after a failure.
        assert!(script.contains("|| (wherry sso login --profile profile-1234 --quiet"));
        assert!(!script.starts_with("wherry sso login"));
    }

    #[test]
    fn public_cluster_has_no_proxy_or_bridge() {
        let e = entry(false);
        assert!(cluster_section(&e).cluster.proxy_url.is_none());

        let script = &user_section(&e).user.exec.args[1];
        assert!(!script.contains("wherry bridge start"));
        assert!(script.contains("aws --region eu-west-1 eks get-token"));
        assert!(script.contains("|| (wherry sso login --profile profile-1234 --quiet"));
        assert!(!script.starts_with("wherry sso login"));
    }

    #[test]
    fn retry_after_sso_login_wraps_command_with_a_single_retry() {
        let wrapped = retry_after_sso_login("do-thing", "my-profile");
        assert_eq!(
            wrapped,
            "do-thing || (wherry sso login --profile my-profile --quiet 1>&2 && do-thing)"
        );
    }

    #[test]
    fn upsert_into_creates_and_appends() {
        let mut mapping = serde_yaml::Mapping::new();
        upsert_into(&mut mapping, "clusters", named("a")).unwrap();
        upsert_into(&mut mapping, "clusters", named("b")).unwrap();

        let seq = mapping
            .get(Value::String("clusters".into()))
            .and_then(Value::as_sequence)
            .unwrap();
        assert_eq!(seq.len(), 2);
    }

    #[test]
    fn upsert_into_replaces_entry_with_same_name() {
        // Re-running for the same cluster must update in place, not duplicate.
        let mut mapping = serde_yaml::Mapping::new();
        upsert_into(&mut mapping, "clusters", named_with("a", "old")).unwrap();
        upsert_into(&mut mapping, "clusters", named_with("a", "new")).unwrap();

        let seq = mapping
            .get(Value::String("clusters".into()))
            .and_then(Value::as_sequence)
            .unwrap();
        assert_eq!(seq.len(), 1);
        assert_eq!(seq[0].get("payload"), Some(&Value::String("new".into())));
    }

    #[test]
    fn upsert_into_repairs_pre_existing_duplicates() {
        // A config polluted by an earlier version has several entries with the
        // same name; upserting should collapse them to a single updated entry.
        let mut mapping = serde_yaml::Mapping::new();
        mapping.insert(
            Value::String("clusters".into()),
            Value::Sequence(vec![
                named_with("a", "old"),
                named("b"),
                named_with("a", "old"),
            ]),
        );

        upsert_into(&mut mapping, "clusters", named_with("a", "new")).unwrap();

        let seq = mapping
            .get(Value::String("clusters".into()))
            .and_then(Value::as_sequence)
            .unwrap();
        // One "a" (updated) plus the untouched "b".
        assert_eq!(seq.len(), 2);
        let a_entries: Vec<_> = seq
            .iter()
            .filter(|item| item.get("name") == Some(&Value::String("a".into())))
            .collect();
        assert_eq!(a_entries.len(), 1);
        assert_eq!(
            a_entries[0].get("payload"),
            Some(&Value::String("new".into()))
        );
    }

    #[test]
    fn name_exists_detects_present_and_absent() {
        let mut mapping = serde_yaml::Mapping::new();
        upsert_into(&mut mapping, "contexts", named("here")).unwrap();
        assert!(name_exists(&mapping, "contexts", "here"));
        assert!(!name_exists(&mapping, "contexts", "missing"));
        assert!(!name_exists(&mapping, "clusters", "here"));
    }

    #[test]
    fn upsert_into_replaces_null_field() {
        // `kubectl` writes `users: null` for an empty config; we must handle it.
        let mut mapping = serde_yaml::Mapping::new();
        mapping.insert(Value::String("users".into()), Value::Null);
        upsert_into(&mut mapping, "users", named("u")).unwrap();

        let seq = mapping
            .get(Value::String("users".into()))
            .and_then(Value::as_sequence)
            .unwrap();
        assert_eq!(seq.len(), 1);
    }

    #[test]
    fn default_config_is_a_usable_skeleton() {
        // A fresh config must let us append cluster/user/context entries.
        let mut config = default_config();
        let mapping = config.as_mapping_mut().unwrap();
        assert_eq!(
            mapping.get(Value::String("kind".into())),
            Some(&Value::String("Config".into()))
        );

        upsert_into(mapping, "clusters", named("c")).unwrap();
        let seq = mapping
            .get(Value::String("clusters".into()))
            .and_then(Value::as_sequence)
            .unwrap();
        assert_eq!(seq.len(), 1);
    }

    fn named(name: &str) -> Value {
        let mut mapping = serde_yaml::Mapping::new();
        mapping.insert(Value::String("name".into()), Value::String(name.into()));
        Value::Mapping(mapping)
    }

    fn named_with(name: &str, payload: &str) -> Value {
        let mut mapping = serde_yaml::Mapping::new();
        mapping.insert(Value::String("name".into()), Value::String(name.into()));
        mapping.insert(
            Value::String("payload".into()),
            Value::String(payload.into()),
        );
        Value::Mapping(mapping)
    }
}
