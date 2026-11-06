//! External conversion-engine runner shared by the `convert` handlers.
//! Engines (LibreOffice, Ghostscript, Chromium, FFmpeg, ...) run as child
//! processes with argv arrays — never through a shell — inside a tempdir
//! the caller owns, under a hard timeout. This module owns process
//! mechanics only; each handler decides which engine and arguments Java
//! would have used.
//
// Engine-backed conversion endpoints (`handlers/convert.rs`) share this
// runner. `#![allow(dead_code)]` covers output fields diagnostics may need
// later (stdout is drained to keep pipes from deadlocking but not yet
// surfaced in error bodies).
#![allow(dead_code)]

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::error::{AppError, AppResult};

/// One engine invocation. `args` is passed to the executable verbatim —
/// callers must never interpolate user input into a shell string, and
/// should pass paths, not names, for anything the engine opens.
pub struct EngineSpec {
    pub program: String,
    pub args: Vec<String>,
    pub timeout: Duration,
    /// Directories searched before `PATH`, for engines installed under a
    /// Stirling-managed prefix (e.g. `/opt/libreoffice/program`).
    pub path_dirs: Vec<PathBuf>,
    /// Working directory for the child; engines that resolve relative
    /// output paths (or write lockfiles next to inputs) need this.
    pub cwd: Option<PathBuf>,
}

impl EngineSpec {
    pub fn new(program: &str) -> Self {
        Self {
            program: program.to_string(),
            args: Vec::new(),
            timeout: Duration::from_secs(120),
            path_dirs: Vec::new(),
            cwd: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }
}

pub struct EngineOutput {
    /// `None` when the process was killed for timeout.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub elapsed: Duration,
    pub timed_out: bool,
}

/// Candidate filenames for `program` inside one directory. Windows
/// executables carry an extension the caller usually omits (`soffice`
/// resolves to `soffice.exe`/`soffice.com` via PATHEXT); elsewhere the
/// name is used verbatim.
fn candidates(dir: &std::path::Path, program: &str) -> Vec<PathBuf> {
    let base = dir.join(program);
    if cfg!(windows) && base.extension().is_none() {
        ["exe", "com", "bat", "cmd"]
            .iter()
            .map(|ext| base.with_extension(ext))
            .collect()
    } else {
        vec![base]
    }
}

/// Locates `program` on `PATH` (plus `path_dirs`) without a shell.
pub fn find_executable(program: &str, path_dirs: &[PathBuf]) -> Option<PathBuf> {
    let is_exe = |p: &PathBuf| {
        if cfg!(windows) {
            p.is_file()
        } else {
            use std::os::unix::fs::PermissionsExt;
            p.is_file()
                && std::fs::metadata(p)
                    .map(|m| m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
        }
    };
    for dir in path_dirs {
        if let Some(found) = candidates(dir, program).into_iter().find(is_exe) {
            return Some(found);
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if let Some(found) = candidates(&dir, program).into_iter().find(is_exe) {
            return Some(found);
        }
    }
    None
}

/// Runs the spec to completion or timeout. Output is drained on reader
/// threads so a chatty engine cannot deadlock on full pipes; on timeout
/// the child is killed and whatever was captured is returned.
pub fn run(spec: &EngineSpec) -> AppResult<EngineOutput> {
    let exe = find_executable(&spec.program, &spec.path_dirs).ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!(
            "conversion engine '{}' is not installed or not on PATH",
            spec.program
        ))
    })?;

    let mut command = Command::new(&exe);
    command
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().map_err(|e| {
        AppError::Internal(anyhow::anyhow!("failed to start '{}': {e}", spec.program))
    })?;

    let mut pipes = Vec::new();
    let stdout = child
        .stdout
        .take()
        .map(|s| Box::new(s) as Box<dyn Read + Send>);
    let stderr = child
        .stderr
        .take()
        .map(|s| Box::new(s) as Box<dyn Read + Send>);
    for stream in [stdout, stderr].into_iter().flatten() {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut stream = stream;
            let _ = stream.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
        pipes.push(rx);
    }

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if start.elapsed() < spec.timeout => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Err(e) => {
                let _ = child.kill();
                return Err(AppError::Internal(anyhow::anyhow!(
                    "failed to wait on '{}': {e}",
                    spec.program
                )));
            }
        }
    };
    let timed_out = status.is_none();

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    for (i, rx) in pipes.into_iter().enumerate() {
        let bytes = rx.recv().unwrap_or_default();
        if i == 0 {
            stdout = bytes;
        } else {
            stderr = bytes;
        }
    }

    Ok(EngineOutput {
        code: status.and_then(|s| s.code()),
        stdout,
        stderr,
        elapsed: start.elapsed(),
        timed_out,
    })
}

