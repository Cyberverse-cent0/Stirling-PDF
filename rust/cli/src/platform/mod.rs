use std::path::PathBuf;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
compile_error!("stirling CLI supports Linux, Windows and macOS only");

pub fn os_name() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "macos"
    }
}

pub fn arch_name() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        "other"
    }
}

/// User-level config directory: `$XDG_CONFIG_HOME` or `~/.config` on Linux,
/// `%APPDATA%` on Windows, `~/Library/Application Support` on macOS.
pub fn user_config_dir() -> anyhow::Result<PathBuf> {
    platform_user_config_dir()
}

/// User-level data directory: `$XDG_DATA_HOME` or `~/.local/share` on Linux,
/// `%LOCALAPPDATA%` on Windows, `~/Library/Application Support` on macOS.
pub fn user_data_dir() -> anyhow::Result<PathBuf> {
    platform_user_data_dir()
}

/// Directory holding per-user service definitions for this platform.
pub fn user_service_dir() -> anyhow::Result<PathBuf> {
    platform_user_service_dir()
}

/// Whether a PID refers to a live process. Used for `.run/*.pid` bookkeeping.
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
}

/// Sends SIGTERM (unix) / taskkill (windows) to `pid`.
pub fn terminate(pid: u32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let status = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status()?;
        anyhow::ensure!(status.success(), "kill {pid} failed: {status}");
    }
    #[cfg(windows)]
    {
        let status = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T"])
            .status()?;
        anyhow::ensure!(status.success(), "taskkill {pid} failed: {status}");
    }
    Ok(())
}
