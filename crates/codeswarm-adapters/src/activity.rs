//! Provider-independent tool activity. Only provider-supplied facts belong here.
use crate::ToolStatus;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ToolActivity {
    pub name: String,
    pub target: Option<String>,
    pub outcome: Option<String>,
    pub arguments: Option<Value>,
    pub output: Option<Value>,
    pub error: Option<Value>,
    #[serde(default)]
    pub locations: Vec<String>,
    #[serde(default)]
    pub diffs: Vec<FileDiff>,
    pub elapsed_seconds: Option<u64>,
    pub exit_code: Option<i64>,
    pub subagents: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileDiff {
    pub path: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

fn display(value: &Value) -> String {
    if let Some(array) = value.as_array() {
        return array.iter().map(display).collect::<Vec<_>>().join("\n");
    }
    if value.get("type").and_then(Value::as_str) == Some("content")
        && let Some(content) = value.get("content")
    {
        return display(content);
    }
    if value.get("type").and_then(Value::as_str) == Some("text")
        && let Some(text) = value.get("text").and_then(Value::as_str)
    {
        return text.into();
    }
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| serde_json::to_string_pretty(value).unwrap_or_default())
}

impl ToolActivity {
    fn arg(&self, keys: &[&str]) -> Option<String> {
        let args = self.arguments.as_ref()?;
        keys.iter().find_map(|key| args.get(*key)).map(display)
    }

    pub fn summary(&self, status: ToolStatus, elapsed: Option<u64>) -> String {
        let name = self.name.to_ascii_lowercase().replace(['_', ' '], "");
        let (verb, keys): (&str, &[&str]) = match name.as_str() {
            "read" | "viewfile" | "readfile" => ("Reading", &["file_path", "AbsolutePath", "path"]),
            "bash" | "runcommand" | "commandexecution" | "execute" => {
                ("Running", &["command", "CommandLine"])
            }
            "edit"
            | "write"
            | "filechange"
            | "replacefilecontent"
            | "multireplacefilecontent"
            | "writetofile" => ("Editing", &["file_path", "TargetFile", "path"]),
            "grep" | "glob" | "grepsearch" | "findbyname" | "search" | "websearch" => {
                ("Searching", &["pattern", "Query", "query", "Pattern"])
            }
            "task" | "agent" | "invokesubagent" | "collabtoolcall" => (
                "Delegating",
                &["description", "subagent_type", "agent", "role"],
            ),
            _ => (
                self.name.as_str(),
                &["path", "file_path", "url", "Url", "description"],
            ),
        };
        let target = self
            .arg(keys)
            .or_else(|| (!self.locations.is_empty()).then(|| self.locations.join(", ")))
            .or_else(|| {
                (!self.diffs.is_empty()).then(|| {
                    self.diffs
                        .iter()
                        .map(|d| d.path.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
            })
            .or_else(|| self.target.clone())
            .unwrap_or_default();
        let verb = if verb.is_empty() { "Tool" } else { verb };
        let mut text = format!("{verb} {target}").trim().to_owned();
        if !self.diffs.is_empty() {
            let (added, removed) = self
                .diffs
                .iter()
                .map(FileDiff::counts)
                .fold((0, 0), |(a, r), (next_a, next_r)| (a + next_a, r + next_r));
            text.push_str(&format!(" · +{added} −{removed}"));
        }
        if verb == "Reading"
            && let Some(start) = self.arg(&["StartLine", "offset"])
        {
            text.push_str(&format!(" · line {start}"));
            if let Some(end) = self.arg(&["EndLine"]) {
                text.push_str(&format!("-{end}"));
            }
        }
        if verb == "Searching"
            && let Some(scope) = self.arg(&["path", "SearchPath", "SearchDirectory"])
        {
            text.push_str(&format!(" in {scope}"));
        }
        let outcome = match status {
            ToolStatus::Pending => "pending",
            ToolStatus::Running => "running",
            ToolStatus::Completed => "done",
            ToolStatus::Failed => "failed",
        };
        text.push_str(&format!(
            " · {}",
            self.outcome.as_deref().unwrap_or(outcome)
        ));
        if let Some(code) = self.exit_code {
            text.push_str(&format!(" · exit {code}"));
        }
        if let Some(seconds) = elapsed.into_iter().chain(self.elapsed_seconds).max() {
            text.push_str(&format!(" · {seconds}s"));
        }
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    pub fn details(&self) -> String {
        let mut parts = Vec::new();
        if let Some(args) = &self.arguments {
            parts.push(format!("Arguments\n{}", display(args)));
        }
        if !self.locations.is_empty() {
            parts.push(format!("Files\n{}", self.locations.join("\n")));
        }
        for diff in &self.diffs {
            if diff.before.is_none() {
                parts.push(format!(
                    "{}: previous contents not supplied by agent.",
                    diff.path
                ));
            }
            parts.push(diff.patch());
        }
        if let Some(output) = &self.output {
            parts.push(format!("Output\n{}", display(output)));
        } else if matches!(
            self.name.to_ascii_lowercase().replace('_', "").as_str(),
            "read" | "viewfile" | "readfile"
        ) {
            parts.push("File contents not supplied by agent.".into());
        }
        if let Some(error) = &self.error {
            parts.push(format!("Error\n{}", display(error)));
        }
        if let Some(subagents) = &self.subagents {
            parts.push(format!("Subagents\n{}", display(subagents)));
        }
        parts.join("\n\n")
    }

    pub fn capture_edit(&mut self) {
        let path = self.arg(&["file_path", "TargetFile", "path"]);
        let before = self.arg(&["old_string", "TargetContent"]);
        let after = self.arg(&["new_string", "ReplacementContent", "content", "CodeContent"]);
        if let Some(path) = path
            && (before.is_some() || after.is_some())
        {
            self.diffs = vec![FileDiff {
                path,
                before,
                after,
            }];
        }
        if let Some(chunks) = self
            .arguments
            .as_ref()
            .and_then(|a| a.get("ReplacementChunks"))
            .and_then(Value::as_array)
        {
            let path = self.arg(&["TargetFile"]).unwrap_or_else(|| "file".into());
            self.diffs = chunks
                .iter()
                .map(|chunk| FileDiff {
                    path: path.clone(),
                    before: chunk
                        .get("TargetContent")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    after: chunk
                        .get("ReplacementContent")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                })
                .collect();
        }
    }
}

impl FileDiff {
    fn counts(&self) -> (usize, usize) {
        let old = self
            .before
            .as_deref()
            .unwrap_or("")
            .lines()
            .collect::<Vec<_>>();
        let new = self
            .after
            .as_deref()
            .unwrap_or("")
            .lines()
            .collect::<Vec<_>>();
        let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(new[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        (new.len() - prefix - suffix, old.len() - prefix - suffix)
    }

    fn patch(&self) -> String {
        let old = self
            .before
            .as_deref()
            .unwrap_or("")
            .lines()
            .collect::<Vec<_>>();
        let new = self
            .after
            .as_deref()
            .unwrap_or("")
            .lines()
            .collect::<Vec<_>>();
        let mut lines = vec![
            format!("--- {}", self.path),
            format!("+++ {}", self.path),
            format!(
                "@@ -{},{} +{},{} @@",
                usize::from(!old.is_empty()),
                old.len(),
                usize::from(!new.is_empty()),
                new.len()
            ),
        ];
        let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(new[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        lines.extend(old[..prefix].iter().map(|line| format!(" {line}")));
        lines.extend(
            old[prefix..old.len() - suffix]
                .iter()
                .map(|line| format!("-{line}")),
        );
        lines.extend(
            new[prefix..new.len() - suffix]
                .iter()
                .map(|line| format!("+{line}")),
        );
        lines.extend(
            old[old.len() - suffix..]
                .iter()
                .map(|line| format!(" {line}")),
        );
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn edits_keep_before_after_and_reads_do_not_invent_contents() {
        let mut edit = ToolActivity {
            name: "Edit".into(),
            arguments: Some(json!({
                "file_path":"src/auth.rs", "old_string":"first\nold\nlast", "new_string":"first\nnew\nlast"
            })),
            ..Default::default()
        };
        edit.capture_edit();
        assert!(edit.summary(ToolStatus::Completed, None).contains("+1 −1"));
        let detail = edit.details();
        assert!(detail.contains("--- src/auth.rs\n+++ src/auth.rs"));
        assert!(detail.contains("\n-old\n+new\n last"));
        let read = ToolActivity {
            name: "Read".into(),
            arguments: Some(json!({"file_path":"missing.rs"})),
            ..Default::default()
        };
        assert!(
            read.details()
                .contains("File contents not supplied by agent.")
        );
        let legacy: crate::ToolUpdate = serde_json::from_value(json!({
            "id":"old", "title":"Read", "status":"Completed", "detail":"retained"
        }))
        .unwrap();
        assert!(legacy.activity.is_none());
    }
}
