//! Local, multi-session transcript archive.
//!
//! Sessions live under an explicit archive root that callers must provide —
//! this module never reads configuration or mutates the environment:
//!
//! ```text
//! <root>/<session id>/meta.json     entry + provider metadata (private, atomic)
//! <root>/<session id>/events.jsonl  append-only journal of human/agent events
//! ```
//!
//! Session ids are generated locally (no global state) and validated before
//! every use, so an id can never traverse out of the archive root. Journal
//! reads tolerate an incomplete final line and report — but never silently
//! discard — malformed records, and per-session metadata corruption is
//! reported per session instead of hiding other sessions. Streaming writes go
//! through [`BufferedSessionArchive`], which performs all filesystem work on a
//! background thread and only requires durability at explicit boundaries.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::AgentEvent;
use crate::persistence::{CURRENT_SCHEMA_VERSION, PersistenceError, SessionMetadata};

/// Current on-disk schema of the archive metadata envelope.
pub const ARCHIVE_SCHEMA_VERSION: u32 = 1;
/// Per-session metadata file name inside the archive root.
pub const META_FILE: &str = "meta.json";
/// Per-session append-only journal file name inside the archive root.
pub const EVENTS_FILE: &str = "events.jsonl";
/// Titles are bounded so a runaway agent response cannot bloat listings.
pub const MAX_TITLE_CHARS: usize = 200;

const MAX_ID_CHARS: usize = 64;
const KIND: &str = "session archive";

fn malformed(detail: String) -> PersistenceError {
    PersistenceError::Malformed { kind: KIND, detail }
}

fn is_not_found(error: &PersistenceError) -> bool {
    matches!(
        error,
        PersistenceError::Io(io) if io.kind() == std::io::ErrorKind::NotFound
    )
}

fn now_unix_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
        .unwrap_or(i64::MAX)
}

/// Timestamps are stored as Unix nanoseconds; this converts to a display value.
pub fn unix_nanos_time(nanos: i64) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(nanos)).ok()
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static ENTROPY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a fresh, filesystem-safe session id.
///
/// The value comes from the operating system entropy pool when available and
/// never reads, writes, or mutates global environment state, so repeated
/// generations in one process remain unique.
pub fn generate_session_id() -> String {
    let mut bytes = [0u8; 16];
    fill_random_bytes(&mut bytes);
    let mut id = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        id.push(char::from_digit(u32::from(byte >> 4), 16).expect("nibble"));
        id.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("nibble"));
    }
    id
}

fn fill_random_bytes(buffer: &mut [u8]) {
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut random) = File::open("/dev/urandom")
            && random.read_exact(buffer).is_ok()
        {
            return;
        }
    }
    fill_entropy_bytes(buffer);
}

fn fill_entropy_bytes(buffer: &mut [u8]) {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    seed ^= u64::from(std::process::id()) << 32;
    let hash_state = RandomState::new();
    for (index, chunk) in buffer.chunks_mut(8).enumerate() {
        let mut z = seed
            .wrapping_add(ENTROPY_COUNTER.fetch_add(1, Ordering::Relaxed))
            .wrapping_add(u64::try_from(index).unwrap_or(u64::MAX));
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        let mut hasher = hash_state.build_hasher();
        hasher.write_u64(z);
        z ^= hasher.finish();
        for (target, source) in chunk.iter_mut().zip(z.to_le_bytes()) {
            *target = source;
        }
    }
}

/// Reject ids that could escape the archive root or confuse the journal.
///
/// Valid ids are 1-64 ASCII letters, digits, `-` or `_`; `.` and separators
/// are excluded entirely, so `..`, `/`, and `\` can never appear.
pub fn validate_session_id(id: &str) -> Result<(), PersistenceError> {
    if id.is_empty() {
        return Err(malformed("session id must not be empty".into()));
    }
    if id.chars().count() > MAX_ID_CHARS {
        return Err(malformed(format!(
            "session id exceeds {MAX_ID_CHARS} characters"
        )));
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(malformed(
            "session id may only contain ASCII letters, digits, '-' and '_'".into(),
        ));
    }
    Ok(())
}

/// Lifecycle description shown by the session browser.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveState {
    Idle,
    Failed,
    #[default]
    Active,
    Completed,
    Cancelled,
    /// Unknown future states load as `Unknown` instead of failing the session.
    #[serde(other)]
    Unknown,
}

/// Immutable identity and presentation data for one archived session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArchiveEntry {
    pub id: String,
    /// Canonical project directory recorded at creation time.
    pub cwd: PathBuf,
    pub title: String,
    #[serde(default)]
    pub preview: String,
    /// Unix nanoseconds since the epoch.
    pub created_at: i64,
    /// Unix nanoseconds since the epoch; the archive advances this at
    /// durability boundaries so browsers can order by last activity.
    pub updated_at: i64,
    #[serde(default)]
    pub roster: Vec<String>,
    #[serde(default)]
    pub state: ArchiveState,
}

impl ArchiveEntry {
    pub fn created_time(&self) -> Option<time::OffsetDateTime> {
        unix_nanos_time(self.created_at)
    }

    pub fn updated_time(&self) -> Option<time::OffsetDateTime> {
        unix_nanos_time(self.updated_at)
    }
}

/// One archived conversation record: a public human message or a normalized
/// agent event, in journal order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ArchiveEvent {
    Human {
        text: String,
        /// `true` when the message was a direct/private turn rather than a
        /// roster-wide prompt.
        direct: bool,
    },
    Agent(AgentEvent),
}

impl ArchiveEvent {
    pub fn human(text: impl Into<String>, direct: bool) -> Self {
        Self::Human {
            text: text.into(),
            direct,
        }
    }

    pub fn agent(event: AgentEvent) -> Self {
        Self::Agent(event)
    }
}

/// A loaded archived session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchivedSession {
    pub entry: ArchiveEntry,
    /// Provider session metadata; unknown keys are preserved verbatim.
    pub metadata: SessionMetadata,
    /// Ordered journal contents.
    pub events: Vec<ArchiveEvent>,
    /// Human-readable reports of tolerated journal damage (torn final lines,
    /// skipped malformed records). Empty for healthy sessions.
    pub warnings: Vec<String>,
}

/// A session whose metadata could not be read. Reporting is per session so a
/// single corrupt file never hides other valid sessions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionFailure {
    pub id: String,
    pub detail: String,
}

/// Result of listing the archive.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionListing {
    /// Valid entries ordered by most recent activity.
    pub entries: Vec<ArchiveEntry>,
    /// Unreadable sessions with the reason each was skipped.
    pub failures: Vec<SessionFailure>,
}

/// Parameters for creating a new archived session.
#[derive(Clone, Debug)]
pub struct CreateSession {
    pub cwd: PathBuf,
    pub title: String,
    pub roster: Vec<String>,
    pub state: ArchiveState,
    pub metadata: SessionMetadata,
}