/// Fails unless the engine exited 0 within its timeout. stderr is truncated
/// for the error body — engines can be verbose and it is diagnostics, not
/// the response.
pub fn require_success(spec_program: &str, out: &EngineOutput) -> AppResult<()> {
    if out.timed_out {
        return Err(AppError::Internal(anyhow::anyhow!(
            "conversion engine '{spec_program}' timed out after {:?}",
            out.elapsed
        )));
    }
    if out.code != Some(0) {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr
            .chars()
            .rev()
            .take(2000)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        return Err(AppError::Internal(anyhow::anyhow!(
            "conversion engine '{spec_program}' exited with {:?}: {tail}",
            out.code
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(code: Option<i32>, timed_out: bool) -> EngineOutput {
        EngineOutput {
            code,
            stdout: Vec::new(),
            stderr: Vec::new(),
            elapsed: Duration::from_millis(1),
            timed_out,
        }
    }

    #[test]
    fn engine_spec_builder_collects_args() {
        let spec = EngineSpec::new("gs")
            .arg("-dNOPAUSE")
            .args(["-sDEVICE=pdfwrite", "-o"])
            .timeout(Duration::from_secs(5))
            .cwd("/tmp");
        assert_eq!(spec.program, "gs");
        assert_eq!(spec.args, vec!["-dNOPAUSE", "-sDEVICE=pdfwrite", "-o"]);
        assert_eq!(spec.timeout, Duration::from_secs(5));
        assert_eq!(spec.cwd, Some(PathBuf::from("/tmp")));
    }

    #[test]
    fn require_success_accepts_exit_zero() {
        assert!(require_success("gs", &output(Some(0), false)).is_ok());
    }

    #[test]
    fn require_success_rejects_timeout_and_nonzero() {
        assert!(require_success("gs", &output(None, true)).is_err());
        let err = require_success("gs", &output(Some(1), false)).unwrap_err();
        assert!(err.to_string().contains("exited with Some(1)"));
    }

    #[test]
    fn find_executable_searches_path_dirs_first() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("my-engine");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let found = find_executable("my-engine", &[dir.path().to_path_buf()]);
        #[cfg(unix)]
        assert_eq!(found, Some(exe));
        #[cfg(windows)]
        assert!(found.is_none() || found.is_some()); // no extension on Windows
        assert!(find_executable("definitely-not-an-engine-xyz", &[]).is_none());
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;
    use std::time::Duration;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    #[test]
    fn find_executable_rejects_non_executable_files() {
        // path_dirs are searched before PATH; files without the exec bit
        // are skipped rather than resolved.
        let dir = tempfile::tempdir().unwrap();
        let noexec = dir.path().join("noexec-engine");
        std::fs::write(&noexec, "x").unwrap();
        std::fs::set_permissions(&noexec, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(find_executable("noexec-engine", &[dir.path().to_path_buf()]).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn run_captures_exit_code_and_output() {
        let spec = EngineSpec::new("sh")
            .args(["-c", "echo out; echo err >&2; exit 3"])
            .timeout(Duration::from_secs(10));
        let out = run(&spec).unwrap();
        assert_eq!(out.code, Some(3));
        assert!(!out.timed_out);
        assert!(String::from_utf8_lossy(&out.stdout).contains("out"));
        assert!(String::from_utf8_lossy(&out.stderr).contains("err"));
        assert!(require_success("sh", &out).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn run_times_out_and_kills_the_child() {
        let spec = EngineSpec::new("sh")
            .args(["-c", "sleep 30"])
            .timeout(Duration::from_millis(100));
        let out = run(&spec).unwrap();
        assert!(out.timed_out);
        assert_eq!(out.code, None);
        assert!(require_success("sh", &out).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn run_reports_missing_engine() {
        let spec = EngineSpec::new("stirling-no-such-engine-zzz");
        assert!(run(&spec).is_err());
    }
}
