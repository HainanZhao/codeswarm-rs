//! Versioned persistence for the Rust client.
//!
//! The first Rust implementation wrote bare [`AgentEvent`] values to JSONL.
//! The types in this module deliberately accept that format as schema zero and
//! provide migration helpers for versioned envelopes.  Session
//! metadata accepts the legacy flattened shape as well as the current envelope.
//! Metadata remains flattened so older CodeSwarm state files stay readable.

use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use serde_json::{Map, Value};

use crate::AgentEvent;

/// The current on-disk schema for Rust persistence.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;
const LEGACY_SCHEMA_VERSION: u32 = 0;

/// Persistence operations report malformed input, unsupported versions, and
/// I/O failures through this type.
#[derive(Debug)]
pub enum PersistenceError {
    Io(std::io::Error),
    Malformed { kind: &'static str, detail: String },
    UnsupportedVersion { kind: &'static str, version: u32 },
}

impl Display for PersistenceError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "persistence I/O error: {error}"),
            Self::Malformed { kind, detail } => write!(formatter, "malformed {kind}: {detail}"),
            Self::UnsupportedVersion { kind, version } => {
                write!(formatter, "unsupported {kind} schema version {version}")
            }
        }
    }
}

impl std::error::Error for PersistenceError {}

impl From<std::io::Error> for PersistenceError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Result of converting a file to the current schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationReport {
    /// `None` means that no source record/version was observed, including when
    /// the source file is missing or empty.
    pub source_version: Option<u32>,
    pub target_version: u32,
    pub records: usize,
    pub changed: bool,
}

/// Events read from a versioned log, including the source versions observed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedEvents {
    pub events: Vec<AgentEvent>,
    pub source_versions: BTreeSet<u32>,
}

/// A JSONL event log that accepts legacy bare events and version-zero envelopes.
#[derive(Clone, Debug)]
pub struct VersionedEventLog {
    path: PathBuf,
}

impl VersionedEventLog {
    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one current-schema envelope.
    pub fn append(&self, event: &AgentEvent) -> Result<(), PersistenceError> {
        let record = serde_json::json!({
            "schema_version": CURRENT_SCHEMA_VERSION,
            "event": event,
        });
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        serde_json::to_writer(&mut file, &record)
            .map_err(|error| malformed("event log", error.to_string()))?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        Ok(())
    }

    /// Read all events. Missing logs are an empty event stream.
    pub fn read(&self) -> Result<Vec<AgentEvent>, PersistenceError> {
        Ok(self.read_with_versions()?.events)
    }

    pub fn read_with_versions(&self) -> Result<LoadedEvents, PersistenceError> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LoadedEvents {
                    events: Vec::new(),
                    source_versions: BTreeSet::new(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let mut events = Vec::new();
        let mut source_versions = BTreeSet::new();
        for (line_number, result) in BufReader::new(file).lines().enumerate() {
            let line = result?;
            if line.trim().is_empty() {
                continue;
            }
            let (version, event) = parse_event_record(&line, line_number + 1)?;
            source_versions.insert(version);
            events.push(event);
        }
        Ok(LoadedEvents {
            events,
            source_versions,
        })
    }

    /// Rewrite legacy records atomically. A malformed record leaves the source
    /// untouched, allowing an operator to repair the bad log manually.
    pub fn migrate_in_place(&self) -> Result<MigrationReport, PersistenceError> {
        let loaded = self.read_with_versions()?;
        if loaded.events.is_empty() && !self.path.exists() {
            return Ok(MigrationReport {
                source_version: None,
                target_version: CURRENT_SCHEMA_VERSION,
                records: 0,
                changed: false,
            });
        }
        let source_version = loaded.source_versions.iter().copied().min();
        let changed = loaded
            .source_versions
            .iter()
            .any(|version| *version != CURRENT_SCHEMA_VERSION);
        if changed {
            atomic_write_event_log(&self.path, &loaded.events)?;
        }
        Ok(MigrationReport {
            source_version,
            target_version: CURRENT_SCHEMA_VERSION,
            records: loaded.events.len(),
            changed,
        })
    }
}

fn parse_event_record(
    line: &str,
    line_number: usize,
) -> Result<(u32, AgentEvent), PersistenceError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|error| malformed("event log", format!("line {line_number}: {error}")))?;
    let object = value.as_object().ok_or_else(|| {
        malformed(
            "event log",
            format!("line {line_number} must be a JSON object"),
        )
    })?;
    let has_envelope = object.contains_key("event")
        || object.contains_key("schema_version")
        || object.contains_key("version");
    let version = if has_envelope {
        read_version(object, "event log", line_number)?
    } else {
        LEGACY_SCHEMA_VERSION
    };
    ensure_supported(version, "event log")?;
    let event_value = object.get("event").unwrap_or(&value);
    let event = serde_json::from_value(event_value.clone())
        .map_err(|error| malformed("event log", format!("line {line_number} event: {error}")))?;
    Ok((version, event))
}