impl CreateSession {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            title: String::new(),
            roster: Vec::new(),
            state: ArchiveState::Active,
            metadata: SessionMetadata::empty(),
        }
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    pub fn roster(mut self, roster: Vec<String>) -> Self {
        self.roster = roster;
        self
    }

    pub fn state(mut self, state: ArchiveState) -> Self {
        self.state = state;
        self
    }

    pub fn metadata(mut self, metadata: SessionMetadata) -> Self {
        self.metadata = metadata;
        self
    }
}

fn bound_title(title: &str) -> String {
    title.chars().take(MAX_TITLE_CHARS).collect()
}

fn canonical_cwd(cwd: &Path) -> PathBuf {
    cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf())
}

/// On-disk metadata envelope. Unknown top-level keys are retained verbatim so
/// a future CodeSwarm version can round-trip them.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ArchiveMetaFile {
    entry: ArchiveEntry,
    metadata: SessionMetadata,
    extra: Map<String, Value>,
}

fn read_archive_meta(path: &Path) -> Result<ArchiveMetaFile, PersistenceError> {
    let raw = fs::read_to_string(path)?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| malformed(format!("{}: {error}", path.display())))?;
    let object = value
        .as_object()
        .ok_or_else(|| malformed("archive metadata must be a JSON object".into()))?;
    let version = match object.get("schema_version") {
        Some(Value::Number(number)) => number.as_u64().ok_or_else(|| {
            malformed("archive schema_version must be an unsigned integer".into())
        })?,
        Some(_) => {
            return Err(malformed(
                "archive schema_version must be an unsigned integer".into(),
            ));
        }
        None => u64::from(ARCHIVE_SCHEMA_VERSION),
    };
    if version > u64::from(ARCHIVE_SCHEMA_VERSION) {
        return Err(PersistenceError::UnsupportedVersion {
            kind: KIND,
            version: u32::try_from(version).unwrap_or(u32::MAX),
        });
    }
    let entry = object
        .get("entry")
        .ok_or_else(|| malformed("archive metadata is missing the session entry".into()))
        .and_then(|entry| {
            serde_json::from_value(entry.clone())
                .map_err(|error| malformed(format!("archive entry: {error}")))
        })?;
    let metadata = match object.get("metadata") {
        Some(Value::Object(object)) => SessionMetadata::new(object.clone()),
        Some(_) => {
            return Err(malformed(
                "archive metadata envelope must be an object".into(),
            ));
        }
        None => SessionMetadata::empty(),
    };
    let extra = object
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "schema_version" | "entry" | "metadata"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    Ok(ArchiveMetaFile {
        entry,
        metadata,
        extra,
    })
}

fn write_archive_meta(path: &Path, file: &ArchiveMetaFile) -> Result<(), PersistenceError> {
    let mut object = file.extra.clone();
    object.insert("schema_version".into(), Value::from(ARCHIVE_SCHEMA_VERSION));
    let entry = serde_json::to_value(&file.entry)
        .map_err(|error| malformed(format!("archive entry: {error}")))?;
    object.insert("entry".into(), entry);
    object.insert(
        "metadata".into(),
        Value::Object(file.metadata.as_object().clone()),
    );
    let payload = serde_json::to_string_pretty(&Value::Object(object))
        .map_err(|error| malformed(format!("archive metadata: {error}")))?;
    atomic_private_write(path, payload.as_bytes())
}

fn edit_archive_metadata<F>(path: &Path, apply: F) -> Result<(), PersistenceError>
where
    F: FnOnce(&mut SessionMetadata),
{
    let mut file = read_archive_meta(path)?;
    apply(&mut file.metadata);
    write_archive_meta(path, &file)
}

fn edit_archive_entry<F>(path: &Path, expected_id: &str, apply: F) -> Result<(), PersistenceError>
where
    F: FnOnce(&mut ArchiveEntry),
{
    let mut file = read_archive_meta(path)?;
    if file.entry.id != expected_id {
        return Err(malformed(format!(
            "archived entry id {:?} does not match session id {expected_id:?}",
            file.entry.id
        )));
    }
    apply(&mut file.entry);
    file.entry.id = expected_id.to_owned();
    file.entry.title = bound_title(&file.entry.title);
    write_archive_meta(path, &file)
}

fn replace_archive_metadata(
    path: &Path,
    metadata: &SessionMetadata,
) -> Result<(), PersistenceError> {
    edit_archive_metadata(path, |current| {
        *current = metadata.clone();
    })
}

fn encode_event_line(event: &ArchiveEvent) -> Result<String, PersistenceError> {
    let event = serde_json::to_value(event)
        .map_err(|error| malformed(format!("archive event: {error}")))?;
    let mut envelope = Map::new();
    envelope.insert("schema_version".into(), Value::from(CURRENT_SCHEMA_VERSION));
    envelope.insert("event".into(), event);
    let mut line = serde_json::to_string(&Value::Object(envelope))
        .map_err(|error| malformed(format!("archive event: {error}")))?;
    line.push('\n');
    Ok(line)
}

/// Read a journal tolerantly: an incomplete final line is a tolerated torn
/// write, malformed records are skipped and reported, and every valid earlier
/// record is always returned.
fn open_journal(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    set_private(&file)?;
    if file.metadata()?.len() > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut tail = [0];
        file.read_exact(&mut tail)?;
        if tail[0] != b'\n' {
            file.write_all(b"\n")?;
        }
    }
    Ok(file)
}

fn read_events(path: &Path) -> Result<(Vec<ArchiveEvent>, Vec<String>), PersistenceError> {
    let content = match fs::read(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new()));
        }
        Err(error) => return Err(error.into()),
    };
    let ends_with_newline = content.last() == Some(&b'\n');
    let mut lines = content.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    if ends_with_newline {
        lines.pop();
    }
    let last = lines.len().saturating_sub(1);
    let mut events = Vec::new();
    let mut warnings = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        match decode_event_line(line) {
            Ok(event) => events.push(event),
            Err(detail) => {
                if !ends_with_newline && index == last {
                    warnings.push(format!("tolerated incomplete final journal line: {detail}"));
                } else {
                    warnings.push(format!(
                        "skipped malformed journal line {}: {detail}",
                        index + 1
                    ));
                }
            }
        }
    }
    Ok((events, warnings))
}

