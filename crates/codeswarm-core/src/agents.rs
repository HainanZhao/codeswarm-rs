//! Agent catalog and user configuration.
//!
//! The catalog is deliberately small and data-only.  Adapters remain free to
//! implement their own protocol, while the launcher can discover and restore
//! both built-in and user-defined commands without depending on the TUI.

use serde::{Deserialize, Serialize};

/// The adapter implementation used to launch an agent.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AdapterKind {
    #[serde(rename = "agy", alias = "native")]
    Native,
    #[serde(rename = "acp")]
    Acp,
}

/// A launchable agent entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentDefinition {
    pub identity: String,
    pub name: String,
    pub short_name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub adapter: AdapterKind,
    #[serde(alias = "run_command")]
    pub command: String,
    #[serde(default)]
    pub detect_command: Option<String>,
    /// Optional argument appended when launching a native adapter in its
    /// default full-access mode.  This mirrors the Python catalog field while
    /// keeping custom adapters free to omit it.
    #[serde(default)]
    pub full_access_startup_argument: Option<String>,
    #[serde(default = "default_active")]
    pub active: bool,
}

fn default_active() -> bool {
    true
}

impl AgentDefinition {
    pub fn new(
        identity: impl Into<String>,
        name: impl Into<String>,
        short_name: impl Into<String>,
        adapter: AdapterKind,
        command: impl Into<String>,
    ) -> Self {
        let identity = identity.into();
        let command = command.into();
        Self {
            name: name.into(),
            short_name: short_name.into(),
            aliases: Vec::new(),
            adapter,
            detect_command: Some(command.clone()),
            full_access_startup_argument: None,
            active: true,
            identity,
            command,
        }
    }
}

/// The built-in catalog shipped with CodeSwarm.
pub fn default_catalog() -> Vec<AgentDefinition> {
    #[allow(clippy::too_many_arguments)]
    fn builtin(
        identity: &str,
        name: &str,
        short_name: &str,
        adapter: AdapterKind,
        command: &str,
        detect_command: &str,
        aliases: &[&str],
        full_access_startup_argument: Option<&str>,
    ) -> AgentDefinition {
        let mut agent = AgentDefinition::new(identity, name, short_name, adapter, command);
        agent.detect_command = Some(detect_command.into());
        agent.aliases = aliases.iter().map(|alias| (*alias).into()).collect();
        agent.full_access_startup_argument = full_access_startup_argument.map(str::to_owned);
        agent
    }

    vec![
        builtin(
            "antigravity.google.com",
            "Antigravity",
            "antigravity",
            AdapterKind::Native,
            "agy",
            "agy",
            &["agy"],
            Some("--dangerously-skip-permissions"),
        ),
        builtin(
            "claude.com",
            "Claude",
            "claude",
            AdapterKind::Acp,
            "npx -y @agentclientprotocol/claude-agent-acp",
            "claude",
            &[],
            None,
        ),
        builtin(
            "geminicli.com",
            "Gemini",
            "gemini",
            AdapterKind::Acp,
            "gemini --experimental-acp",
            "gemini",
            &[],
            None,
        ),
        builtin(
            "openai.com",
            "Codex",
            "codex",
            AdapterKind::Acp,
            "npx -y --package=@agentclientprotocol/codex-acp codex-acp",
            "codex",
            &["openai"],
            None,
        ),
        builtin(
            "opencode.ai",
            "OpenCode",
            "opencode",
            AdapterKind::Acp,
            "opencode acp",
            "opencode",
            &[],
            None,
        ),
        builtin(
            "qwen.ai",
            "Qwen",
            "qwen",
            AdapterKind::Acp,
            "qwen --acp",
            "qwen",
            &[],
            None,
        ),
    ]
}

#[derive(Deserialize)]
struct SettingsFile {
    agents: Option<serde_json::Value>,
}

/// Load built-ins plus valid user entries from a settings document.
///
/// User entries replace a built-in with the same identity (case-insensitive),
/// and may add custom identities. Invalid entries are ignored so a typo in a
/// config file cannot prevent the store from opening. Set `active` to false
/// to hide a built-in or custom entry from the launcher.
pub fn catalog_from_settings(settings_json: &str) -> Vec<AgentDefinition> {
    let mut catalog = default_catalog();
    let Ok(settings) = serde_json::from_str::<SettingsFile>(settings_json) else {
        return catalog;
    };
    let Some(value) = settings.agents else {
        return catalog;
    };
    let entries = match value {
        serde_json::Value::Array(entries) => entries,
        serde_json::Value::Object(entries) => entries
            .into_iter()
            .filter_map(|(identity, mut value)| {
                let object = value.as_object_mut()?;
                object
                    .entry("identity".to_owned())
                    .or_insert(serde_json::Value::String(identity));
                Some(value)
            })
            .collect(),
        _ => return catalog,
    };
    for value in entries {
        let Ok(entry) = serde_json::from_value::<AgentDefinition>(value) else {
            continue;
        };
        if entry.identity.trim().is_empty() || entry.command.trim().is_empty() {
            continue;
        }
        if let Some(existing) = catalog
            .iter_mut()
            .find(|candidate| candidate.identity.eq_ignore_ascii_case(&entry.identity))
        {
            *existing = entry;
        } else {
            catalog.push(entry);
        }
    }
    catalog
}

