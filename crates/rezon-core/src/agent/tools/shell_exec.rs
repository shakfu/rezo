// Run a shell command and return stdout / stderr / exit status.
// Confirmation-required by default — this is the canonical destructive
// tool, and the user should explicitly approve every command the
// model proposes.
//
// Caps:
//   - 60s wall-clock timeout (process is killed on overrun)
//   - 256 KiB total stdout/stderr cap each; oversize is truncated with a
//     flag so the model knows the output was clipped
//   - Runs via the user's shell ($SHELL or /bin/sh) so the command can
//     use pipes, redirects, globs, etc.
//
// Future polish (see TODO.md): stdin piping, cwd override, env
// pass-through, mid-run cancel via SIGTERM.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::agent::tool::{Tool, ToolContext, ToolError};

const TIMEOUT_SECS: u64 = 60;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// The timeout is a field rather than a constant so tests can drive the
/// overrun path in milliseconds instead of a minute. Production
/// construction goes through `Default`, so behaviour is unchanged.
///
/// It lives here rather than on `ToolContext` deliberately: a timeout
/// is this tool's business, and putting it on the shared context would
/// push a test-only concern into a type every tool sees.
pub struct ShellExec {
    timeout: Duration,
}

impl Default for ShellExec {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(TIMEOUT_SECS),
        }
    }
}

impl ShellExec {
    /// Test-only constructor for exercising the overrun path quickly.
    #[cfg(test)]
    fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }
}

#[async_trait]
impl Tool for ShellExec {
    fn name(&self) -> &str {
        "shell_exec"
    }

    fn description(&self) -> &str {
        "Run a shell command (via $SHELL or /bin/sh) and return its stdout, \
         stderr, and exit status. 60s timeout; output capped at 256KB per \
         stream."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command line (e.g. \"ls -la ~/projects | head\")."
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional absolute working directory. Defaults to the rezon process cwd."
                }
            },
            "required": ["command"]
        })
    }

    fn requires_confirmation(&self) -> bool {
        true
    }

    async fn dispatch(&self, args: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        #[derive(Deserialize)]
        struct Args {
            command: String,
            #[serde(default)]
            cwd: Option<String>,
        }
        let args: Args = serde_json::from_value(args)
            .map_err(|e| ToolError::Argument(format!("invalid args: {e}")))?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

        let mut cmd = Command::new(&shell);
        cmd.arg("-c").arg(&args.command);
        if let Some(dir) = &args.cwd {
            let p = std::path::Path::new(dir);
            if !p.is_absolute() {
                return Err(ToolError::Argument(format!("cwd must be absolute: {dir}")));
            }
            cmd.current_dir(p);
        }
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        // Put the child in its own process group so a timeout kill can
        // signal the whole group (the shell + everything it spawned),
        // not just the shell. Without this, a `sh -c "sleep 120"` is
        // killed at the shell layer but `sleep` is orphaned and keeps
        // running.
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Runtime(anyhow::anyhow!("spawn {shell}: {e}")))?;

        // Take pipes once, up front. read_capped borrows them mutably;
        // child.wait() borrows child mutably. Keeping the pipes outside
        // the child means we can call child.start_kill() / child.wait()
        // independently after a timeout.
        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();
        let mut stdout_truncated = false;
        let mut stderr_truncated = false;

        // Race the natural finish (all three: stdout EOF, stderr EOF, child
        // wait) against the wall-clock timeout. select! drops the losing
        // arm's future, releasing all borrows it held — so on timeout we
        // can reach back into `child` to kill it.
        let timed_out = tokio::select! {
            _ = async {
                let _ = tokio::join!(
                    read_capped(&mut stdout, &mut stdout_buf, &mut stdout_truncated),
                    read_capped(&mut stderr, &mut stderr_buf, &mut stderr_truncated),
                    child.wait(),
                );
            } => false,
            _ = tokio::time::sleep(self.timeout) => true,
        };

        if timed_out {
            // SIGKILL the entire process group so descendants of the
            // shell die too (e.g. a `sleep` started by `sh -c`). On
            // platforms without process_group support, fall back to
            // killing the immediate child.
            kill_process_tree(&mut child);
            let _ = child.wait().await;
            return Ok(json!({
                "command": args.command,
                "timedOut": true,
                "timeoutSecs": self.timeout.as_secs(),
                "stdout": String::from_utf8_lossy(&stdout_buf).into_owned(),
                "stderr": String::from_utf8_lossy(&stderr_buf).into_owned(),
                "stdoutTruncated": stdout_truncated,
                "stderrTruncated": stderr_truncated,
            }));
        }

        // Natural finish path. The wait inside select! has already been
        // resolved, but we don't capture the ExitStatus from inside join!
        // due to borrow shape — re-call wait, which is now a no-op that
        // returns the cached status.
        let status = child
            .wait()
            .await
            .map_err(|e| ToolError::Runtime(anyhow::anyhow!("wait: {e}")))?;

        Ok(json!({
            "command": args.command,
            "exitCode": status.code(),
            "success": status.success(),
            "stdout": String::from_utf8_lossy(&stdout_buf).into_owned(),
            "stderr": String::from_utf8_lossy(&stderr_buf).into_owned(),
            "stdoutTruncated": stdout_truncated,
            "stderrTruncated": stderr_truncated,
        }))
    }
}

