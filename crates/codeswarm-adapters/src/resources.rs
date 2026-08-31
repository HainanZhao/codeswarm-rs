//! Safe, bounded workspace resources for prompt attachments.
//!
//! This mirrors the retired client's resource contract without coupling file
//! discovery to the terminal renderer: callers get either UTF-8 text or
//! bounded binary data, and every path is resolved beneath the workspace root.

use std::path::{Path, PathBuf};

pub const MAX_RESOURCE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resource {
    pub path: PathBuf,
    pub mime_type: String,
    pub text: Option<String>,
    pub data: Option<Vec<u8>>,
}

#[derive(Debug)]
pub enum ResourceError {
    NotRelative,
    OutsideWorkspace,
    NotFound,
    TooLarge { bytes: u64 },
    Io(std::io::Error),
}

impl std::fmt::Display for ResourceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRelative => formatter.write_str("resource path must be relative"),
            Self::OutsideWorkspace => formatter.write_str("resource path is outside workspace"),
            Self::NotFound => formatter.write_str("resource was not found"),
            Self::TooLarge { bytes } => write!(
                formatter,
                "resource is too large ({bytes} bytes; limit {MAX_RESOURCE_BYTES})"
            ),
            Self::Io(error) => write!(formatter, "resource read failed: {error}"),
        }
    }
}

impl std::error::Error for ResourceError {}

pub fn load(
    root: impl AsRef<Path>,
    relative_path: impl AsRef<Path>,
) -> Result<Resource, ResourceError> {
    let root = root.as_ref().canonicalize().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ResourceError::NotFound
        } else {
            ResourceError::Io(error)
        }
    })?;
    let relative_path = relative_path.as_ref();
    if relative_path.is_absolute() {
        return Err(ResourceError::NotRelative);
    }
    let path = root.join(relative_path).canonicalize().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ResourceError::NotFound
        } else {
            ResourceError::Io(error)
        }
    })?;
    if !path.starts_with(&root) {
        return Err(ResourceError::OutsideWorkspace);
    }
    let metadata = std::fs::metadata(&path).map_err(ResourceError::Io)?;
    if metadata.len() > MAX_RESOURCE_BYTES {
        return Err(ResourceError::TooLarge {
            bytes: metadata.len(),
        });
    }
    let bytes = std::fs::read(&path).map_err(ResourceError::Io)?;
    let mime_type = mime_type(&path);
    let (text, data) = match std::str::from_utf8(&bytes) {
        Ok(text) => (Some(text.to_owned()), None),
        Err(_) => (None, Some(bytes)),
    };
    Ok(Resource {
        path,
        mime_type,
        text,
        data,
    })
}

fn mime_type(path: &Path) -> String {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mime = match extension.as_str() {
        "txt" | "text" => "text/plain",
        "log" => "text/plain",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "csv" => "text/csv",
        "md" => "text/markdown",
        "markdown" => "text/markdown",
        "rs" => "text/rust",
        "py" => "text/x-python",
        "js" | "mjs" | "cjs" => "text/javascript",
        "ts" | "tsx" => "text/typescript",
        "c" => "text/x-c",
        "h" => "text/x-c",
        "cc" | "cpp" | "cxx" | "hpp" => "text/x-c++",
        "go" => "text/x-go",
        "java" => "text/x-java-source",
        "rb" => "text/x-ruby",
        "php" => "text/x-php",
        "sh" | "bash" | "zsh" | "fish" => "application/x-sh",
        "sql" => "application/sql",
        "toml" => "application/toml",
        "yaml" | "yml" => "application/yaml",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    };
    mime.to_owned()
}

#[cfg(test)]
mod tests {
    use super::{MAX_RESOURCE_BYTES, ResourceError, load};

    fn temp_root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "codeswarm-resource-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn reads_text_and_rejects_escape_paths() {
        let root = temp_root();
        std::fs::write(root.join("note.md"), "hello").expect("write");
        let resource = load(&root, "note.md").expect("resource");
        assert_eq!(resource.mime_type, "text/markdown");
        assert_eq!(resource.text.as_deref(), Some("hello"));
        assert!(matches!(
            load(&root, "../outside"),
            Err(ResourceError::NotFound | ResourceError::OutsideWorkspace)
        ));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn rejects_absolute_and_oversized_resources() {
        let root = temp_root();
        let absolute = root.join("note.txt");
        assert!(matches!(
            load(&root, &absolute),
            Err(ResourceError::NotRelative)
        ));
        std::fs::write(
            root.join("large.bin"),
            vec![b'x'; MAX_RESOURCE_BYTES as usize + 1],
        )
        .expect("large file");
        assert!(matches!(
            load(&root, "large.bin"),
            Err(ResourceError::TooLarge { .. })
        ));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn reports_common_text_and_source_mime_types() {
        let root = temp_root();
        std::fs::write(root.join("notes.txt"), "hello").expect("text");
        std::fs::write(root.join("run.sh"), "#!/bin/sh\n").expect("shell");
        assert_eq!(
            load(&root, "notes.txt").expect("text resource").mime_type,
            "text/plain"
        );
        assert_eq!(
            load(&root, "run.sh").expect("shell resource").mime_type,
            "application/x-sh"
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_to_outside_workspace() {
        let root = temp_root();
        let outside = root.with_extension("outside");
        std::fs::write(&outside, "secret").expect("outside");
        std::os::unix::fs::symlink(&outside, root.join("link")).expect("symlink");
        assert!(matches!(
            load(&root, "link"),
            Err(ResourceError::OutsideWorkspace)
        ));
        std::fs::remove_file(root.join("link")).expect("link cleanup");
        std::fs::remove_file(outside).expect("outside cleanup");
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