/// Read a catalog from a settings file, falling back to built-ins on IO or
/// parse errors. The returned list includes inactive entries for callers that
/// need to display/modify them; use [`active_catalog`] for launch choices.
pub fn catalog_from_path(path: impl AsRef<std::path::Path>) -> Vec<AgentDefinition> {
    std::fs::read_to_string(path).map_or_else(
        |_| default_catalog(),
        |settings| catalog_from_settings(&settings),
    )
}

/// Return only entries enabled for launch.
pub fn active_catalog(catalog: impl IntoIterator<Item = AgentDefinition>) -> Vec<AgentDefinition> {
    catalog.into_iter().filter(|agent| agent.active).collect()
}

#[cfg(test)]
mod tests {
    use super::{AdapterKind, active_catalog, catalog_from_settings, default_catalog};

    #[test]
    fn builtins_cover_native_and_acp_agents() {
        let catalog = default_catalog();
        assert_eq!(catalog.len(), 6);
        assert!(
            catalog
                .iter()
                .any(|agent| agent.adapter == AdapterKind::Native)
        );
        assert!(
            catalog
                .iter()
                .any(|agent| agent.adapter == AdapterKind::Acp)
        );
    }

    #[test]
    fn custom_entries_are_added_and_builtin_entries_can_be_replaced() {
        let settings = r#"{
            "agents": [
                {"identity":"openai.com","name":"Codex local","short_name":"codex","adapter":"acp","command":"codex --acp","active":true},
                {"identity":"custom.example","name":"Custom","short_name":"custom","adapter":"native","command":"my-agent","aliases":["mine"]}
            ]
        }"#;
        let catalog = catalog_from_settings(settings);
        assert_eq!(catalog.len(), 7);
        assert_eq!(
            catalog
                .iter()
                .find(|a| a.identity == "openai.com")
                .unwrap()
                .name,
            "Codex local"
        );
        assert!(catalog.iter().any(|a| a.identity == "custom.example"));
    }

    #[test]
    fn object_form_uses_the_map_key_as_identity() {
        let settings = r#"{"agents":{"mine.example":{"name":"Mine","short_name":"mine","adapter":"acp","command":"mine --acp"}}}"#;
        let catalog = catalog_from_settings(settings);
        assert_eq!(
            catalog
                .iter()
                .find(|a| a.identity == "mine.example")
                .unwrap()
                .short_name,
            "mine"
        );
    }

    #[test]
    fn malformed_entries_do_not_poison_builtin_catalog_and_inactive_is_filterable() {
        let settings = r#"{"agents":[42,{"identity":"hidden","name":"Hidden","short_name":"hidden","adapter":"acp","command":"hidden","active":false}]}"#;
        let catalog = catalog_from_settings(settings);
        assert_eq!(catalog.len(), 7);
        assert!(
            !active_catalog(catalog)
                .iter()
                .any(|a| a.identity == "hidden")
        );
    }

    #[test]
    fn builtins_keep_python_aliases_and_detect_real_cli_not_npx_bridge() {
        let catalog = default_catalog();
        let antigravity = catalog
            .iter()
            .find(|agent| agent.identity == "antigravity.google.com")
            .expect("antigravity");
        assert_eq!(antigravity.aliases, ["agy"]);
        assert_eq!(antigravity.detect_command.as_deref(), Some("agy"));
        assert_eq!(
            antigravity.full_access_startup_argument.as_deref(),
            Some("--dangerously-skip-permissions")
        );

        let codex = catalog
            .iter()
            .find(|agent| agent.identity == "openai.com")
            .expect("codex");
        assert_eq!(codex.aliases, ["openai"]);
        assert_eq!(codex.detect_command.as_deref(), Some("codex"));
        assert_ne!(
            codex.detect_command.as_deref(),
            Some(codex.command.as_str())
        );
        let gemini = catalog
            .iter()
            .find(|agent| agent.identity == "geminicli.com")
            .expect("gemini");
        assert_eq!(gemini.command, "gemini --experimental-acp");
    }
}