/// Journal lines accept both the current schema-version envelope and bare
/// events, mirroring the house persistence format.
fn decode_event_line(line: &[u8]) -> Result<ArchiveEvent, String> {
    let text = std::str::from_utf8(line).map_err(|error| format!("invalid UTF-8: {error}"))?;
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("invalid JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or("journal line must be a JSON object")?;
    let has_envelope = object.contains_key("event") || object.contains_key("schema_version");
    let event_value = if has_envelope {
        let version = object
            .get("schema_version")
            .and_then(Value::as_u64)
            .unwrap_or(u64::from(CURRENT_SCHEMA_VERSION));
        if version > u64::from(CURRENT_SCHEMA_VERSION) {
            return Err(format!("unsupported journal schema version {version}"));
        }
        object.get("event").ok_or("envelope is missing an event")?
    } else {
        &value
    };
    serde_json::from_value(event_value.clone()).map_err(|error| error.to_string())
}

/// Local archive of every CodeSwarm session for one machine.
///
/// Constructed from an explicit root; this type performs no environment
/// lookups, so the root CLI stays in control of storage locations.
#[derive(Clone, Debug)]
pub struct SessionArchive {
    root: PathBuf,
}

impl SessionArchive {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn session_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    fn meta_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join(META_FILE)
    }

    fn events_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join(EVENTS_FILE)
    }

    /// Create a new session with a fresh, immutable id. An existing archived
    /// conversation is never overwritten: the id is allocated exclusively.
    pub fn create(&self, request: CreateSession) -> Result<ArchiveEntry, PersistenceError> {
        for _ in 0..8 {
            let id = generate_session_id();
            match self.create_exact(&id, request.clone()) {
                Ok(entry) => return Ok(entry),
                Err(PersistenceError::Io(error))
                    if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(malformed("unable to allocate a unique session id".into()))
    }

    /// Create a session with an explicit id. Fails when the id is invalid or
    /// already archived, so archived conversations can never be overwritten.
    pub fn create_exact(
        &self,
        id: &str,
        request: CreateSession,
    ) -> Result<ArchiveEntry, PersistenceError> {
        validate_session_id(id)?;
        fs::create_dir_all(&self.root)?;
        let dir = self.session_dir(id);
        fs::create_dir(&dir).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("archived session {id} already exists"),
                )
            } else {
                error
            }
        })?;
        set_private_dir(&dir)?;
        let now = now_unix_nanos();
        let entry = ArchiveEntry {
            id: id.to_owned(),
            cwd: canonical_cwd(&request.cwd),
            title: bound_title(&request.title),
            preview: String::new(),
            created_at: now,
            updated_at: now,
            roster: request.roster,
            state: request.state,
        };
        let file = ArchiveMetaFile {
            entry: entry.clone(),
            metadata: request.metadata,
            extra: Map::new(),
        };
        match write_archive_meta(&self.meta_path(id), &file) {
            Ok(()) => Ok(entry),
            Err(error) => {
                let _ = fs::remove_dir_all(&dir);
                Err(error)
            }
        }
    }

    /// List every archived session whose canonical project directory matches
    /// `cwd`. Failures are always reported: corrupt metadata cannot be
    /// attributed to a project, so it is surfaced for every listing.
    pub fn list(&self, cwd: &Path) -> Result<SessionListing, PersistenceError> {
        let mut listing = self.list_all()?;
        let wanted = canonical_cwd(cwd);
        listing
            .entries
            .retain(|entry| canonical_cwd(&entry.cwd) == wanted);
        Ok(listing)
    }

    /// List every archived session, newest activity first.
    pub fn list_all(&self) -> Result<SessionListing, PersistenceError> {
        let mut listing = SessionListing::default();
        let read_dir = match fs::read_dir(&self.root) {
            Ok(read_dir) => read_dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(listing);
            }
            Err(error) => return Err(error.into()),
        };
        for candidate in read_dir {
            let Ok(candidate) = candidate else {
                continue;
            };
            let path = candidate.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
                continue;
            };
            if name.starts_with('.') || validate_session_id(name).is_err() {
                continue;
            }
            let meta_path = path.join(META_FILE);
            match read_archive_meta(&meta_path) {
                Ok(file) => listing.entries.push(file.entry),
                Err(error) => {
                    let missing_meta = is_not_found(&error) && !path.join(EVENTS_FILE).exists();
                    if !missing_meta {
                        listing.failures.push(SessionFailure {
                            id: name.to_owned(),
                            detail: error.to_string(),
                        });
                    }
                }
            }
        }
        listing.entries.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.created_at.cmp(&a.created_at))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(listing)
    }

    /// Load one archived session: entry, provider metadata, and ordered
    /// events. Missing sessions and unreadable metadata are reported with
    /// distinct errors; damaged journals still return every valid event.
    pub fn load(&self, id: &str) -> Result<ArchivedSession, PersistenceError> {
        validate_session_id(id)?;
        let file = read_archive_meta(&self.meta_path(id)).map_err(|error| {
            if is_not_found(&error) {
                PersistenceError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("archived session {id} was not found"),
                ))
            } else {
                error
            }
        })?;
        if file.entry.id != id {
            return Err(malformed(format!(
                "archived entry id {:?} does not match session id {id:?}",
                file.entry.id
            )));
        }
        let (events, warnings) = read_events(&self.events_path(id))?;
        Ok(ArchivedSession {
            entry: file.entry,
            metadata: file.metadata,
            events,
            warnings,
        })
    }

    /// Append one journal record synchronously and force it to stable
    /// storage. Runtime streaming should prefer [`Self::buffered`].
    pub fn append(&self, id: &str, event: &ArchiveEvent) -> Result<(), PersistenceError> {
        validate_session_id(id)?;
        let line = encode_event_line(event)?;
        let path = self.events_path(id);
        if !self.meta_path(id).exists() {
            return Err(PersistenceError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("archived session {id} was not found"),
            )));
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let mut file = open_journal(&path)?;
        file.write_all(line.as_bytes())?;
        file.sync_data()?;
        Ok(())
    }

    pub fn append_human(
        &self,
        id: &str,
        text: impl Into<String>,
        direct: bool,
    ) -> Result<(), PersistenceError> {
        self.append(id, &ArchiveEvent::human(text, direct))
    }

    pub fn append_agent(&self, id: &str, event: &AgentEvent) -> Result<(), PersistenceError> {
        self.append(id, &ArchiveEvent::agent(event.clone()))
    }

    /// Edit provider metadata in place, preserving unknown keys.
    pub fn update_metadata<F>(&self, id: &str, apply: F) -> Result<(), PersistenceError>
    where
        F: FnOnce(&mut SessionMetadata),
    {
        validate_session_id(id)?;
        edit_archive_metadata(&self.meta_path(id), apply)
    }

    /// Edit the archive entry (title, state, roster, activity timestamp).
    /// The session id is pinned to the directory; titles stay bounded.
    pub fn update_entry<F>(&self, id: &str, apply: F) -> Result<(), PersistenceError>
    where
        F: FnOnce(&mut ArchiveEntry),
    {
        validate_session_id(id)?;
        edit_archive_entry(&self.meta_path(id), id, apply)
    }

    pub fn set_title(&self, id: &str, title: impl AsRef<str>) -> Result<(), PersistenceError> {
        let title = title.as_ref().to_owned();
        self.update_entry(id, |entry| entry.title = title)
    }

    pub fn set_state(&self, id: &str, state: ArchiveState) -> Result<(), PersistenceError> {
        self.update_entry(id, |entry| entry.state = state)
    }

    /// Mark the session as most recently active.
    pub fn touch(&self, id: &str) -> Result<(), PersistenceError> {
        self.update_entry(id, |entry| entry.updated_at = now_unix_nanos())
    }

    /// Start a background writer that keeps journal and metadata filesystem
    /// work off the caller thread. Events and metadata edits become durable
    /// at [`BufferedSessionArchive::flush`] boundaries and on drop.
    pub fn buffered(&self, id: &str) -> std::io::Result<BufferedSessionArchive> {
        self.buffered_with_errors(id, |_| {})
    }

    /// Like [`Self::buffered`], reporting background write failures through
    /// `on_error` so the terminal can surface them in the status ribbon.
    pub fn buffered_with_errors<F>(
        &self,
        id: &str,
        on_error: F,
    ) -> std::io::Result<BufferedSessionArchive>
    where
        F: Fn(String) + Send + 'static,
    {
        validate_session_id(id).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
        })?;
        let entry_path = self.meta_path(id);
        if !entry_path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("archived session {id} was not found"),
            ));
        }
        let worker = ArchiveWorker {
            id: id.to_owned(),
            entry_path,
            events_path: self.events_path(id),
            writer: None,
            retry: VecDeque::new(),
            pending: VecDeque::new(),
            events_since_boundary: false,
            first_error: None,
            on_error: Box::new(on_error),
        };
        BufferedSessionArchive::spawn(id.to_owned(), worker)
    }
}