fn atomic_write_event_log(path: &Path, events: &[AgentEvent]) -> Result<(), PersistenceError> {
    let temporary = temporary_path(path);
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        for event in events {
            let record = serde_json::json!({
                "schema_version": CURRENT_SCHEMA_VERSION,
                "event": event,
            });
            serde_json::to_writer(&mut file, &record)
                .map_err(|error| malformed("event log", error.to_string()))?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Metadata imported from either a legacy plain object or the Rust envelope.
/// The map intentionally retains unknown keys for forward compatibility.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionMetadata {
    data: Map<String, Value>,
}

impl SessionMetadata {
    pub fn new(data: Map<String, Value>) -> Self {
        Self { data }
    }

    pub fn empty() -> Self {
        Self::new(Map::new())
    }

    pub fn schema_version(&self) -> u32 {
        CURRENT_SCHEMA_VERSION
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.data.get(key)
    }

    /// Replace one metadata value while retaining all other keys.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Value>) -> Option<Value> {
        self.data.insert(key.into(), value.into())
    }

    /// Remove one metadata value, returning the previous value when present.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        self.data.remove(key)
    }

    pub fn as_object(&self) -> &Map<String, Value> {
        &self.data
    }

    /// Flattened JSON keeps existing `roster`/`agent_data` keys addressable.
    pub fn to_value(&self) -> Value {
        let mut object = self.data.clone();
        object.insert("schema_version".into(), Value::from(CURRENT_SCHEMA_VERSION));
        Value::Object(object)
    }

    pub fn to_json(&self) -> Result<String, PersistenceError> {
        serde_json::to_string(&self.to_value())
            .map_err(|error| malformed("session metadata", error.to_string()))
    }
}

/// A loaded metadata value carries the version from which it was imported.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedSessionMetadata {
    pub metadata: SessionMetadata,
    pub source_version: u32,
}

impl Deref for LoadedSessionMetadata {
    type Target = SessionMetadata;

    fn deref(&self) -> &Self::Target {
        &self.metadata
    }
}

/// File-backed session metadata with schema migration.
#[derive(Clone, Debug)]
pub struct SessionMetadataStore {
    path: PathBuf,
}

impl SessionMetadataStore {
    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<Option<LoadedSessionMetadata>, PersistenceError> {
        let raw = match fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let value: Value = serde_json::from_str(&raw)
            .map_err(|error| malformed("session metadata", error.to_string()))?;
        let object = value
            .as_object()
            .ok_or_else(|| malformed("session metadata", "expected a JSON object".into()))?;
        let source_version = read_version(object, "session metadata", 0)?;
        ensure_supported(source_version, "session metadata")?;
        let data = if let Some(metadata) = object.get("metadata") {
            metadata
                .as_object()
                .ok_or_else(|| {
                    malformed(
                        "session metadata",
                        "metadata envelope must be an object".into(),
                    )
                })?
                .clone()
        } else {
            object
                .iter()
                .filter(|(key, _)| key.as_str() != "schema_version" && key.as_str() != "version")
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        };
        Ok(Some(LoadedSessionMetadata {
            metadata: SessionMetadata::new(data),
            source_version,
        }))
    }

    pub fn load(&self) -> Result<Option<LoadedSessionMetadata>, PersistenceError> {
        self.read()
    }

    /// Read the current snapshot, apply an in-memory edit, and atomically
    /// publish the replacement. A missing snapshot is treated as empty
    /// metadata; malformed or newer snapshots are left untouched and
    /// returned as errors.
    pub fn update<F>(&self, edit: F) -> Result<(), PersistenceError>
    where
        F: FnOnce(&mut SessionMetadata),
    {
        let mut metadata = self
            .read()?
            .map(|loaded| loaded.metadata)
            .unwrap_or_else(SessionMetadata::empty);
        edit(&mut metadata);
        self.write(&metadata)
    }

