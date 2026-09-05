//! User-owned session goals, independent of provider sessions and relay limits.
use serde::{Deserialize, Serialize};

pub const GOAL_USAGE: &str = "/goal [OBJECTIVE | run | done | clear]";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Completed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Goal {
    pub objective: String,
    pub status: GoalStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalCommand {
    Show,
    Set(String),
    Run,
    Done,
    Clear,
}

impl GoalCommand {
    pub fn parse(argument: &str) -> Result<Self, String> {
        let text = argument.trim();
        let mut parts = text.split_whitespace();
        let Some(first) = parts.next() else {
            return Ok(Self::Show);
        };
        let reserved = match first.to_ascii_lowercase().as_str() {
            "run" => Some(Self::Run),
            "done" => Some(Self::Done),
            "clear" => Some(Self::Clear),
            _ => None,
        };
        if let Some(command) = reserved {
            return if parts.next().is_none() {
                Ok(command)
            } else {
                Err(format!("usage: {GOAL_USAGE}"))
            };
        }
        validate_objective(text)?;
        Ok(Self::Set(text.into()))
    }
}

fn validate_objective(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("goal objective must not be empty".into());
    }
    if text.len() > 16_000 {
        return Err("goal objective must be at most 16,000 bytes".into());
    }
    Ok(())
}

impl Goal {
    pub fn from_metadata(value: &serde_json::Value) -> Option<Self> {
        let goal: Self = serde_json::from_value(value.clone()).ok()?;
        validate_objective(&goal.objective).ok()?;
        Some(goal)
    }
    pub fn summary(&self) -> String {
        format!(
            "Goal {}: {}",
            match self.status {
                GoalStatus::Active => "active",
                GoalStatus::Completed => "completed",
            },
            self.objective
        )
    }
}

/// Returns a human task to dispatch only for setting or resuming a goal.
pub fn apply(goal: &mut Option<Goal>, command: GoalCommand) -> Result<Option<String>, String> {
    match command {
        GoalCommand::Set(objective) => {
            validate_objective(&objective)?;
            let task = format!("Work toward this goal: {}", objective.trim());
            *goal = Some(Goal {
                objective: objective.trim().into(),
                status: GoalStatus::Active,
            });
            Ok(Some(task))
        }
        GoalCommand::Run => match goal {
            Some(goal) if goal.status == GoalStatus::Active => Ok(Some(format!(
                "Continue working toward this goal: {}",
                goal.objective
            ))),
            _ => Err("no active goal; use /goal OBJECTIVE to start one".into()),
        },
        GoalCommand::Done => {
            let goal = goal.as_mut().ok_or("no goal to complete")?;
            goal.status = GoalStatus::Completed;
            Ok(None)
        }
        GoalCommand::Clear => {
            *goal = None;
            Ok(None)
        }
        GoalCommand::Show => Ok(None),
    }
}

pub fn prompt(goal: Option<&Goal>, task: &str) -> String {
    let context = match goal {
        Some(goal) if goal.status == GoalStatus::Active => format!(
            "Active shared goal: {}\nWork toward this objective. The current user request takes priority. Report progress and remaining work honestly; completion is tracked by the user with /goal done. Respect permissions and relay limits.",
            goal.objective
        ),
        _ => "No active shared goal. Follow the current user request.".into(),
    };
    format!("{task}\n\n[CodeSwarm goal context]\n{context}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn goal_lifecycle_and_invalid_metadata_are_explicit() {
        let mut goal = None;
        assert!(apply(&mut goal, GoalCommand::Run).is_err());
        assert!(apply(&mut goal, GoalCommand::Done).is_err());
        assert_eq!(GoalCommand::parse("").unwrap(), GoalCommand::Show);
        assert!(GoalCommand::parse("run extra").is_err());
        assert!(GoalCommand::parse(&"x".repeat(16_001)).is_err());
        let objective = "Fix login\n  preserve existing sessions";
        let action = GoalCommand::parse(objective).unwrap();
        assert!(
            apply(&mut goal, action)
                .unwrap()
                .unwrap()
                .contains(objective)
        );
        let encoded = serde_json::to_value(&goal).unwrap();
        assert_eq!(Goal::from_metadata(&encoded), goal);
        assert!(prompt(goal.as_ref(), "review").contains(objective));
        apply(&mut goal, GoalCommand::Done).unwrap();
        assert!(apply(&mut goal, GoalCommand::Run).is_err());
        assert!(!prompt(goal.as_ref(), "new request").contains(objective));
        apply(&mut goal, GoalCommand::Clear).unwrap();
        assert!(goal.is_none());
        for value in [
            serde_json::json!(null),
            serde_json::json!({"objective":"", "status":"active"}),
            serde_json::json!({"objective":"task", "status":"unknown"}),
            serde_json::json!({"objective":42,"status":"active"}),
        ] {
            assert!(Goal::from_metadata(&value).is_none());
        }
    }
}