type MetadataEdit = Box<dyn FnOnce(&mut SessionMetadata) + Send>;
type EntryEdit = Box<dyn FnOnce(&mut ArchiveEntry) + Send>;

enum PendingOperation {
    ReplaceMetadata(SessionMetadata),
    EditMetadata(Option<MetadataEdit>),
    EditEntry(Option<EntryEdit>),
    TouchEntry,
    WriteSnapshot(ArchiveMetaFile),
}

enum ArchiveCommand {
    AppendEvent {
        line: String,
    },
    Enqueue(PendingOperation),
    Checkpoint,
    Flush {
        reply: Sender<Result<(), PersistenceError>>,
    },
    Shutdown {
        reply: Sender<()>,
    },
}

struct ArchiveWorker {
    id: String,
    entry_path: PathBuf,
    events_path: PathBuf,
    writer: Option<std::io::BufWriter<File>>,
    retry: VecDeque<String>,
    pending: VecDeque<PendingOperation>,
    events_since_boundary: bool,
    first_error: Option<String>,
    on_error: Box<dyn Fn(String) + Send>,
}

impl ArchiveWorker {
    fn note_error(&mut self, error: &PersistenceError) {
        if self.first_error.is_none() {
            self.first_error = Some(error.to_string());
            (self.on_error)(error.to_string());
        }
    }

    fn ensure_open(&mut self) -> Result<&mut std::io::BufWriter<File>, PersistenceError> {
        if self.writer.is_none() {
            if let Some(parent) = self.events_path.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)?;
            }
            let file = open_journal(&self.events_path)?;
            self.writer = Some(std::io::BufWriter::new(file));
        }
        Ok(self.writer.as_mut().expect("writer initialized"))
    }

    fn drain_retry(&mut self) -> Result<(), PersistenceError> {
        while !self.retry.is_empty() {
            let line = self.retry.front().cloned().expect("retry non-empty");
            let writer = self.ensure_open()?;
            writer
                .write_all(line.as_bytes())
                .map_err(PersistenceError::Io)?;
            self.retry.pop_front();
        }
        Ok(())
    }

    fn sync_journal(&mut self) -> Result<(), PersistenceError> {
        self.drain_retry()?;
        if let Some(writer) = self.writer.as_mut() {
            writer.flush()?;
            writer.get_ref().sync_data()?;
        }
        Ok(())
    }

    fn apply_operation(
        &mut self,
        operation: &mut PendingOperation,
    ) -> Result<(), PersistenceError> {
        match operation {
            PendingOperation::WriteSnapshot(file) => write_archive_meta(&self.entry_path, file),
            PendingOperation::ReplaceMetadata(metadata) => {
                replace_archive_metadata(&self.entry_path, metadata)
            }
            PendingOperation::EditMetadata(edit) => {
                let mut file = read_archive_meta(&self.entry_path)?;
                if let Some(edit) = edit.take() {
                    edit(&mut file.metadata);
                }
                *operation = PendingOperation::WriteSnapshot(file);
                self.apply_operation(operation)
            }
            PendingOperation::EditEntry(edit) => {
                let mut file = read_archive_meta(&self.entry_path)?;
                if let Some(edit) = edit.take() {
                    edit(&mut file.entry);
                }
                file.entry.id = self.id.clone();
                file.entry.title = bound_title(&file.entry.title);
                *operation = PendingOperation::WriteSnapshot(file);
                self.apply_operation(operation)
            }
            PendingOperation::TouchEntry => {
                let mut file = read_archive_meta(&self.entry_path)?;
                file.entry.updated_at = now_unix_nanos();
                write_archive_meta(&self.entry_path, &file)
            }
        }
    }

    fn complete_boundary(&mut self) -> Result<(), PersistenceError> {
        let mut result = self.sync_journal();
        if result.is_ok() && self.events_since_boundary {
            self.pending.push_back(PendingOperation::TouchEntry);
            self.events_since_boundary = false;
        }
        while result.is_ok() {
            let Some(mut operation) = self.pending.pop_front() else {
                break;
            };
            if let Err(error) = self.apply_operation(&mut operation) {
                self.note_error(&error);
                self.pending.push_front(operation);
                result = Err(error);
            }
        }
        if result.is_ok() {
            self.first_error = None;
        }
        result
    }

    fn run(mut self, receiver: Receiver<ArchiveCommand>) -> Result<(), PersistenceError> {
        loop {
            let command = match receiver.recv_timeout(std::time::Duration::from_secs(1)) {
                Ok(command) => command,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.pending.is_empty()
                        && self.retry.is_empty()
                        && !self.events_since_boundary
                    {
                        continue;
                    }
                    if let Err(error) = self.complete_boundary() {
                        self.note_error(&error);
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            match command {
                ArchiveCommand::Checkpoint => {
                    if let Err(error) = self.complete_boundary() {
                        self.note_error(&error);
                    }
                }
                ArchiveCommand::AppendEvent { line } => {
                    self.retry.push_back(line);
                    self.events_since_boundary = true;
                    if let Err(error) = self.drain_retry() {
                        self.note_error(&error);
                    }
                }
                ArchiveCommand::Enqueue(operation) => {
                    self.pending.push_back(operation);
                }
                ArchiveCommand::Flush { reply } => {
                    let result = self.complete_boundary();
                    let _ = reply.send(result);
                }
                ArchiveCommand::Shutdown { reply } => {
                    let result = self.complete_boundary();
                    let _ = reply.send(());
                    return result;
                }
            }
        }
        self.complete_boundary()
    }
}

/// Background archive writer used from the terminal event loop.
///
/// The handle only serializes events and queues commands; all filesystem work
/// happens on its worker thread. Queued journal lines and metadata edits are
/// retried until [`flush`](Self::flush) or drop makes them durable, and the
/// entry's `updated_at` advances at each successful boundary that received
/// events so the session browser can order by last activity.
pub struct BufferedSessionArchive {
    id: String,
    sender: Sender<ArchiveCommand>,
    worker: Option<std::thread::JoinHandle<Result<(), PersistenceError>>>,
}

impl std::fmt::Debug for BufferedSessionArchive {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BufferedSessionArchive")
            .field("id", &self.id)
            .field("worker_running", &self.worker.is_some())
            .finish_non_exhaustive()
    }
}