    pub fn write(&self, metadata: &SessionMetadata) -> Result<(), PersistenceError> {
        let json = metadata.to_json()?;
        let temporary = temporary_path(&self.path);
        let result = (|| {
            if let Some(parent) = self.path.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)?;
            }
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            file.write_all(format!("{json}\n").as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn migrate_in_place(&self) -> Result<MigrationReport, PersistenceError> {
        let Some(loaded) = self.read()? else {
            return Ok(MigrationReport {
                source_version: None,
                target_version: CURRENT_SCHEMA_VERSION,
                records: 0,
                changed: false,
            });
        };
        let changed = loaded.source_version != CURRENT_SCHEMA_VERSION
            || serde_json::from_str::<Value>(&fs::read_to_string(&self.path)?)
                .ok()
                .and_then(|value| {
                    value
                        .as_object()
                        .map(|object| object.contains_key("metadata"))
                })
                .unwrap_or(false);
        if changed {
            self.write(&loaded.metadata)?;
        }
        Ok(MigrationReport {
            source_version: Some(loaded.source_version),
            target_version: CURRENT_SCHEMA_VERSION,
            records: 1,
            changed,
        })
    }

    /// Start a background metadata writer. Runtime event loops should enqueue
    /// snapshots through this handle and call [`BufferedSessionMetadataStore::flush`]
    /// only at lifecycle boundaries; atomic writes and fsync therefore never
    /// block terminal input or rendering.
    pub fn buffered(&self) -> std::io::Result<BufferedSessionMetadataStore> {
        self.buffered_with_errors(|_| {})
    }

    pub fn buffered_with_errors(
        &self,
        on_error: impl Fn(String) + Send + 'static,
    ) -> std::io::Result<BufferedSessionMetadataStore> {
        BufferedSessionMetadataStore::open(self.path.clone(), Box::new(on_error))
    }
}

enum MetadataCommand {
    Write(SessionMetadata),
    Flush(Sender<Result<(), PersistenceError>>),
    Shutdown(Sender<()>),
}

/// Background writer for runtime session metadata snapshots.
///
/// Each queued snapshot is written in order, and every write replaces the
/// previous file atomically. The handle itself only performs channel sends;
/// filesystem work happens on its worker thread. `flush` is the explicit
/// durability boundary used before a session exits.
pub struct BufferedSessionMetadataStore {
    sender: Sender<MetadataCommand>,
    worker: Option<std::thread::JoinHandle<Result<(), PersistenceError>>>,
}

impl std::fmt::Debug for BufferedSessionMetadataStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BufferedSessionMetadataStore")
            .field("worker_running", &self.worker.is_some())
            .finish_non_exhaustive()
    }
}

impl BufferedSessionMetadataStore {
    fn open(path: PathBuf, on_error: Box<dyn Fn(String) + Send>) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("codeswarm-session-metadata".into())
            .spawn(move || metadata_worker(path, receiver, on_error))?;
        Ok(Self {
            sender,
            worker: Some(worker),
        })
    }

    /// Queue a complete metadata snapshot without doing filesystem I/O in
    /// the caller.
    pub fn write(&self, metadata: SessionMetadata) -> Result<(), PersistenceError> {
        self.sender
            .send(MetadataCommand::Write(metadata))
            .map_err(|_| {
                PersistenceError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "session metadata writer stopped",
                ))
            })
    }

    /// Drain queued snapshots and wait until the latest one is durable.
    pub fn flush(&self) -> Result<(), PersistenceError> {
        let (reply, result) = mpsc::channel();
        self.sender
            .send(MetadataCommand::Flush(reply))
            .map_err(|_| {
                PersistenceError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "session metadata writer stopped",
                ))
            })?;
        result.recv().map_err(|_| {
            PersistenceError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "session metadata writer stopped",
            ))
        })?
    }
}

impl Drop for BufferedSessionMetadataStore {
    fn drop(&mut self) {
        let Some(worker) = self.worker.take() else {
            return;
        };
        let (reply, result) = mpsc::channel();
        if self.sender.send(MetadataCommand::Shutdown(reply)).is_ok() {
            let _ = result.recv();
        }
        let _ = worker.join();
    }
}

