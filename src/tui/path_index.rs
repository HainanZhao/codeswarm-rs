//! Lazy, bounded workspace path indexing for the prompt picker.
//!
//! The index deliberately owns its worker thread and communicates with the
//! terminal loop through non-blocking channels.  A repository scan therefore
//! never shares the input/render thread's latency budget.  The implementation
//! is intentionally small: it has no filesystem watcher, no dependency on a
//! parser, and caps both traversal and search output so a large workspace
//! cannot turn a prompt key into a multi-second redraw.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
};

/// Minimum query length used by the Python path picker.
pub const MIN_PATH_QUERY_CHARS: usize = 3;
/// Maximum number of entries retained by one index.
pub const MAX_INDEX_ENTRIES: usize = 8_192;
/// Maximum number of picker rows returned for one query.
pub const MAX_PATH_RESULTS: usize = 30;
/// Traversal depth cap.  The root itself is depth zero.
pub const MAX_INDEX_DEPTH: usize = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathCandidate {
    /// Workspace-relative path, always using `/` separators.
    pub path: String,
    pub directory: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathMatch {
    pub path: String,
    pub directory: bool,
    /// Fuzzy score; larger is a stronger match.
    pub score: usize,
    /// Character offsets that matched the query.  These are useful to a
    /// renderer for underlining without rescanning the candidate.
    pub offsets: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PathIndexUpdate {
    /// The scan has completed.  A subsequent query is safe and cheap.
    Ready { generation: u64, total: usize },
    /// A query result.  `generation` prevents a result from an old workspace
    /// from replacing a newer one after a project switch.
    Matches {
        generation: u64,
        query: String,
        matches: Vec<PathMatch>,
    },
}

enum Request {
    Query { generation: u64, query: String },
    Rescan { generation: u64, root: PathBuf },
    Stop,
}

/// A background path index that is safe to poll from a synchronous TUI loop.
#[derive(Debug)]
pub struct PathIndex {
    requests: Sender<Request>,
    updates: Receiver<PathIndexUpdate>,
    worker: Option<JoinHandle<()>>,
    generation: u64,
    query: String,
}

impl PathIndex {
    /// Start indexing `root`; this function only starts a thread and returns.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let (requests, request_receiver) = mpsc::channel();
        let (update_sender, updates) = mpsc::channel();
        let root = root.into();
        let worker = thread::Builder::new()
            .name("codeswarm-path-index".into())
            .spawn(move || worker_loop(root, request_receiver, update_sender))
            .ok();
        Self {
            requests,
            updates,
            worker,
            generation: 0,
            query: String::new(),
        }
    }

    /// Request a new scan.  Existing results remain usable until the new
    /// scan finishes, which avoids a blank picker during a project switch.
    pub fn rescan(&mut self, root: impl Into<PathBuf>) {
        self.generation = self.generation.wrapping_add(1);
        let query = self.query.clone();
        let _ = self.requests.send(Request::Rescan {
            generation: self.generation,
            root: root.into(),
        });
        // A query may already have been processed before the rescan request.
        // Re-queue the current query with the new generation so a project
        // switch cannot leave the picker showing results from the old root.
        if !query.is_empty() {
            let _ = self.requests.send(Request::Query {
                generation: self.generation,
                query,
            });
        }
    }

    /// Ask the worker for a fuzzy result set.  Duplicate queries are dropped
    /// before they cross the channel, keeping fast key repeats bounded.
    pub fn query(&mut self, query: impl Into<String>) {
        let query = query.into();
        if query == self.query {
            return;
        }
        self.query = query.clone();
        let _ = self.requests.send(Request::Query {
            generation: self.generation,
            query,
        });
    }

    pub fn current_query(&self) -> &str {
        &self.query
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Drain all ready updates without waiting.  The caller should retain the
    /// latest update for display and may safely call this once per frame.
    pub fn poll(&self) -> Vec<PathIndexUpdate> {
        self.updates.try_iter().collect()
    }
}

impl Drop for PathIndex {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn worker_loop(root: PathBuf, requests: Receiver<Request>, updates: Sender<PathIndexUpdate>) {
    let mut generation = 0;
    let mut candidates = scan_workspace(&root);
    let _ = updates.send(PathIndexUpdate::Ready {
        generation,
        total: candidates.len(),
    });
    let mut pending_query: Option<String> = None;

    while let Ok(request) = requests.recv() {
        match request {
            Request::Stop => break,
            Request::Rescan {
                generation: next_generation,
                root: next_root,
            } => {
                generation = next_generation;
                candidates = scan_workspace(&next_root);
                let _ = updates.send(PathIndexUpdate::Ready {
                    generation,
                    total: candidates.len(),
                });
                if let Some(query) = pending_query.take() {
                    send_matches(&updates, generation, &query, &candidates);
                }
            }
            Request::Query {
                generation: query_generation,
                query,
            } => {
                // Keep the latest query if a rescan is currently being
                // serviced.  The generation check prevents stale data from
                // being displayed after a root switch.
                if query_generation != generation {
                    pending_query = Some(query);
                } else {
                    send_matches(&updates, generation, &query, &candidates);
                }
            }
        }
    }
}

fn send_matches(
    updates: &Sender<PathIndexUpdate>,
    generation: u64,
    query: &str,
    candidates: &[PathCandidate],
) {
    let matches = rank_matches(query, candidates);
    let _ = updates.send(PathIndexUpdate::Matches {
        generation,
        query: query.to_owned(),
        matches,
    });
}

/// Traverse a workspace without following symlinks and without descending
/// into generated/dependency trees.  Directory entries are included because
/// the picker uses them for navigation and for Python-compatible trailing `/`
/// insertion semantics.
pub fn scan_workspace(root: &Path) -> Vec<PathCandidate> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let ignore_rules = load_ignore_rules(&root);
    let mut pending = vec![(root.clone(), PathBuf::new(), 0usize)];
    let mut candidates = Vec::new();
    while let Some((directory, relative, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if candidates.len() >= MAX_INDEX_ENTRIES {
                return candidates;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let name = entry.file_name();
            if name == ".git" || name == "node_modules" || name == "target" {
                continue;
            }
            let child_relative = relative.join(&name);
            let display = child_relative.to_string_lossy().replace('\\', "/");
            let directory_entry = file_type.is_dir();
            if is_ignored(&display, directory_entry, &ignore_rules) {
                continue;
            }
            candidates.push(PathCandidate {
                path: display,
                directory: directory_entry,
            });
            if directory_entry && depth < MAX_INDEX_DEPTH {
                pending.push((entry.path(), child_relative, depth + 1));
            }
        }
    }
    candidates.sort_by(|left, right| {
        left.path
            .matches('/')
            .count()
            .cmp(&right.path.matches('/').count())
            .then_with(|| {
                left.path
                    .to_ascii_lowercase()
                    .cmp(&right.path.to_ascii_lowercase())
            })
    });
    candidates
}

#[derive(Clone, Debug)]
struct IgnoreRule {
    pattern: String,
    negated: bool,
    directory_only: bool,
    anchored: bool,
}

fn load_ignore_rules(root: &Path) -> Vec<IgnoreRule> {
    let Ok(content) = std::fs::read_to_string(root.join(".gitignore")) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let negated = line.starts_with('!');
            let line = line.strip_prefix('!').unwrap_or(line);
            let directory_only = line.ends_with('/');
            let line = line.trim_end_matches('/');
            let anchored = line.starts_with('/');
            let pattern = line.trim_start_matches('/').to_owned();
            (!pattern.is_empty()).then_some(IgnoreRule {
                pattern,
                negated,
                directory_only,
                anchored,
            })
        })
        .collect()
}

fn is_ignored(path: &str, directory: bool, rules: &[IgnoreRule]) -> bool {
    let mut ignored = false;
    for rule in rules {
        if rule.directory_only && !directory {
            continue;
        }
        let matched = if rule.pattern.contains('/') || rule.anchored {
            glob_match(&rule.pattern, path)
                || (!rule.anchored
                    && path
                        .strip_prefix("./")
                        .is_some_and(|path| glob_match(&rule.pattern, path)))
        } else {
            path.split('/')
                .any(|component| glob_match(&rule.pattern, component))
        };
        if matched {
            ignored = !rule.negated;
        }
    }
    ignored
}

/// Small shell-style glob matcher.  Git's wildmatch has additional edge
/// cases, but this covers the common `.gitignore` forms while staying tiny,
/// allocation-free, and predictable in a background index worker.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let mut states = vec![false; text.len() + 1];
    states[0] = true;
    for &character in pattern {
        let mut next = vec![false; text.len() + 1];
        match character {
            b'*' => {
                let mut seen = false;
                for index in 0..=text.len() {
                    seen |= states[index];
                    next[index] = seen;
                }
            }
            b'?' => {
                next[1..text.len() + 1].copy_from_slice(&states[..text.len()]);
            }
            literal => {
                for index in 0..text.len() {
                    next[index + 1] = states[index] && text[index].eq_ignore_ascii_case(&literal);
                }
            }
        }
        states = next;
    }
    states[text.len()]
}

