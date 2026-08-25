// Read a file from disk and return its contents as text. Defaults
// to confirmation-required: even though the action is read-only, the
// model picks the path and the user has more context about whether a
// given path is safe to expose.
//
// Caps:
//   - path must be absolute (no implicit working-directory traversal)
//   - must be a regular file. A FIFO, character device, or socket
//     either never reaches EOF (`/dev/zero`, an unwritten pipe) or
//     blocks indefinitely, and a tool that hangs forever is worse than
//     one that refuses.
//   - body capped at 256 KiB, enforced *during* the read. Reading the
//     whole file first and slicing afterwards means a multi-gigabyte
//     file is fully resident before the cap ever applies.

use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::fs;
use tokio::io::AsyncReadExt;

use crate::agent::tool::{Tool, ToolContext, ToolError};

const MAX_BYTES: usize = 256 * 1024;

pub struct FileRead;

#[async_trait]
impl Tool for FileRead {
    fn name(&self) -> &str {
        "file_read"
    }

    fn description(&self) -> &str {
        "Read a UTF-8 text file from an absolute path. Returns up to 256KB; \
         larger files are truncated and a `truncated: true` flag is set."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute file path (e.g. /Users/me/notes.txt)."
                }
            },
            "required": ["path"]
        })
    }

    fn requires_confirmation(&self) -> bool {
        true
    }

    async fn dispatch(&self, args: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
        }
        let args: Args = serde_json::from_value(args)
            .map_err(|e| ToolError::Argument(format!("invalid args: {e}")))?;
        let path = Path::new(&args.path);
        if !path.is_absolute() {
            return Err(ToolError::Argument(format!(
                "path must be absolute: {}",
                args.path
            )));
        }

        // `metadata` follows symlinks, which is intended: a symlink to
        // a regular file is fine to read. What is rejected is the
        // final target not being a regular file.
        let meta = fs::metadata(path)
            .await
            .map_err(|e| ToolError::Runtime(anyhow::anyhow!("stat {}: {e}", args.path)))?;
        if meta.is_dir() {
            return Err(ToolError::Argument(format!(
                "path is a directory: {}",
                args.path
            )));
        }
        if !meta.is_file() {
            return Err(ToolError::Argument(format!(
                "not a regular file: {}",
                args.path
            )));
        }

        // Read one byte past the cap so "exactly at the cap" and
        // "larger than the cap" are distinguishable without reading the
        // rest of the file.
        let mut file = fs::File::open(path)
            .await
            .map_err(|e| ToolError::Runtime(anyhow::anyhow!("read {}: {e}", args.path)))?;
        let mut bytes = Vec::new();
        (&mut file)
            .take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|e| ToolError::Runtime(anyhow::anyhow!("read {}: {e}", args.path)))?;

        let truncated = bytes.len() > MAX_BYTES;
        bytes.truncate(MAX_BYTES);
        let content = String::from_utf8_lossy(&bytes).into_owned();
        Ok(json!({
            "path": args.path,
            // Bytes returned, not the file's total length. The old
            // field reported the full size because the whole file had
            // already been read; obtaining it now would defeat the cap.
            // `metadata` carries the real length for callers that care.
            "size": bytes.len(),
            "fileSize": meta.len(),
            "truncated": truncated,
            "content": content,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn ctx() -> ToolContext {
        ToolContext {
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn read(path: &str) -> Result<Value, ToolError> {
        FileRead.dispatch(json!({ "path": path }), &ctx()).await
    }

    #[tokio::test]
    async fn rejects_relative_paths() {
        let err = read("notes.txt").await.unwrap_err();
        assert!(err.to_string().contains("must be absolute"), "{err}");
    }

    #[tokio::test]
    async fn reads_a_small_file_whole() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "hello").unwrap();

        let v = read(p.to_str().unwrap()).await.unwrap();
        assert_eq!(v["content"], "hello");
        assert_eq!(v["size"], 5);
        assert_eq!(v["fileSize"], 5);
        assert_eq!(v["truncated"], false);
    }

    #[tokio::test]
    async fn oversized_file_is_capped_and_flagged() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("big.txt");
        std::fs::write(&p, vec![b'x'; MAX_BYTES * 2]).unwrap();

        let v = read(p.to_str().unwrap()).await.unwrap();
        assert_eq!(v["truncated"], true);
        assert_eq!(v["size"], MAX_BYTES);
        assert_eq!(v["content"].as_str().unwrap().len(), MAX_BYTES);
        // The real length still gets reported, just not by reading it.
        assert_eq!(v["fileSize"], (MAX_BYTES * 2) as u64);
    }

    #[tokio::test]
    async fn file_exactly_at_the_cap_is_not_flagged_truncated() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("exact.txt");
        std::fs::write(&p, vec![b'x'; MAX_BYTES]).unwrap();

        let v = read(p.to_str().unwrap()).await.unwrap();
        assert_eq!(v["size"], MAX_BYTES);
        assert_eq!(v["truncated"], false);
    }

    #[tokio::test]
    async fn rejects_a_directory() {
        let dir = TempDir::new().unwrap();
        let err = read(dir.path().to_str().unwrap()).await.unwrap_err();
        assert!(err.to_string().contains("is a directory"), "{err}");
    }

    #[tokio::test]
    async fn missing_file_is_a_stat_error() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("nope.txt");
        let err = read(p.to_str().unwrap()).await.unwrap_err();
        assert!(err.to_string().contains("stat"), "{err}");
    }

    #[tokio::test]
    async fn follows_a_symlink_to_a_regular_file() {
        // Symlinks to regular files stay readable; only the final
        // target's type is checked.
        #[cfg(unix)]
        {
            let dir = TempDir::new().unwrap();
            let target = dir.path().join("real.txt");
            std::fs::write(&target, "via link").unwrap();
            let link = dir.path().join("link.txt");
            std::os::unix::fs::symlink(&target, &link).unwrap();

            let v = read(link.to_str().unwrap()).await.unwrap();
            assert_eq!(v["content"], "via link");
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn rejects_a_fifo_instead_of_hanging_on_it() {
        // The pre-fix `fs::read` would block here forever: a FIFO with
        // no writer never reaches EOF, and the tool has no timeout.
        let dir = TempDir::new().unwrap();
        let fifo = dir.path().join("pipe");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // Safe: mknod on a path inside a fresh temp dir.
        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "mkfifo failed");

        let err = read(fifo.to_str().unwrap()).await.unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn rejects_a_character_device_instead_of_reading_it_forever() {
        // /dev/zero is infinite. Capping the read would bound it, but
        // returning 256 KiB of NULs is not a useful answer either.
        let err = read("/dev/zero").await.unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
    }
}