fn metadata_worker(
    path: PathBuf,
    receiver: Receiver<MetadataCommand>,
    on_error: Box<dyn Fn(String) + Send>,
) -> Result<(), PersistenceError> {
    let store = SessionMetadataStore::open(path);
    let mut first_error = None;
    let mut pending = None;
    loop {
        let command = if pending.is_some() {
            match receiver.recv_timeout(std::time::Duration::from_secs(1)) {
                Ok(command) => command,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(metadata) = &pending
                        && store.write(metadata).is_ok()
                    {
                        pending = None;
                        first_error = None;
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match receiver.recv() {
                Ok(command) => command,
                Err(_) => break,
            }
        };
        match command {
            MetadataCommand::Write(metadata) => match store.write(&metadata) {
                Ok(()) => {
                    pending = None;
                    first_error = None;
                }
                Err(error) => {
                    if first_error.is_none() {
                        on_error(error.to_string());
                    }
                    pending = Some(metadata);
                    first_error = Some(error);
                }
            },
            MetadataCommand::Flush(reply) => {
                if let Some(metadata) = &pending {
                    match store.write(metadata) {
                        Ok(()) => {
                            pending = None;
                            first_error = None;
                        }
                        Err(error) => {
                            first_error = Some(error);
                        }
                    }
                }
                let _ = reply.send(match first_error.take() {
                    Some(error) => Err(error),
                    None => Ok(()),
                });
            }
            MetadataCommand::Shutdown(reply) => {
                if let Some(metadata) = &pending {
                    first_error = store.write(metadata).err();
                }
                let result = first_error.take();
                let _ = reply.send(());
                return result.map_or(Ok(()), Err);
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn read_version(
    object: &Map<String, Value>,
    kind: &'static str,
    line_number: usize,
) -> Result<u32, PersistenceError> {
    let schema = object
        .get("schema_version")
        .or_else(|| object.get("version"));
    let Some(schema) = schema else {
        return Ok(LEGACY_SCHEMA_VERSION);
    };
    let version = schema.as_u64().ok_or_else(|| {
        malformed(
            kind,
            if line_number == 0 {
                "schema_version must be an unsigned integer".into()
            } else {
                format!("line {line_number} schema_version must be an unsigned integer")
            },
        )
    })?;
    u32::try_from(version).map_err(|_| malformed(kind, "schema_version is too large".into()))
}

fn ensure_supported(version: u32, kind: &'static str) -> Result<(), PersistenceError> {
    if version > CURRENT_SCHEMA_VERSION {
        return Err(PersistenceError::UnsupportedVersion { kind, version });
    }
    Ok(())
}

fn malformed(kind: &'static str, detail: String) -> PersistenceError {
    PersistenceError::Malformed { kind, detail }
}

fn temporary_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("data");
    path.with_file_name(format!(".{file_name}.migration-{}.tmp", std::process::id()))
}

/// Convert a legacy session metadata blob to the current Rust metadata value.
pub fn import_legacy_session_metadata(
    value: Option<&str>,
) -> Result<Option<LoadedSessionMetadata>, PersistenceError> {
    let Some(value) = value else { return Ok(None) };
    let parsed: Value = serde_json::from_str(value)
        .map_err(|error| malformed("session metadata", error.to_string()))?;
    let object = parsed
        .as_object()
        .ok_or_else(|| malformed("session metadata", "expected a JSON object".into()))?;
    let source_version = read_version(object, "session metadata", 0)?;
    ensure_supported(source_version, "session metadata")?;
    let data = object
        .iter()
        .filter(|(key, _)| key.as_str() != "schema_version" && key.as_str() != "version")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    Ok(Some(LoadedSessionMetadata {
        metadata: SessionMetadata::new(data),
        source_version,
    }))
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::{Value, json};

    use super::{
        CURRENT_SCHEMA_VERSION, PersistenceError, SessionMetadata, SessionMetadataStore,
        VersionedEventLog, import_legacy_session_metadata,
    };
    use crate::AgentEvent;

    fn temp_path(suffix: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("codeswarm-persistence-{unique}-{suffix}"))
    }

    fn event() -> AgentEvent {
        AgentEvent::Text {
            slot: 0,
            text: "hello".into(),
        }
    }

    #[test]
    fn missing_event_log_and_metadata_are_empty() {
        let event_log = VersionedEventLog::open(temp_path("events.jsonl"));
        assert!(event_log.read().expect("missing log").is_empty());
        let metadata = SessionMetadataStore::open(temp_path("metadata.json"));
        assert!(metadata.read().expect("missing metadata").is_none());
        assert!(
            !metadata
                .migrate_in_place()
                .expect("missing migration")
                .changed
        );
    }

    #[test]
    fn malformed_data_is_rejected_without_rewriting_source() {
        let event_path = temp_path("malformed-events.jsonl");
        std::fs::write(&event_path, "not-json\n").expect("write");
        let event_log = VersionedEventLog::open(&event_path);
        assert!(matches!(
            event_log.read(),
            Err(PersistenceError::Malformed { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&event_path).expect("read"),
            "not-json\n"
        );
        std::fs::remove_file(event_path).expect("cleanup");

        let metadata_path = temp_path("malformed-metadata.json");
        std::fs::write(&metadata_path, "[]").expect("write");
        let metadata = SessionMetadataStore::open(&metadata_path);
        assert!(matches!(
            metadata.read(),
            Err(PersistenceError::Malformed { .. })
        ));
        assert_eq!(std::fs::read_to_string(&metadata_path).expect("read"), "[]");
        std::fs::remove_file(metadata_path).expect("cleanup");
    }

    #[test]
    fn old_bare_event_log_migrates_to_current_envelope() {
        let path = temp_path("old-events.jsonl");
        std::fs::write(
            &path,
            serde_json::to_string(&event()).expect("event") + "\n",
        )
        .expect("write");
        let log = VersionedEventLog::open(&path);
        let report = log.migrate_in_place().expect("migrate");
        assert_eq!(report.source_version, Some(0));
        assert!(report.changed);
        assert_eq!(log.read().expect("read"), vec![event()]);
        let migrated = std::fs::read_to_string(&path).expect("read raw");
        assert!(migrated.contains(&format!("\"schema_version\":{CURRENT_SCHEMA_VERSION}")));
        std::fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn old_metadata_migrates_and_preserves_unknown_keys() {
        let path = temp_path("old-metadata.json");
        std::fs::write(
            &path,
            r#"{"roster":["openai.com"],"agent_data":{"name":"Codex"}}"#,
        )
        .expect("write");
        let store = SessionMetadataStore::open(&path);
        let loaded = store.read().expect("read").expect("metadata");
        assert_eq!(loaded.source_version, 0);
        assert_eq!(loaded.get("roster"), Some(&json!(["openai.com"])));
        let report = store.migrate_in_place().expect("migrate");
        assert!(report.changed);
        let migrated = std::fs::read_to_string(&path).expect("read");
        assert!(migrated.contains("\"roster\""));
        assert!(migrated.contains("\"schema_version\":1"));
        std::fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn current_event_and_metadata_data_is_not_rewritten() {
        let event_path = temp_path("current-events.jsonl");
        let log = VersionedEventLog::open(&event_path);
        log.append(&event()).expect("append");
        let before = std::fs::read_to_string(&event_path).expect("read");
        let report = log.migrate_in_place().expect("migrate");
        assert_eq!(report.source_version, Some(1));
        assert!(!report.changed);
        assert_eq!(std::fs::read_to_string(&event_path).expect("read"), before);
        std::fs::remove_file(event_path).expect("cleanup");

        let metadata_path = temp_path("current-metadata.json");
        let store = SessionMetadataStore::open(&metadata_path);
        let mut data = serde_json::Map::new();
        data.insert("roster".into(), json!(["agy"]));
        store.write(&SessionMetadata::new(data)).expect("write");
        let report = store.migrate_in_place().expect("migrate");
        assert_eq!(report.source_version, Some(1));
        assert!(!report.changed);
        std::fs::remove_file(metadata_path).expect("cleanup");
    }

    #[test]
    fn metadata_write_creates_parent_and_replaces_previous_snapshot() {
        let path = temp_path("nested").join("session.json");
        let store = SessionMetadataStore::open(&path);

        let mut first = serde_json::Map::new();
        first.insert("owner".into(), json!("Claude"));
        store
            .write(&SessionMetadata::new(first))
            .expect("first write");

        let mut second = serde_json::Map::new();
        second.insert("owner".into(), json!("Codex"));
        second.insert("roster".into(), json!(["openai.com", "claude.ai"]));
        store
            .write(&SessionMetadata::new(second))
            .expect("replacement write");

        let loaded = store.read().expect("read").expect("snapshot");
        assert_eq!(loaded.source_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(loaded.get("owner"), Some(&json!("Codex")));
        assert_eq!(
            loaded.get("roster"),
            Some(&json!(["openai.com", "claude.ai"]))
        );
        std::fs::remove_dir_all(path.parent().expect("parent")).expect("cleanup");
    }

    #[test]
    fn metadata_update_merges_current_values_and_creates_missing_snapshot() {
        let path = temp_path("update").join("session.json");
        let store = SessionMetadataStore::open(&path);
        store
            .update(|metadata| {
                metadata.insert("roster", json!(["claude.ai"]));
            })
            .expect("create snapshot");
        store
            .update(|metadata| {
                metadata.insert("owner", json!("Claude"));
                metadata.remove("roster");
            })
            .expect("merge snapshot");
        let loaded = store.read().expect("read").expect("snapshot");
        assert_eq!(loaded.get("owner"), Some(&json!("Claude")));
        assert_eq!(loaded.get("roster"), None);
        std::fs::remove_dir_all(path.parent().expect("parent")).expect("cleanup");
    }

    #[test]
    fn failed_metadata_snapshot_is_retained_and_newer_writes_recover() {
        let root = temp_path("recovery");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("metadata");
        std::fs::create_dir(&path).unwrap(); // A directory cannot be replaced by a JSON file.
        let (sender, errors) = std::sync::mpsc::channel();
        let store = SessionMetadataStore::open(&path);
        let writer = store
            .buffered_with_errors(move |error| {
                let _ = sender.send(error);
            })
            .unwrap();
        let snapshot = |value| {
            SessionMetadata::new(serde_json::Map::from_iter([("value".into(), json!(value))]))
        };
        writer.write(snapshot(1)).unwrap();
        assert!(
            errors
                .recv_timeout(std::time::Duration::from_secs(2))
                .is_ok()
        );
        std::fs::remove_dir(&path).unwrap();
        writer.write(snapshot(2)).unwrap();
        writer.flush().unwrap();
        assert_eq!(
            store.read().unwrap().unwrap().metadata.get("value"),
            Some(&json!(2))
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        writer.write(snapshot(3)).unwrap();
        assert!(
            errors
                .recv_timeout(std::time::Duration::from_secs(2))
                .is_ok()
        );
        std::fs::remove_dir(&path).unwrap();
        writer.flush().unwrap(); // Retries the retained snapshot without another write.
        assert_eq!(
            store.read().unwrap().unwrap().metadata.get("value"),
            Some(&json!(3))
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        writer.write(snapshot(4)).unwrap();
        assert!(
            errors
                .recv_timeout(std::time::Duration::from_secs(2))
                .is_ok()
        );
        std::fs::remove_dir(&path).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !path.is_file() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            store.read().unwrap().unwrap().metadata.get("value"),
            Some(&json!(4))
        );
        drop(writer);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn buffered_metadata_writes_are_durable_at_flush_and_drop() {
        let path = temp_path("buffered").join("session.json");
        let store = SessionMetadataStore::open(&path);
        let writer = store.buffered().expect("writer");
        let mut metadata = SessionMetadata::empty();
        metadata.insert("roster", json!(["claude.ai", "openai.com"]));
        writer.write(metadata).expect("queue snapshot");
        writer.flush().expect("flush snapshot");
        let loaded = store.read().expect("read").expect("snapshot");
        assert_eq!(
            loaded.get("roster"),
            Some(&json!(["claude.ai", "openai.com"]))
        );
        drop(writer);
        std::fs::remove_dir_all(path.parent().expect("parent")).expect("cleanup");
    }

    #[test]
    fn legacy_import_accepts_missing_and_current_metadata() {
        assert!(
            import_legacy_session_metadata(None)
                .expect("missing")
                .is_none()
        );
        let loaded = import_legacy_session_metadata(Some(
            r#"{"schema_version":1,"roster":["agy"],"agent_data":{"name":"Agy"}}"#,
        ))
        .expect("current")
        .expect("metadata");
        assert_eq!(loaded.source_version, 1);
        assert_eq!(
            loaded
                .get("agent_data")
                .and_then(Value::as_object)
                .and_then(|m| m.get("name"))
                .and_then(Value::as_str),
            Some("Agy")
        );
    }

    #[test]
    fn future_versions_are_rejected() {
        let event_path = temp_path("future-events.jsonl");
        std::fs::write(
            &event_path,
            serde_json::to_string(&json!({"schema_version": 99, "event": event()})).expect("json"),
        )
        .expect("write");
        assert!(matches!(
            VersionedEventLog::open(&event_path).read(),
            Err(PersistenceError::UnsupportedVersion { version: 99, .. })
        ));
        std::fs::remove_file(event_path).expect("cleanup");
    }
}