impl BufferedSessionArchive {
    fn spawn(id: String, worker: ArchiveWorker) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("codeswarm-session-archive".into())
            .spawn(move || worker.run(receiver))?;
        Ok(Self {
            id,
            sender,
            worker: Some(thread),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    fn send(&self, command: ArchiveCommand) -> Result<(), PersistenceError> {
        self.sender.send(command).map_err(|_| {
            PersistenceError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "session archive background writer stopped",
            ))
        })
    }

    /// Serialize and queue one journal record without filesystem I/O in the
    /// caller.
    pub fn append(&self, event: &ArchiveEvent) -> Result<(), PersistenceError> {
        let line = encode_event_line(event)?;
        self.send(ArchiveCommand::AppendEvent { line })
    }

    pub fn append_human(
        &self,
        text: impl Into<String>,
        direct: bool,
    ) -> Result<(), PersistenceError> {
        self.append(&ArchiveEvent::human(text, direct))
    }

    pub fn append_agent(&self, event: &AgentEvent) -> Result<(), PersistenceError> {
        self.append(&ArchiveEvent::agent(event.clone()))
    }

    /// Queue a complete provider metadata snapshot, retaining unknown keys.
    pub fn replace_metadata(&self, metadata: SessionMetadata) -> Result<(), PersistenceError> {
        self.send(ArchiveCommand::Enqueue(PendingOperation::ReplaceMetadata(
            metadata,
        )))
    }

    /// Queue an in-place provider metadata edit, preserving unknown keys.
    pub fn update_metadata<F>(&self, apply: F) -> Result<(), PersistenceError>
    where
        F: FnOnce(&mut SessionMetadata) + Send + 'static,
    {
        self.send(ArchiveCommand::Enqueue(PendingOperation::EditMetadata(
            Some(Box::new(apply)),
        )))
    }

    /// Queue an archive entry edit (title, state, roster). The session id is
    /// pinned to the directory and titles stay bounded.
    pub fn update_entry<F>(&self, apply: F) -> Result<(), PersistenceError>
    where
        F: FnOnce(&mut ArchiveEntry) + Send + 'static,
    {
        self.send(ArchiveCommand::Enqueue(PendingOperation::EditEntry(Some(
            Box::new(apply),
        ))))
    }

    /// Queue an activity timestamp update.
    pub fn touch(&self) -> Result<(), PersistenceError> {
        self.send(ArchiveCommand::Enqueue(PendingOperation::TouchEntry))
    }

    /// Request a durable checkpoint without blocking the UI.
    pub fn checkpoint(&self) -> Result<(), PersistenceError> {
        self.send(ArchiveCommand::Checkpoint)
    }

    /// Drain queued records and metadata edits and make them durable.
    pub fn flush(&self) -> Result<(), PersistenceError> {
        let (reply, result) = mpsc::channel();
        self.send(ArchiveCommand::Flush { reply })?;
        result.recv().map_err(|_| {
            PersistenceError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "session archive background writer stopped",
            ))
        })?
    }
}

impl Drop for BufferedSessionArchive {
    fn drop(&mut self) {
        let Some(worker) = self.worker.take() else {
            return;
        };
        let (reply, result) = mpsc::channel();
        if self.sender.send(ArchiveCommand::Shutdown { reply }).is_ok() {
            let _ = result.recv();
        }
        let _ = worker.join();
    }
}

