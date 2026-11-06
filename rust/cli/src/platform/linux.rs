use std::path::PathBuf;

fn home() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))
}

pub fn platform_user_config_dir() -> anyhow::Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg));
    }
    Ok(home()?.join(".config"))
}

pub fn platform_user_data_dir() -> anyhow::Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(xdg));
    }
    Ok(home()?.join(".local").join("share"))
}

/// `~/.config/systemd/user` when systemd is present; callers must check for
/// the `systemctl` executable before relying on this directory.
pub fn platform_user_service_dir() -> anyhow::Result<PathBuf> {
    Ok(platform_user_config_dir()?.join("systemd").join("user"))
}
