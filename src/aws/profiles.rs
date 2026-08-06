use crate::aws::config_file;

/// List AWS profiles configured locally by parsing `~/.aws/config` and
/// `~/.aws/credentials`.
///
/// Section headers look like `[profile foo]` in the config file and `[foo]` in
/// the credentials file (plus a bare `[default]` in either). The result is
/// de-duplicated and sorted for a stable interactive picker.
pub fn list_profiles() -> Vec<String> {
    let mut profiles: Vec<String> = config_file::load_config()
        .into_iter()
        .chain(config_file::load_credentials())
        .filter(|s| s.kind == config_file::SectionKind::Profile)
        .map(|s| s.name)
        .collect();

    profiles.sort();
    profiles.dedup();
    profiles
}