pub fn rank_matches(query: &str, candidates: &[PathCandidate]) -> Vec<PathMatch> {
    let query = query.trim();
    if query.chars().count() < MIN_PATH_QUERY_CHARS {
        return Vec::new();
    }
    let query = query.trim_start_matches('@');
    if query.chars().count() < MIN_PATH_QUERY_CHARS {
        return Vec::new();
    }
    let mut matches = candidates
        .iter()
        .filter_map(|candidate| {
            fuzzy_score(query, &candidate.path).map(|(score, offsets)| PathMatch {
                path: candidate.path.clone(),
                directory: candidate.directory,
                score,
                offsets,
            })
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| {
        right.score.cmp(&left.score).then_with(|| {
            left.path
                .to_ascii_lowercase()
                .cmp(&right.path.to_ascii_lowercase())
        })
    });
    matches.truncate(MAX_PATH_RESULTS);
    matches
}

fn fuzzy_score(query: &str, candidate: &str) -> Option<(usize, Vec<usize>)> {
    let query = query.to_ascii_lowercase();
    let candidate_lower = candidate.to_ascii_lowercase();
    let candidate = candidate_lower.as_str();
    let mut cursor = 0usize;
    let mut previous = None;
    let mut score = 0usize;
    let mut offsets = Vec::with_capacity(query.chars().count());
    for character in query.chars() {
        let position = candidate[cursor..].find(character)? + cursor;
        let boundary = position == 0 || candidate.as_bytes().get(position - 1) == Some(&b'/');
        score += if previous == Some(position.saturating_sub(1)) {
            4
        } else if boundary {
            3
        } else {
            1
        };
        offsets.push(position);
        previous = Some(position);
        cursor = position + character.len_utf8();
    }
    let filename_start = candidate.rfind('/').map_or(0, |index| index + 1);
    if candidate[filename_start..].starts_with(&query) {
        score += query.chars().count() * 4;
    }
    // Shallower paths are easier to scan and more useful as a default.
    score = score.saturating_sub(candidate.matches('/').count());
    Some((score, offsets))
}

/// Quote a selected picker value in the syntax accepted by ACP resource
/// expansion.  Directories keep a trailing slash so a user can continue
/// typing inside them; files are followed by a space like the Python picker.
pub fn insertion_text(path: &str, directory: bool) -> String {
    let value = if directory {
        format!("@{}/", path.trim_end_matches('/'))
    } else if path.contains(char::is_whitespace) {
        format!("@\"{path}\"")
    } else {
        format!("@{path}")
    };
    if directory {
        value
    } else {
        format!("{value} ")
    }
}

/// Return a set of duplicate-free path strings, useful when an embedding
/// wants to replace its static completion candidates after the scan.
pub fn completion_values(candidates: &[PathCandidate]) -> Vec<String> {
    let mut seen = HashSet::new();
    candidates
        .iter()
        .filter_map(|candidate| {
            let value = format!("@{}", candidate.path);
            seen.insert(value.clone()).then_some(value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_PATH_RESULTS, MIN_PATH_QUERY_CHARS, PathCandidate, PathIndexUpdate, completion_values,
        insertion_text, rank_matches, scan_workspace,
    };

    #[test]
    fn bounded_scan_includes_directories_and_skips_generated_trees() {
        let root = std::env::temp_dir().join(format!(
            "codeswarm-path-index-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("src/nested")).expect("dirs");
        std::fs::create_dir_all(root.join("target")).expect("target");
        std::fs::write(root.join("src/main.rs"), "fn main() {}").expect("file");
        std::fs::write(root.join("target/ignored.rs"), "ignored").expect("ignored");
        std::fs::write(root.join(".gitignore"), "*.log\nignored-dir/\n").expect("ignore");
        std::fs::create_dir_all(root.join("ignored-dir")).expect("ignored dir");
        std::fs::write(root.join("trace.log"), "ignored").expect("ignored log");
        let paths = scan_workspace(&root);
        assert!(
            paths
                .iter()
                .any(|path| path.path == "src" && path.directory)
        );
        assert!(paths.iter().any(|path| path.path == "src/main.rs"));
        assert!(!paths.iter().any(|path| path.path.starts_with("target")));
        assert!(
            !paths
                .iter()
                .any(|path| path.path.starts_with("ignored-dir"))
        );
        assert!(!paths.iter().any(|path| path.path == "trace.log"));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn fuzzy_matches_rank_filename_and_cap_results() {
        let candidates = vec![
            PathCandidate {
                path: "src/main.rs".into(),
                directory: false,
            },
            PathCandidate {
                path: "docs/readme.md".into(),
                directory: false,
            },
        ];
        let matches = rank_matches("@main", &candidates);
        assert_eq!(matches[0].path, "src/main.rs");
        assert!(!matches[0].offsets.is_empty());
        assert!(rank_matches("@ma", &candidates).is_empty());
        let many = (0..(MAX_PATH_RESULTS + 5))
            .map(|index| PathCandidate {
                path: format!("src/main{index}.rs"),
                directory: false,
            })
            .collect::<Vec<_>>();
        assert_eq!(rank_matches("main", &many).len(), MAX_PATH_RESULTS);
        assert_eq!(MIN_PATH_QUERY_CHARS, 3);
    }

    #[test]
    fn insertion_quotes_spaces_and_keeps_directories_open() {
        assert_eq!(insertion_text("src/main.rs", false), "@src/main.rs ");
        assert_eq!(
            insertion_text("notes/project plan.md", false),
            "@\"notes/project plan.md\" "
        );
        assert_eq!(insertion_text("src", true), "@src/");
    }

    #[test]
    fn completion_values_are_prefixed_and_deduplicated() {
        let values = completion_values(&[
            PathCandidate {
                path: "README.md".into(),
                directory: false,
            },
            PathCandidate {
                path: "README.md".into(),
                directory: false,
            },
        ]);
        assert_eq!(values, ["@README.md"]);
    }

    #[test]
    fn path_index_can_scan_without_blocking_the_caller() {
        let root = std::env::temp_dir().join(format!(
            "codeswarm-path-index-worker-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("root");
        std::fs::write(root.join("main.rs"), "main").expect("file");
        let mut index = super::PathIndex::new(root.clone());
        index.query("main");
        let mut saw_matches = false;
        for _ in 0..100 {
            for update in index.poll() {
                if let PathIndexUpdate::Matches { matches, .. } = update
                    && matches.iter().any(|value| value.path == "main.rs")
                {
                    saw_matches = true;
                }
            }
            if saw_matches {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(saw_matches);
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
