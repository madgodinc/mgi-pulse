//! TOML config for the headless modes. Every section is optional; the
//! command line can supply the same sources and rules.
//!
//! ```toml
//! collapse = "10m"
//! output = "text"
//!
//! [[source]]
//! name = "api"
//! path = "/var/log/api.ndjson"
//!
//! [[source]]
//! name = "web"
//! journal = ["nginx.service"]
//!
//! [[rule]]
//! name = "errors"
//! query = "level=error"
//! ```

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::sources::{SourceKind, SourceSpec};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub collapse: Option<String>,
    pub output: Option<String>,
    #[serde(default)]
    pub source: Vec<SourceConf>,
    #[serde(default)]
    pub rule: Vec<RuleConf>,
    pub telegram: Option<TelegramConf>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramConf {
    /// Environment variable holding the bot token. Default
    /// `MGI_PULSE_TELEGRAM_TOKEN`.
    pub token_env: Option<String>,
    /// File holding the bot token (first line).
    pub token_file: Option<PathBuf>,
    /// Accepted only to refuse it with a pointer to the safe options.
    pub token: Option<String>,
    /// Chats that receive watch alerts.
    #[serde(default)]
    pub alert_chats: Vec<i64>,
    /// Users allowed to send commands, in any chat with the bot.
    #[serde(default)]
    pub allowed_users: Vec<i64>,
    /// Chats where every member may send commands.
    #[serde(default)]
    pub allowed_chats: Vec<i64>,
    /// Alert messages per chat per minute. Default 20.
    pub max_per_minute: Option<u32>,
    /// Answer commands. Default true when an allowlist is set.
    pub commands: Option<bool>,
    /// Bot API base URL, for a self-hosted Bot API server.
    pub api_base: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConf {
    pub name: Option<String>,
    pub path: Option<PathBuf>,
    /// Units to read; an empty list reads the whole journal.
    pub journal: Option<Vec<String>>,
    pub format: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConf {
    pub name: String,
    pub query: String,
    /// Send matches of this rule to alert channels. Default true.
    #[serde(default = "yes")]
    pub alert: bool,
}

fn yes() -> bool {
    true
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("config {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Config> {
        Ok(toml::from_str(text)?)
    }

    /// Relative paths are resolved against the config file's directory,
    /// so a config works no matter where it is started from.
    pub fn source_specs(&self, base: Option<&Path>) -> Result<Vec<SourceSpec>> {
        let mut out = Vec::new();
        for (i, s) in self.source.iter().enumerate() {
            let format = s
                .format
                .as_deref()
                .map(crate::ingest::parse_format)
                .transpose()?;
            let (kind, default_name) = match (&s.path, &s.journal) {
                (Some(p), None) => {
                    let p = match base {
                        Some(b) if p.is_relative() => b.join(p),
                        _ => p.clone(),
                    };
                    let n = p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| format!("source{}", i + 1));
                    (SourceKind::File(p), n)
                }
                (None, Some(units)) => (SourceKind::Journal(units.clone()), "journal".to_string()),
                _ => bail!(
                    "[[source]] #{} needs exactly one of `path` or `journal`",
                    i + 1
                ),
            };
            out.push(SourceSpec {
                name: s.name.clone().unwrap_or(default_name),
                kind,
                format,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sources_and_rules() {
        let c = Config::parse(
            r#"
            collapse = "5m"
            [[source]]
            name = "api"
            path = "logs/api.ndjson"
            [[source]]
            journal = ["nginx.service"]
            [[rule]]
            name = "errors"
            query = "level=error"
            [[rule]]
            name = "slow"
            query = "msg~/slow/"
            alert = false
            "#,
        )
        .unwrap();
        assert_eq!(c.collapse.as_deref(), Some("5m"));
        let specs = c.source_specs(Some(Path::new("/etc/pulse"))).unwrap();
        assert_eq!(specs[0].name, "api");
        assert_eq!(
            specs[0].kind,
            SourceKind::File(Path::new("/etc/pulse").join("logs/api.ndjson"))
        );
        assert_eq!(specs[1].name, "journal");
        assert!(c.rule[0].alert);
        assert!(!c.rule[1].alert);
    }

    #[test]
    fn source_needs_one_kind() {
        let c = Config::parse("[[source]]\nname = \"x\"\n").unwrap();
        assert!(c.source_specs(None).is_err());
    }

    #[test]
    fn unknown_keys_are_errors() {
        assert!(Config::parse("colapse = \"5m\"\n").is_err());
    }
}