#[cfg(unix)]
fn set_private(file: &File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private(_file: &File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn atomic_private_write(path: &Path, payload: &[u8]) -> Result<(), PersistenceError> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("meta");
    let temporary = path.with_file_name(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        set_private(&file)?;
        file.write_all(payload)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::{
        ArchiveEvent, ArchiveState, BufferedSessionArchive, CreateSession, EVENTS_FILE,
        MAX_TITLE_CHARS, META_FILE, SessionArchive, SessionListing, generate_session_id,
        validate_session_id,
    };
    use crate::AgentEvent;
    use crate::persistence::{PersistenceError, SessionMetadata};

    fn temp_root(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("codeswarm-session-archive-{unique}-{name}"));
        fs::create_dir_all(&root).expect("create root");
        root
    }

    fn project(root: &std::path::Path, name: &str) -> std::path::PathBuf {
        let path = root.join(name);
        fs::create_dir_all(&path).expect("create project");
        path
    }

    fn metadata(pairs: &[(&str, serde_json::Value)]) -> SessionMetadata {
        let mut data = SessionMetadata::empty();
        for (key, value) in pairs {
            data.insert(*key, value.clone());
        }
        data
    }

    fn remove(root: std::path::PathBuf) {
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn generated_ids_are_unique_and_path_safe() {
        let mut ids = std::collections::BTreeSet::new();
        for _ in 0..512 {
            let id = generate_session_id();
            assert_eq!(id.len(), 32);
            assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
            validate_session_id(&id).expect("valid generated id");
            ids.insert(id);
        }
        assert_eq!(ids.len(), 512);
    }

    #[test]
    fn invalid_session_ids_are_rejected_everywhere() {
        let root = temp_root("invalid-ids");
        let archive = SessionArchive::open(&root);
        let request = CreateSession::new(root.join("project"));
        for id in [
            "",
            "../escape",
            "sub/dir",
            "sub\\dir",
            ".hidden",
            "a b",
            "id\n",
        ] {
            assert!(
                super::validate_session_id(id).is_err(),
                "id {id:?} must be rejected"
            );
            assert!(archive.create_exact(id, request.clone()).is_err(), "{id:?}");
            assert!(archive.load(id).is_err(), "{id:?}");
            assert!(archive.buffered(id).is_err(), "{id:?}");
        }
        let too_long = "a".repeat(65);
        assert!(archive.create_exact(&too_long, request.clone()).is_err());
        assert_eq!(
            fs::read_dir(&root).expect("root").count(),
            0,
            "invalid ids must not create directories"
        );
        remove(root);
    }

    #[test]
    fn create_and_load_round_trip_metadata() {
        let root = temp_root("create-load");
        let archive = SessionArchive::open(&root);
        let cwd = project(&root, "relay-fix");
        let entry = archive
            .create(
                CreateSession::new(&cwd)
                    .title("Fix the relay ring")
                    .roster(vec!["Codex".into(), "Agy".into()])
                    .metadata(metadata(&[("provider_session", json!("abc-123"))])),
            )
            .expect("create");
        assert_eq!(entry.title, "Fix the relay ring");
        assert_eq!(entry.roster, vec!["Codex".to_owned(), "Agy".to_owned()]);
        assert_eq!(entry.state, ArchiveState::Active);
        assert_eq!(entry.cwd, cwd.canonicalize().expect("canonical"));
        assert!(entry.created_at > 0);
        assert!(entry.updated_at >= entry.created_at);
        assert!(entry.created_time().is_some());

        let loaded = archive.load(&entry.id).expect("load");
        assert_eq!(loaded.entry, entry);
        assert!(loaded.events.is_empty());
        assert!(loaded.warnings.is_empty());
        assert_eq!(
            loaded.metadata.get("provider_session"),
            Some(&json!("abc-123"))
        );
        remove(root);
    }

    #[test]
    fn new_sessions_never_overwrite_archived_conversations() {
        let root = temp_root("no-overwrite");
        let archive = SessionArchive::open(&root);
        let request = CreateSession::new(root.join("project")).title("original");
        let entry = archive
            .create_exact("fixedid", request.clone())
            .expect("create");
        archive
            .append_human("fixedid", "precious history", false)
            .expect("append");
        let conflict = archive
            .create_exact("fixedid", request)
            .expect_err("exists");
        assert_eq!(
            conflict.to_string(),
            "persistence I/O error: archived session fixedid already exists"
        );
        let generated = archive
            .create(CreateSession::new(root.join("project")).title("other"))
            .expect("create");
        assert_ne!(generated.id, entry.id);
        let loaded = archive.load("fixedid").expect("load");
        assert_eq!(loaded.entry.title, "original");
        assert_eq!(
            loaded.events,
            vec![ArchiveEvent::human("precious history", false)]
        );
        remove(root);
    }

    #[test]
    fn events_round_trip_in_order() {
        let root = temp_root("events");
        let archive = SessionArchive::open(&root);
        let entry = archive
            .create(CreateSession::new(root.join("p")))
            .expect("create");
        archive
            .append_human(&entry.id, "please fix the ring", false)
            .expect("append human");
        archive
            .append_agent(
                &entry.id,
                &AgentEvent::Text {
                    slot: 0,
                    text: "on it".into(),
                },
            )
            .expect("append agent");
        archive
            .append_human(&entry.id, "direct note", true)
            .expect("append direct");
        let loaded = archive.load(&entry.id).expect("load");
        assert_eq!(
            loaded.events,
            vec![
                ArchiveEvent::human("please fix the ring", false),
                ArchiveEvent::agent(AgentEvent::Text {
                    slot: 0,
                    text: "on it".into(),
                }),
                ArchiveEvent::human("direct note", true),
            ]
        );
        assert!(loaded.warnings.is_empty());
        assert!(loaded.entry.updated_at >= loaded.entry.created_at);
        remove(root);
    }

    #[test]
    fn titles_are_bounded_safely() {
        let root = temp_root("titles");
        let archive = SessionArchive::open(&root);
        let long = "héllo wörld ".repeat(40);
        let entry = archive
            .create(CreateSession::new(root.join("p")).title(long.clone()))
            .expect("create");
        assert_eq!(entry.title.chars().count(), MAX_TITLE_CHARS);
        assert!(long.starts_with(&entry.title));
        assert!(
            entry
                .title
                .chars()
                .last()
                .is_some_and(char::is_alphanumeric)
        );
        remove(root);
    }

    #[test]
    fn listing_matches_canonical_cwd() {
        let root = temp_root("canonical");
        let archive = SessionArchive::open(&root);
        let project_dir = project(&root, "widget");
        let first = archive
            .create(CreateSession::new(&project_dir).title("first"))
            .expect("create");
        let second = archive
            .create(CreateSession::new(root.join("other")).title("second"))
            .expect("create");
        let listing = archive.list(&project_dir).expect("list");
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|entry| entry.id.clone())
                .collect::<Vec<_>>(),
            vec![first.id.clone()]
        );
        assert_eq!(archive.list(&root).expect("list").entries.len(), 0);
        #[cfg(unix)]
        {
            let alias = root.join("widget-alias");
            std::os::unix::fs::symlink(&project_dir, &alias).expect("symlink");
            let through_alias = archive.list(&alias).expect("list via symlink");
            assert_eq!(
                through_alias
                    .entries
                    .iter()
                    .map(|entry| entry.id.clone())
                    .collect::<Vec<_>>(),
                vec![first.id]
            );
        }
        let _ = second;
        remove(root);
    }

    #[test]
    fn list_orders_by_recent_activity() {
        let root = temp_root("ordering");
        let archive = SessionArchive::open(&root);
        let first = archive
            .create(CreateSession::new(root.join("p")))
            .expect("create");
        let second = archive
            .create(CreateSession::new(root.join("p")))
            .expect("create");
        let third = archive
            .create(CreateSession::new(root.join("p")))
            .expect("create");
        archive
            .update_entry(&first.id, |entry| entry.updated_at = 100)
            .expect("edit");
        archive
            .update_entry(&second.id, |entry| entry.updated_at = 300)
            .expect("edit");
        archive
            .update_entry(&third.id, |entry| entry.updated_at = 200)
            .expect("edit");
        let listing = archive.list_all().expect("list");
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|entry| entry.id.clone())
                .collect::<Vec<_>>(),
            vec![second.id, third.id, first.id]
        );
        remove(root);
    }

    #[test]
    fn corrupt_metadata_does_not_hide_valid_sessions() {
        let root = temp_root("corrupt");
        let archive = SessionArchive::open(&root);
        let healthy = archive
            .create(CreateSession::new(root.join("p")).title("healthy"))
            .expect("create");
        let corrupt = archive
            .create_exact("corruptmeta", CreateSession::new(root.join("p")))
            .expect("create");
        fs::write(root.join("corruptmeta").join(META_FILE), "{not json").expect("write");
        let orphan_dir = root.join("orphan_events");
        fs::create_dir(&orphan_dir).expect("dir");
        fs::write(orphan_dir.join(EVENTS_FILE), "leftover\n").expect("write");
        fs::write(orphan_dir.join(META_FILE), "{\"cwd\":").expect("write");

        let listing = archive.list_all().expect("list");
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|entry| entry.id.clone())
                .collect::<Vec<_>>(),
            vec![healthy.id]
        );
        assert_eq!(
            listing
                .failures
                .iter()
                .map(|failure| failure.id.clone())
                .collect::<Vec<_>>(),
            vec!["corruptmeta".to_owned(), "orphan_events".to_owned()]
        );
        assert!(
            listing
                .failures
                .iter()
                .all(|failure| !failure.detail.is_empty())
        );
        assert!(archive.load("corruptmeta").is_err());
        assert_eq!(
            archive.list(&root.join("p")).expect("list").entries.len(),
            1,
            "corrupt metadata must not hide this project's healthy session"
        );
        let _ = corrupt;
        remove(root);
    }

    #[test]
    fn session_without_metadata_files_is_ignored_when_empty() {
        let root = temp_root("torn-create");
        let archive = SessionArchive::open(&root);
        fs::create_dir(root.join("emptysession")).expect("dir");
        let listing: SessionListing = archive.list_all().expect("list");
        assert!(listing.entries.is_empty());
        assert!(listing.failures.is_empty());
        remove(root);
    }

    #[test]
    fn torn_final_journal_line_is_tolerated_without_losing_events() {
        let root = temp_root("torn-journal");
        let archive = SessionArchive::open(&root);
        let entry = archive
            .create(CreateSession::new(root.join("p")))
            .expect("create");
        archive
            .append_human(&entry.id, "kept message", false)
            .expect("append");
        let journal = root.join(&entry.id).join(EVENTS_FILE);
        let intact = fs::read_to_string(&journal).expect("read journal");
        fs::write(&journal, format!("{intact}{{\"type\":\"hum")).expect("torn write");
        let loaded = archive.load(&entry.id).expect("load");
        assert_eq!(
            loaded.events,
            vec![ArchiveEvent::human("kept message", false)]
        );
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("incomplete final journal line"));
        remove(root);
    }

    #[test]
    fn malformed_middle_journal_lines_do_not_discard_valid_events() {
        let root = temp_root("middle-journal");
        let archive = SessionArchive::open(&root);
        let entry = archive
            .create(CreateSession::new(root.join("p")))
            .expect("create");
        let first = serde_json::to_string(&ArchiveEvent::human("first", false)).expect("json");
        let third = serde_json::to_string(&ArchiveEvent::agent(AgentEvent::Text {
            slot: 1,
            text: "third".into(),
        }))
        .expect("json");
        fs::write(
            root.join(&entry.id).join(EVENTS_FILE),
            format!("{first}\nnot-json-at-all\n{third}\n"),
        )
        .expect("write journal");
        let loaded = archive.load(&entry.id).expect("load");
        assert_eq!(
            loaded.events,
            vec![
                ArchiveEvent::human("first", false),
                ArchiveEvent::agent(AgentEvent::Text {
                    slot: 1,
                    text: "third".into(),
                }),
            ]
        );
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("line 2"));
        remove(root);
    }

    #[test]
    fn unknown_metadata_and_top_level_keys_are_preserved() {
        let root = temp_root("unknown-keys");
        let archive = SessionArchive::open(&root);
        let entry = archive
            .create(CreateSession::new(root.join("p")).metadata(metadata(&[
                ("agents", json!([{"identity": "openai.com"}])),
                ("mystery_provider_key", json!({"keep": true})),
            ])))
            .expect("create");
        let meta_path = root.join(&entry.id).join(META_FILE);
        let mut raw = fs::read_to_string(&meta_path).expect("read meta");
        raw.insert_str(raw.len() - 1, ",\"future_top_level\":{\"carried\":true}");
        fs::write(&meta_path, raw).expect("write meta");
        let raw = fs::read_to_string(&meta_path).expect("read meta");
        let future = raw.replace("\"schema_version\": 1", "\"schema_version\": 99");
        fs::write(&meta_path, &future).expect("write meta");
        assert!(archive.load(&entry.id).is_err(), "future schema is refused");
        fs::write(&meta_path, raw).expect("restore meta");
        archive
            .update_metadata(&entry.id, |data| {
                data.insert("added_later", json!("value"));
            })
            .expect("update");
        let loaded = archive.load(&entry.id).expect("load");
        assert_eq!(
            loaded.metadata.get("mystery_provider_key"),
            Some(&json!({"keep": true}))
        );
        assert_eq!(loaded.metadata.get("added_later"), Some(&json!("value")));
        assert_eq!(
            loaded.metadata.get("agents"),
            Some(&json!([{"identity": "openai.com"}]))
        );
        let on_disk = fs::read_to_string(&meta_path).expect("read meta");
        assert!(on_disk.contains("\"future_top_level\""));
        remove(root);
    }

    #[test]
    fn entry_states_round_trip_and_unknown_states_fall_back() {
        let root = temp_root("states");
        let archive = SessionArchive::open(&root);
        let entry = archive
            .create(CreateSession::new(root.join("p")).state(ArchiveState::Completed))
            .expect("create");
        archive
            .set_state(&entry.id, ArchiveState::Cancelled)
            .expect("set");
        assert_eq!(
            archive.load(&entry.id).expect("load").entry.state,
            ArchiveState::Cancelled
        );
        let meta_path = root.join(&entry.id).join(META_FILE);
        let raw = fs::read_to_string(&meta_path).expect("read");
        let updated = raw.replace("\"state\": \"cancelled\"", "\"state\": \"fancy_future\"");
        fs::write(&meta_path, updated).expect("write");
        assert_eq!(
            archive.load(&entry.id).expect("load").entry.state,
            ArchiveState::Unknown
        );
        remove(root);
    }

    #[test]
    fn missing_sessions_and_roots_are_clean_errors() {
        let root = temp_root("missing");
        let archive = SessionArchive::open(root.join("does-not-exist"));
        let listing = archive.list_all().expect("empty listing");
        assert!(listing.entries.is_empty());
        assert!(listing.failures.is_empty());
        let error = archive.load("absent").expect_err("missing session");
        assert!(matches!(
            error,
            PersistenceError::Io(ref io) if io.kind() == std::io::ErrorKind::NotFound
        ));
        let archive = SessionArchive::open(&root);
        let entry = archive
            .create(CreateSession::new(root.join("p")))
            .expect("create");
        let loaded = archive.load(&entry.id).expect("load without journal");
        assert!(loaded.events.is_empty());
        assert!(loaded.warnings.is_empty());
        let orphan = archive
            .append("nevercreated", &ArchiveEvent::human("orphan", false))
            .expect_err("append without session");
        assert!(matches!(
            orphan,
            PersistenceError::Io(ref io) if io.kind() == std::io::ErrorKind::NotFound
        ));
        assert!(!root.join("nevercreated").exists(), "no orphan directory");
        remove(root);
    }

    #[test]
    fn appending_after_a_torn_journal_retains_the_new_record() {
        let root = temp_root("torn-append");
        let cwd = project(&root, "project");
        let archive = SessionArchive::open(root.join("archive"));
        let entry = archive.create(CreateSession::new(cwd)).unwrap();
        let path = archive.events_path(&entry.id);
        fs::write(&path, b"{\"incomplete\":").unwrap();
        let writer = archive.buffered(&entry.id).unwrap();
        writer.append_human("new message", false).unwrap();
        writer.flush().unwrap();
        drop(writer);
        let loaded = archive.load(&entry.id).unwrap();
        assert_eq!(
            loaded.events,
            vec![ArchiveEvent::human("new message", false)]
        );
        assert_eq!(loaded.warnings.len(), 1);
        remove(root);
    }

    #[test]
    fn metadata_edit_is_retained_when_its_first_write_fails() {
        let root = temp_root("edit-retry");
        let cwd = project(&root, "project");
        let archive = SessionArchive::open(root.join("archive"));
        let entry = archive.create(CreateSession::new(cwd)).unwrap();
        let path = archive.meta_path(&entry.id);
        let original = fs::read(&path).unwrap();
        let writer = archive.buffered(&entry.id).unwrap();
        let moved_path = path.clone();
        writer
            .update_metadata(move |metadata| {
                metadata.insert("retained", true);
                fs::remove_file(&moved_path).unwrap();
                fs::create_dir(&moved_path).unwrap();
            })
            .unwrap();
        assert!(writer.flush().is_err());
        fs::remove_dir(&path).unwrap();
        fs::write(&path, original).unwrap();
        writer.flush().unwrap();
        assert_eq!(
            archive.load(&entry.id).unwrap().metadata.get("retained"),
            Some(&json!(true))
        );
        drop(writer);
        remove(root);
    }

    #[test]
    fn buffered_writer_is_durable_at_flush_and_drop() {
        let root = temp_root("buffered");
        let archive = SessionArchive::open(&root);
        let entry = archive
            .create(CreateSession::new(root.join("p")))
            .expect("create");
        let writer: BufferedSessionArchive = archive.buffered(&entry.id).expect("writer");
        assert_eq!(writer.id(), entry.id);
        writer.append_human("queued first", false).expect("queue");
        writer.flush().expect("flush");
        let loaded = archive.load(&entry.id).expect("load");
        assert_eq!(
            loaded.events,
            vec![ArchiveEvent::human("queued first", false)]
        );
        let before = loaded.entry.updated_at;

        writer
            .append_agent(&AgentEvent::Text {
                slot: 0,
                text: "reply".into(),
            })
            .expect("queue");
        writer.append_human("queued second", true).expect("queue");
        drop(writer);
        let loaded = archive.load(&entry.id).expect("load after drop");
        assert_eq!(
            loaded.events,
            vec![
                ArchiveEvent::human("queued first", false),
                ArchiveEvent::agent(AgentEvent::Text {
                    slot: 0,
                    text: "reply".into(),
                }),
                ArchiveEvent::human("queued second", true),
            ]
        );
        assert!(loaded.entry.updated_at >= before);
        assert!(loaded.warnings.is_empty());
        remove(root);
    }

    #[test]
    fn buffered_metadata_operations_apply_in_order() {
        let root = temp_root("buffered-meta");
        let archive = SessionArchive::open(&root);
        let entry = archive
            .create(
                CreateSession::new(root.join("p"))
                    .metadata(metadata(&[("provider_session", json!("before"))])),
            )
            .expect("create");
        let writer = archive.buffered(&entry.id).expect("writer");
        writer
            .replace_metadata(metadata(&[("provider_session", json!("replaced"))]))
            .expect("queue snapshot");
        writer
            .update_metadata(|data| {
                data.insert("roster", json!(["Codex", "Agy"]));
                data.insert("mystery", json!({"keep": true}));
            })
            .expect("queue edit");
        writer
            .update_entry(|entry| {
                entry.title = "renamed".into();
                entry.state = ArchiveState::Completed;
            })
            .expect("queue entry edit");
        writer.flush().expect("flush");
        let loaded = archive.load(&entry.id).expect("load");
        assert_eq!(loaded.entry.title, "renamed");
        assert_eq!(loaded.entry.state, ArchiveState::Completed);
        assert_eq!(
            loaded.metadata.get("provider_session"),
            Some(&json!("replaced"))
        );
        assert_eq!(
            loaded.metadata.get("roster"),
            Some(&json!(["Codex", "Agy"]))
        );
        assert_eq!(loaded.metadata.get("mystery"), Some(&json!({"keep": true})));
        remove(root);
    }

    #[test]
    fn buffered_writer_recovers_from_transient_failures_and_reports() {
        let root = temp_root("buffered-recovery");
        let archive = SessionArchive::open(&root);
        let entry = archive
            .create(CreateSession::new(root.join("p")))
            .expect("create");
        let journal = root.join(&entry.id).join(EVENTS_FILE);
        let (sender, errors) = mpsc::channel();
        let writer = archive
            .buffered_with_errors(&entry.id, move |error| {
                let _ = sender.send(error);
            })
            .expect("writer");
        fs::write(&journal, b"").expect("create journal");
        fs::remove_file(&journal).expect("remove journal");
        fs::create_dir(&journal).expect("break journal path");
        writer.append_human("during failure", false).expect("queue");
        assert!(
            errors.recv_timeout(Duration::from_secs(2)).is_ok(),
            "background failure must be reported"
        );
        writer.flush().expect_err("flush reports the failure");
        fs::remove_dir(&journal).expect("repair journal path");
        writer.flush().expect("flush after repair");
        let loaded = archive.load(&entry.id).expect("load");
        assert_eq!(
            loaded.events,
            vec![ArchiveEvent::human("during failure", false)]
        );
        assert!(loaded.warnings.is_empty());
        drop(writer);
        remove(root);
    }

    #[test]
    fn buffered_writer_for_missing_session_is_rejected() {
        let root = temp_root("buffered-missing");
        let archive = SessionArchive::open(&root);
        let error = archive.buffered("nosuchsession").expect_err("missing");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        remove(root);
    }

    #[cfg(unix)]
    #[test]
    fn archive_files_and_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("private");
        let archive = SessionArchive::open(&root);
        let entry = archive
            .create(CreateSession::new(root.join("p")))
            .expect("create");
        archive
            .append_human(&entry.id, "private", false)
            .expect("append");
        let dir_mode = fs::metadata(root.join(&entry.id))
            .expect("dir")
            .permissions()
            .mode()
            & 0o777;
        let meta_mode = fs::metadata(root.join(&entry.id).join(META_FILE))
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        let journal_mode = fs::metadata(root.join(&entry.id).join(EVENTS_FILE))
            .expect("journal")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(meta_mode, 0o600);
        assert_eq!(journal_mode, 0o600);
        remove(root);
    }

    #[test]
    fn sync_metadata_edits_preserve_unknown_keys() {
        let root = temp_root("sync-edit");
        let archive = SessionArchive::open(&root);
        let entry = archive
            .create(
                CreateSession::new(root.join("p"))
                    .metadata(metadata(&[("agents", json!([{"identity": "claude.ai"}]))])),
            )
            .expect("create");
        archive.set_title(&entry.id, "retitled").expect("title");
        archive
            .update_metadata(&entry.id, |data| {
                data.remove("agents");
                data.insert("provider_session", json!("sid-9"));
            })
            .expect("update");
        let loaded = archive.load(&entry.id).expect("load");
        assert_eq!(loaded.entry.title, "retitled");
        assert_eq!(loaded.metadata.get("agents"), None);
        assert_eq!(
            loaded.metadata.get("provider_session"),
            Some(&json!("sid-9"))
        );
        remove(root);
    }
}
