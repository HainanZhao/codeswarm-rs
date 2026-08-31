//! Adapter-independent permission policy resolution.

use crate::Mode;

pub const DEFAULT_POLICY_ID: &str = "codeswarm:mode:full-access";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModePolicy {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    aliases: &'static [&'static str],
}

impl ModePolicy {
    pub fn resolve(&self, modes: &[Mode]) -> Option<Mode> {
        modes.iter().find_map(|mode| {
            let id = normalize(&mode.id);
            let label = normalize(&mode.label);
            self.aliases
                .iter()
                .any(|alias| *alias == id || *alias == label)
                .then(|| mode.clone())
        })
    }

    pub fn display_mode(&self) -> Mode {
        Mode {
            id: self.id.into(),
            label: self.name.into(),
        }
    }
}

pub const POLICIES: &[ModePolicy] = &[
    ModePolicy {
        id: "codeswarm:mode:plan",
        name: "Plan",
        description: "Read-only planning with no tool execution",
        aliases: &["plan", "planmode", "readonly"],
    },
    ModePolicy {
        id: "codeswarm:mode:manual",
        name: "Manual",
        description: "Ask before operations that require permission",
        aliases: &["default", "manual", "ask", "agymanual"],
    },
    ModePolicy {
        id: "codeswarm:mode:accept-edits",
        name: "Accept Edits",
        description: "Automatically approve file edits, but keep safeguards",
        aliases: &["acceptedits", "autoedit", "autoapproveedits"],
    },
    ModePolicy {
        id: DEFAULT_POLICY_ID,
        name: "Auto pilot",
        description: "Automatically approve tools and bypass prompts",
        aliases: &[
            "fullaccess",
            "yolo",
            "bypasspermissions",
            "skippermissions",
            "codeswarmstartupfullaccess",
            "agyfullaccess",
        ],
    },
];

pub fn resolve(policy_id: &str, modes: &[Mode]) -> Option<Mode> {
    POLICIES
        .iter()
        .find(|policy| policy.id == policy_id)
        .and_then(|policy| policy.resolve(modes))
}

/// Policies available across every active adapter. Mixed is deliberately not a
/// display mode: callers either receive a shared policy or no selection.
pub fn shared_modes(mode_sets: &[Vec<Mode>]) -> Vec<Mode> {
    if mode_sets.is_empty() {
        return Vec::new();
    }
    POLICIES
        .iter()
        .filter(|policy| {
            mode_sets
                .iter()
                .all(|modes| policy.resolve(modes).is_some())
        })
        .map(ModePolicy::display_mode)
        .collect()
}

pub fn shared_current_mode(states: &[(Vec<Mode>, Option<String>)]) -> Option<Mode> {
    POLICIES.iter().find_map(|policy| {
        states
            .iter()
            .all(|(modes, current)| {
                current.as_ref().is_some_and(|current| {
                    policy
                        .resolve(modes)
                        .is_some_and(|native| native.id == *current)
                })
            })
            .then(|| policy.display_mode())
    })
}

fn normalize(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::Mode;

    use super::{DEFAULT_POLICY_ID, shared_current_mode, shared_modes};

    #[test]
    fn maps_different_native_names_to_one_policy() {
        let sets = vec![
            vec![Mode {
                id: "yolo".into(),
                label: "YOLO".into(),
            }],
            vec![Mode {
                id: "full-access".into(),
                label: "Auto pilot".into(),
            }],
        ];
        let modes = shared_modes(&sets);
        assert!(modes.iter().any(|mode| mode.id == DEFAULT_POLICY_ID));
    }

    #[test]
    fn maps_codex_full_access_catalog_to_auto_pilot() {
        let modes = vec![
            Mode {
                id: "read-only".into(),
                label: "Ask for approval".into(),
            },
            Mode {
                id: "agent".into(),
                label: "Approve for me".into(),
            },
            Mode {
                id: "agent-full-access".into(),
                label: "Full access".into(),
            },
        ];
        assert_eq!(
            super::resolve(DEFAULT_POLICY_ID, &modes).map(|mode| mode.id),
            Some("agent-full-access".into())
        );
    }

    #[test]
    fn mixed_native_state_is_not_a_user_visible_mode() {
        let states = vec![
            (
                vec![Mode {
                    id: "plan".into(),
                    label: "Plan".into(),
                }],
                Some("plan".into()),
            ),
            (
                vec![Mode {
                    id: "default".into(),
                    label: "Manual".into(),
                }],
                Some("default".into()),
            ),
        ];
        assert_eq!(shared_current_mode(&states), None);
    }
}