/// SIGKILL the child's whole process group on Unix (so descendants
/// like `sleep` started by `sh -c` die too). On other platforms,
/// fall back to killing just the immediate child.
fn kill_process_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // Negative pid = kill the whole process group whose pgid
            // equals abs(pid). Safe: just an FFI syscall, no data race.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            return;
        }
    }
    let _ = child.start_kill();
}

/// Read from a pipe into `buf` until EOF or `MAX_OUTPUT_BYTES`. After the
/// cap, drains and discards remaining bytes so the child can still
/// progress (a blocked write on a full stdout pipe would otherwise stall
/// it).
async fn read_capped<R>(reader: &mut R, buf: &mut Vec<u8>, truncated: &mut bool)
where
    R: AsyncReadExt + Unpin,
{
    let mut tmp = [0u8; 8192];
    loop {
        match reader.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < MAX_OUTPUT_BYTES {
                    let take = (MAX_OUTPUT_BYTES - buf.len()).min(n);
                    buf.extend_from_slice(&tmp[..take]);
                    if take < n {
                        *truncated = true;
                    }
                } else {
                    *truncated = true;
                }
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn ctx() -> ToolContext {
        ToolContext {
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn run(cmd: &str) -> Value {
        ShellExec::default()
            .dispatch(json!({ "command": cmd }), &ctx())
            .await
            .unwrap()
    }

    async fn run_with_timeout(cmd: &str, timeout: Duration) -> Value {
        ShellExec::with_timeout(timeout)
            .dispatch(json!({ "command": cmd }), &ctx())
            .await
            .unwrap()
    }

    // ---- Basics -----------------------------------------------------

    #[tokio::test]
    async fn captures_stdout_and_exit_status() {
        let v = run("echo hello").await;
        assert_eq!(v["stdout"], "hello\n");
        assert_eq!(v["exitCode"], 0);
        assert_eq!(v["success"], true);
        assert_eq!(v["timedOut"], Value::Null);
    }

    #[tokio::test]
    async fn captures_stderr_separately_from_stdout() {
        let v = run("echo out; echo err >&2").await;
        assert_eq!(v["stdout"], "out\n");
        assert_eq!(v["stderr"], "err\n");
    }

    #[tokio::test]
    async fn nonzero_exit_is_reported_not_errored() {
        // A failing command is a result the model should see and react
        // to, not a tool error.
        let v = run("exit 3").await;
        assert_eq!(v["exitCode"], 3);
        assert_eq!(v["success"], false);
    }

    #[tokio::test]
    async fn shell_features_work() {
        // The whole reason for going through `$SHELL -c`.
        let v = run("printf 'b\\na\\n' | sort | tr -d '\\n'").await;
        assert_eq!(v["stdout"], "ab");
    }

    #[tokio::test]
    async fn stdin_is_null_so_a_reader_does_not_hang() {
        // stdin is /dev/null, so `cat` sees immediate EOF rather than
        // blocking until the timeout.
        let v = run_with_timeout("cat", Duration::from_secs(5)).await;
        assert_eq!(v["stdout"], "");
        assert_eq!(v["timedOut"], Value::Null);
    }

    #[tokio::test]
    async fn relative_cwd_is_rejected() {
        let err = ShellExec::default()
            .dispatch(json!({"command": "pwd", "cwd": "relative/dir"}), &ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must be absolute"), "{err}");
    }

    #[tokio::test]
    async fn absolute_cwd_is_honored() {
        let dir = tempfile::TempDir::new().unwrap();
        // Resolve through symlinks (/tmp is one on macOS) so the
        // comparison is against what `pwd` will actually print.
        let real = dir.path().canonicalize().unwrap();
        let v = ShellExec::default()
            .dispatch(
                json!({"command": "pwd", "cwd": real.to_str().unwrap()}),
                &ctx(),
            )
            .await
            .unwrap();
        assert_eq!(v["stdout"].as_str().unwrap().trim(), real.to_str().unwrap());
    }

    // ---- Timeout ----------------------------------------------------

    #[tokio::test]
    async fn overrun_is_killed_and_flagged() {
        let v = run_with_timeout("sleep 30", Duration::from_millis(200)).await;
        assert_eq!(v["timedOut"], true);
        // No exit status on the timeout path — the process was killed,
        // it did not finish.
        assert_eq!(v["exitCode"], Value::Null);
    }

    #[tokio::test]
    async fn partial_output_before_a_timeout_is_still_returned() {
        // Whatever the command managed to emit is useful to the model
        // even though the run was cut short.
        let v = run_with_timeout("echo early; sleep 30", Duration::from_millis(300)).await;
        assert_eq!(v["timedOut"], true);
        assert_eq!(v["stdout"], "early\n");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn timeout_kills_the_whole_process_group_not_just_the_shell() {
        // This is what `cmd.process_group(0)` plus the negative-pid
        // kill exist for. Without them the shell dies and the `sleep`
        // it spawned is orphaned and keeps running to completion.
        //
        // The grandchild writes to a file after it would have been
        // killed; if that file appears, the kill did not reach it.
        let dir = tempfile::TempDir::new().unwrap();
        let marker = dir.path().join("survived");
        let cmd = format!("(sleep 1; touch {}) & wait", marker.to_str().unwrap());

        let v = run_with_timeout(&cmd, Duration::from_millis(200)).await;
        assert_eq!(v["timedOut"], true);

        // Well past the grandchild's own sleep. If it survived the
        // kill, the marker is there by now.
        tokio::time::sleep(Duration::from_millis(1600)).await;
        assert!(
            !marker.exists(),
            "grandchild outlived the timeout kill: the process group was not signalled"
        );
    }

    // ---- Output caps ------------------------------------------------

    #[tokio::test]
    async fn oversized_stdout_is_capped_and_flagged() {
        // 512 KiB against a 256 KiB cap.
        let v = run(&format!(
            "head -c {} /dev/zero | tr '\\0' 'x'",
            MAX_OUTPUT_BYTES * 2
        ))
        .await;
        assert_eq!(v["stdoutTruncated"], true);
        assert_eq!(v["stdout"].as_str().unwrap().len(), MAX_OUTPUT_BYTES);
        assert_eq!(v["stderrTruncated"], false);
    }

    #[tokio::test]
    async fn a_command_producing_far_more_than_the_cap_still_completes() {
        // The drain half of `read_capped`: past the cap it keeps
        // reading and discarding. If it stopped reading instead, the
        // child would block on a full pipe and this would hit the
        // timeout rather than exiting cleanly.
        let v = run_with_timeout(
            &format!("head -c {} /dev/zero | tr '\\0' 'x'", MAX_OUTPUT_BYTES * 20),
            Duration::from_secs(20),
        )
        .await;
        assert_eq!(v["timedOut"], Value::Null, "child stalled on a full pipe");
        assert_eq!(v["success"], true);
        assert_eq!(v["stdoutTruncated"], true);
        assert_eq!(v["stdout"].as_str().unwrap().len(), MAX_OUTPUT_BYTES);
    }

    #[tokio::test]
    async fn stderr_is_capped_independently_of_stdout() {
        let v = run(&format!(
            "head -c {} /dev/zero | tr '\\0' 'y' >&2",
            MAX_OUTPUT_BYTES * 2
        ))
        .await;
        assert_eq!(v["stderrTruncated"], true);
        assert_eq!(v["stderr"].as_str().unwrap().len(), MAX_OUTPUT_BYTES);
        assert_eq!(v["stdoutTruncated"], false);
    }

    #[test]
    fn shell_exec_requires_confirmation() {
        // The confirmation floor keys off this. If it ever flips,
        // arbitrary command execution stops prompting.
        assert!(ShellExec::default().requires_confirmation());
    }
}
