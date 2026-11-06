use std::path::{Path, PathBuf};

/// Walks up from `start` looking for a directory containing `.git` or a
/// `rust/Cargo.toml`; falls back to the current directory.
pub fn repo_root(start: &Path) -> PathBuf {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() || dir.join("rust/Cargo.toml").is_file() {
            return dir;
        }
        if !dir.pop() {
            return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        }
    }
}

pub fn rust_dir(root: &Path) -> PathBuf {
    root.join("rust")
}

/// `.run/` bookkeeping directory used by `stirling start/stop/status/logs`,
/// replacing `scripts/run.sh`.
pub fn run_dir(root: &Path) -> PathBuf {
    root.join(".run")
}

pub fn log_dir(root: &Path) -> PathBuf {
    run_dir(root).join("logs")
}

pub fn pid_file(root: &Path, name: &str) -> PathBuf {
    run_dir(root).join(format!("{name}.pid"))
}

pub fn component_names(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(run_dir(root)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(".pid"))
                .map(str::to_string)
        })
        .collect();
    names.sort();
    names
}

pub fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub fn write_pid(path: &Path, pid: u32) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, pid.to_string())
        .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", path.display()))
}

pub fn remove_pid(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Last `lines` lines of `path`, newest last.
pub fn tail(path: &Path, lines: usize) -> anyhow::Result<String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    let all: Vec<&str> = content.lines().collect();
    let start = all.len().saturating_sub(lines);
    Ok(all[start..].join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_returns_last_lines() {
        let dir = std::env::temp_dir().join(format!("stirling-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("tail.log");
        std::fs::write(&file, "a\nb\nc\nd\n").unwrap();
        assert_eq!(tail(&file, 2).unwrap(), "c\nd");
        assert_eq!(tail(&file, 10).unwrap(), "a\nb\nc\nd");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pid_roundtrip() {
        let dir = std::env::temp_dir().join(format!("stirling-cli-pid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("x.pid");
        write_pid(&file, 4321).unwrap();
        assert_eq!(read_pid(&file), Some(4321));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
