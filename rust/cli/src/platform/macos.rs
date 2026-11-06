use std::path::PathBuf;

fn home() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))
}

pub fn platform_user_config_dir() -> anyhow::Result<PathBuf> {
    Ok(home()?.join("Library").join("Application Support"))
}

pub fn platform_user_data_dir() -> anyhow::Result<PathBuf> {
    Ok(home()?.join("Library").join("Application Support"))
}

/// `~/Library/LaunchAgents` — launchd user agents need no privileges.
pub fn platform_user_service_dir() -> anyhow::Result<PathBuf> {
    Ok(home()?.join("Library").join("LaunchAgents"))
}
