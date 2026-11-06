use std::path::PathBuf;

pub fn find_executable(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    #[cfg(windows)]
    let candidates: Vec<String> = {
        let has_ext = std::path::Path::new(name).extension().is_some();
        if has_ext {
            vec![name.to_string()]
        } else {
            ["", ".exe", ".cmd", ".bat", ".com"]
                .iter()
                .map(|ext| format!("{name}{ext}"))
                .collect()
        }
    };
    #[cfg(not(windows))]
    let candidates: Vec<String> = vec![name.to_string()];

    for dir in std::env::split_paths(&paths) {
        for candidate in &candidates {
            let path = dir.join(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

pub fn run(
    program: &std::path::Path,
    args: &[String],
    cwd: Option<&std::path::Path>,
) -> anyhow::Result<std::process::Output> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.output()
        .map_err(|e| anyhow::anyhow!("failed to execute {}: {e}", program.display()))
}

/// Runs `program` to completion, killing it if it exceeds `timeout`.
pub fn run_with_timeout(
    program: &std::path::Path,
    args: &[String],
    cwd: Option<&std::path::Path>,
    timeout: std::time::Duration,
) -> anyhow::Result<std::process::Output> {
    use std::io::Read;
    use std::time::Instant;

    let mut child = {
        let mut cmd = std::process::Command::new(program);
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        cmd.spawn()
            .map_err(|e| anyhow::anyhow!("failed to execute {}: {e}", program.display()))?
    };

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = Vec::new();
                let mut err = Vec::new();
                if let Some(mut s) = child.stdout.take() {
                    let _ = s.read_to_end(&mut out);
                }
                if let Some(mut s) = child.stderr.take() {
                    let _ = s.read_to_end(&mut err);
                }
                return Ok(std::process::Output {
                    status,
                    stdout: out,
                    stderr: err,
                });
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("{} timed out after {:?}", program.display(), timeout);
            }
            Err(e) => anyhow::bail!("waiting on {} failed: {e}", program.display()),
        }
    }
}

/// Spawns a detached child whose stdout/stderr append to `log_path`.
/// The child must not be a shell: pass the executable and its argv directly.
pub fn spawn_detached(
    program: &std::path::Path,
    args: &[String],
    cwd: Option<&std::path::Path>,
    env: &[(String, String)],
    log_path: &std::path::Path,
) -> anyhow::Result<u32> {
    use std::os::unix::process::CommandExt;

    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| anyhow::anyhow!("cannot open log {}: {e}", log_path.display()))?;
    let log_err = log.try_clone()?;

    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err));
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }

    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    let child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to start {}: {e}", program.display()))?;
    Ok(child.id())
}
