//! Small, atomic JSON settings store.
//!
//! Settings are user-edited state rather than a database.  Updates preserve
//! unknown keys, replace malformed intermediate objects in the same way as
//! the legacy settings layer, and use a same-directory create/sync/rename so
//! an interrupted write cannot leave a half-written config file.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Read a settings object. Missing files are treated as empty settings.
pub fn read_object(path: impl AsRef<Path>) -> io::Result<Map<String, Value>> {
    let path = path.as_ref();
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => return Err(error),
    };
    let value = serde_json::from_str::<Value>(&raw).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("settings file is not valid JSON: {error}"),
        )
    })?;
    value.as_object().cloned().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "settings file must contain a JSON object",
        )
    })
}

/// Update a settings object and commit it atomically.
pub fn update<F>(path: impl AsRef<Path>, edit: F) -> io::Result<()>
where
    F: FnOnce(&mut Map<String, Value>),
{
    let path = path.as_ref();
    let settings = read_object(path)?;
    let mut updated = settings;
    edit(&mut updated);
    atomic_write(path, &Value::Object(updated))
}

/// Write one JSON value via a private same-directory temporary file.
pub fn atomic_write(path: &Path, value: &Value) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "settings path has no parent directory",
        ));
    };
    fs::create_dir_all(parent)?;
    set_private_directory(parent)?;

    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = PathBuf::from(format!(
        ".{}.tmp.{}.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("settings"),
        std::process::id(),
        counter
    ));
    let temporary = parent.join(temporary);
    let result = (|| {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        let mut file = options.open(&temporary)?;
        set_private_file(&file)?;
        let encoded = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
        file.write_all(&encoded)?;
        file.write_all(b"\n")?;
        file.sync_all()?;

        // Replacing a user's existing settings keeps its explicit mode. New
        // files were created 0600 above.
        #[cfg(unix)]
        if let Ok(metadata) = fs::metadata(path) {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(
                metadata.permissions().mode() & 0o777,
            ))?;
            file.sync_all()?;
        }
        drop(file);
        fs::rename(&temporary, path)?;
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn set_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn set_private_file(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::{read_object, update};

    #[test]
    fn update_preserves_unknown_values_and_repairs_intermediate_objects() {
        let directory = tempfile_directory();
        let path = directory.join("codeswarm.json");
        fs::write(
            &path,
            r#"{"other":{"keep":true},"ui":"legacy","broken":42}"#,
        )
        .expect("write");
        update(&path, |settings| {
            let ui = settings
                .entry("ui")
                .or_insert_with(|| serde_json::json!({}));
            if !ui.is_object() {
                *ui = serde_json::json!({});
            }
            ui["follow_output"] = serde_json::Value::Bool(true);
        })
        .expect("update");
        let value = read_object(&path).expect("read");
        assert_eq!(value["other"]["keep"], true);
        assert_eq!(value["ui"]["follow_output"], true);
        assert_eq!(value["broken"], 42);
        // The fixture starts with the platform's normal 0644 mode; atomic
        // replacement must preserve an existing file's explicit mode.
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o644
        );
        assert!(
            fs::read_dir(&directory)
                .expect("directory")
                .all(|entry| entry.expect("entry").file_name() != ".codeswarm.json.tmp")
        );
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn malformed_settings_are_not_overwritten() {
        let directory = tempfile_directory();
        let path = directory.join("codeswarm.json");
        fs::write(&path, "not json").expect("write");
        assert!(update(&path, |_| {}).is_err());
        assert_eq!(fs::read_to_string(&path).expect("read"), "not json");
        fs::remove_dir_all(directory).expect("cleanup");
    }

    fn tempfile_directory() -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "codeswarm-settings-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("directory");
        path
    }
}
