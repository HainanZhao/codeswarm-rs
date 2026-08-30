//! Startup configuration and saved-roster restoration.
//!
//! The launcher runs before a session or adapter exists.  This module keeps
//! that decision independent from the terminal UI: a saved roster is only a
//! request to restore agents that still exist in the current catalog.  The
//! catalog is authoritative, so stale identities are discarded and an empty
//! result always opens the store instead of auto-starting detected agents.

use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct SettingsFile {
    launcher: Option<LauncherSettings>,
}

#[derive(Debug, Deserialize)]
struct LauncherSettings {
    roster: Option<String>,
}

/// The action the bare launcher should take after reading persisted state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaunchDecision {
    /// Restore this roster in its persisted order.
    Restore { identities: Vec<String> },
    /// Show the agent store so the user can choose a roster.
    OpenStore,
}

impl LaunchDecision {
    /// Whether this decision contains a usable saved roster.
    pub fn should_restore(&self) -> bool {
        matches!(self, Self::Restore { identities } if !identities.is_empty())
    }

    /// Return the identities to restore, or an empty slice for the store.
    pub fn identities(&self) -> &[String] {
        match self {
            Self::Restore { identities } => identities,
            Self::OpenStore => &[],
        }
    }
}

/// Parse `launcher.roster` from a persisted CodeSwarm settings document.
///
/// Each non-empty line is one identity.  Parsing failures, missing settings,
/// and values of the wrong type are treated as an empty saved roster; startup
/// must remain safe when a user has a truncated or hand-edited settings file.
pub fn parse_saved_roster(settings_json: &str) -> Vec<String> {
    let Ok(settings) = serde_json::from_str::<SettingsFile>(settings_json) else {
        return Vec::new();
    };
    settings
        .launcher
        .and_then(|launcher| launcher.roster)
        .map(|roster| {
            roster
                .lines()
                .map(str::trim)
                .filter(|identity| !identity.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Read and parse a persisted settings file.
///
/// A missing or unreadable file has the same safe startup behavior as a
/// malformed file: no roster is restored.  The launcher can then open the
/// store without guessing which detected agent should be started.
pub fn read_saved_roster(path: impl AsRef<Path>) -> Vec<String> {
    std::fs::read_to_string(path)
        .ok()
        .map_or_else(Vec::new, |settings| parse_saved_roster(&settings))
}

/// Resolve a saved roster against the currently known canonical identities.
///
/// Resolution is case-insensitive and returns the catalog's spelling.
/// Persisted order is retained, including
/// repeated identities; the launcher does not silently reorder a user's
/// roster.  Unknown or removed identities are filtered out.
pub fn resolve_saved_roster(settings_json: &str, available_identities: &[String]) -> Vec<String> {
    parse_saved_roster(settings_json)
        .into_iter()
        .filter_map(|saved| {
            available_identities
                .iter()
                .find(|available| available.eq_ignore_ascii_case(&saved))
                .cloned()
        })
        .collect()
}

/// Decide whether bare launch restores the saved roster or opens the store.
///
/// This intentionally never falls back to preferred/detected agents.  Agent
/// detection may preselect entries once the store is visible, but it must not
/// turn a missing or stale saved roster into an unexpected session.
pub fn launch_decision(settings_json: &str, available_identities: &[String]) -> LaunchDecision {
    let identities = resolve_saved_roster(settings_json, available_identities);
    if identities.is_empty() {
        LaunchDecision::OpenStore
    } else {
        LaunchDecision::Restore { identities }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        LaunchDecision, launch_decision, parse_saved_roster, read_saved_roster,
        resolve_saved_roster,
    };

    fn catalog() -> Vec<String> {
        vec![
            "claude.ai".into(),
            "openai.com".into(),
            "gemini.google.com".into(),
        ]
    }

    #[test]
    fn parses_multiline_roster_and_ignores_blank_lines() {
        let settings = r#"{"launcher":{"roster":" claude.ai\n\nopenai.com \n"}}"#;
        assert_eq!(parse_saved_roster(settings), ["claude.ai", "openai.com"]);
    }

    #[test]
    fn malformed_or_wrongly_shaped_settings_are_empty() {
        assert!(parse_saved_roster("not json").is_empty());
        assert!(parse_saved_roster(r#"{"launcher":{"roster":42}}"#).is_empty());
        assert!(parse_saved_roster(r#"{"launcher":[]}"#).is_empty());
        assert!(parse_saved_roster(r#"{"other":{"roster":"claude.ai"}}"#).is_empty());
    }

    #[test]
    fn filters_removed_identities_and_preserves_saved_order() {
        let settings = r#"{"launcher":{"roster":"OPENAI.COM\nremoved.ai\nclaude.ai"}}"#;
        assert_eq!(
            resolve_saved_roster(settings, &catalog()),
            ["openai.com", "claude.ai"]
        );
    }

    #[test]
    fn empty_or_fully_stale_roster_opens_store() {
        let available = catalog();
        assert_eq!(launch_decision("{}", &available), LaunchDecision::OpenStore);
        assert_eq!(
            launch_decision(r#"{"launcher":{"roster":"gone.ai"}}"#, &available),
            LaunchDecision::OpenStore
        );
    }

    #[test]
    fn partial_roster_restores_only_current_identities() {
        let available = catalog();
        let decision = launch_decision(
            r#"{"launcher":{"roster":"gone.ai\nclaude.ai\nopenai.com"}}"#,
            &available,
        );
        assert_eq!(
            decision,
            LaunchDecision::Restore {
                identities: vec!["claude.ai".into(), "openai.com".into()]
            }
        );
        assert!(decision.should_restore());
        assert_eq!(decision.identities(), ["claude.ai", "openai.com"]);
    }

    #[test]
    fn reads_saved_roster_from_disk() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("codeswarm-launcher-{unique}.json"));
        std::fs::write(&path, r#"{"launcher":{"roster":"claude.ai"}}"#).expect("write");
        assert_eq!(read_saved_roster(&path), ["claude.ai"]);
        std::fs::remove_file(path).expect("cleanup");
    }
}
