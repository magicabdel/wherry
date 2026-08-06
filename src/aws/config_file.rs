use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// The kind of section found in an AWS config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    /// `[profile foo]` in `~/.aws/config`, `[foo]` in `~/.aws/credentials`.
    Profile,
    /// `[sso-session foo]` in `~/.aws/config`.
    SsoSession,
    /// Anything else (`[services foo]`, ...) that we do not care about.
    Other,
}

/// A parsed section of an AWS config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub kind: SectionKind,
    pub name: String,
    pub settings: BTreeMap<String, String>,
}

impl Section {
    /// Look up a setting of this section.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.settings.get(key).map(String::as_str)
    }
}

/// Path of the shared config file, honouring `AWS_CONFIG_FILE`.
pub fn config_path() -> Option<PathBuf> {
    path_from_env("AWS_CONFIG_FILE").or_else(|| home_dir().map(|h| h.join(".aws/config")))
}

/// Path of the shared credentials file, honouring `AWS_SHARED_CREDENTIALS_FILE`.
pub fn credentials_path() -> Option<PathBuf> {
    path_from_env("AWS_SHARED_CREDENTIALS_FILE")
        .or_else(|| home_dir().map(|h| h.join(".aws/credentials")))
}

/// Sections of `~/.aws/config`. A missing or unreadable file yields nothing:
/// callers always have a sensible "nothing configured" fallback.
pub fn load_config() -> Vec<Section> {
    load(config_path(), true)
}

/// Sections of `~/.aws/credentials`.
pub fn load_credentials() -> Vec<Section> {
    load(credentials_path(), false)
}

fn load(path: Option<PathBuf>, config_style: bool) -> Vec<Section> {
    let Some(contents) = path.and_then(|p| fs::read_to_string(p).ok()) else {
        return Vec::new();
    };
    parse(&contents, config_style)
}

/// Parse an AWS config/credentials file.
///
/// `config_style` selects the header flavour: `~/.aws/config` prefixes profiles
/// with `profile ` (except `[default]`) and may declare `[sso-session name]`
/// blocks, while `~/.aws/credentials` uses bare profile names.
///
/// Nested settings (an indented block under a key with an empty value, such as
/// the `s3 =` sub-table) are skipped: we only need flat, top-level keys.
pub fn parse(contents: &str, config_style: bool) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut in_nested_block = false;

    for raw_line in contents.lines() {
        // Strip inline comments only when they follow whitespace, matching the
        // AWS parsers closely enough for our purposes.
        let line = strip_comment(raw_line);
        if line.trim().is_empty() {
            continue;
        }

        let trimmed = line.trim();

        if let Some(header) = trimmed
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            in_nested_block = false;
            if let Some(section) = new_section(header.trim(), config_style) {
                sections.push(section);
            }
            continue;
        }

        let indented = line.starts_with([' ', '\t']);
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();

        if indented && in_nested_block {
            // Sub-setting of the previous key, e.g. `s3 =\n  addressing_style = path`.
            continue;
        }
        // A key with no value opens a nested block.
        in_nested_block = value.is_empty();

        if let Some(section) = sections.last_mut() {
            if !key.is_empty() && !value.is_empty() {
                section.settings.insert(key, value.to_string());
            }
        }
    }

    sections
}

fn new_section(header: &str, config_style: bool) -> Option<Section> {
    if header.is_empty() {
        return None;
    }

    let (kind, name) = if !config_style {
        (SectionKind::Profile, header)
    } else if let Some(name) = header.strip_prefix("profile ") {
        (SectionKind::Profile, name.trim())
    } else if let Some(name) = header.strip_prefix("sso-session ") {
        (SectionKind::SsoSession, name.trim())
    } else if header == "default" {
        (SectionKind::Profile, header)
    } else {
        (SectionKind::Other, header)
    };

    if name.is_empty() {
        return None;
    }

    Some(Section {
        kind,
        name: name.to_string(),
        settings: BTreeMap::new(),
    })
}

fn strip_comment(line: &str) -> &str {
    // A `#`/`;` at the start of the line always comments it out; further in, it
    // only does when preceded by whitespace (URLs contain `#` fragments).
    let bytes = line.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'#' && *byte != b';' {
            continue;
        }
        match index.checked_sub(1).map(|i| bytes[i]) {
            None => return "",
            Some(b' ' | b'\t') => return &line[..index],
            Some(_) => continue,
        }
    }
    line
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn path_from_env(key: &str) -> Option<PathBuf> {
    let value = std::env::var_os(key)?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_profiles_and_sso_sessions() {
        let contents = "\
[profile llm]
sso_session = sia
sso_account_id = 123456789012

[sso-session sia]
sso_start_url = https://example.awsapps.com/start/#/
sso_region = eu-west-3

[default]
region = eu-west-1

[services my-services]
s3 =
  endpoint_url = http://localhost:4566
";
        let sections = parse(contents, true);
        assert_eq!(sections.len(), 4);

        assert_eq!(sections[0].kind, SectionKind::Profile);
        assert_eq!(sections[0].name, "llm");
        assert_eq!(sections[0].get("sso_session"), Some("sia"));

        assert_eq!(sections[1].kind, SectionKind::SsoSession);
        assert_eq!(sections[1].name, "sia");
        // The `#/` fragment of a start URL must survive comment stripping.
        assert_eq!(
            sections[1].get("sso_start_url"),
            Some("https://example.awsapps.com/start/#/")
        );

        assert_eq!(sections[2].kind, SectionKind::Profile);
        assert_eq!(sections[2].name, "default");

        assert_eq!(sections[3].kind, SectionKind::Other);
        // Nested keys are not hoisted into the section.
        assert!(sections[3].get("endpoint_url").is_none());
    }

    #[test]
    fn parses_credentials_style_headers() {
        let sections = parse("[foo]\naws_access_key_id = AKIA\n", false);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].kind, SectionKind::Profile);
        assert_eq!(sections[0].name, "foo");
    }

    #[test]
    fn skips_comments_and_blank_lines() {
        let sections = parse(
            "# comment\n[profile a]\n; another\nregion = eu-west-1 # trailing\n",
            true,
        );
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].get("region"), Some("eu-west-1"));
    }
}
