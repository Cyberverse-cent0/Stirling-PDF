use std::path::PathBuf;

fn env_dir(var: &str) -> anyhow::Result<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("{var} is not set"))
}

pub fn platform_user_config_dir() -> anyhow::Result<PathBuf> {
    env_dir("APPDATA")
}

pub fn platform_user_data_dir() -> anyhow::Result<PathBuf> {
    env_dir("LOCALAPPDATA")
}

/// Windows services are machine-wide and need elevation; per-user service
/// definitions are not supported, so this reports the directory a future
/// scheduled-task based implementation would use.
pub fn platform_user_service_dir() -> anyhow::Result<PathBuf> {
    Ok(env_dir("LOCALAPPDATA")?.join("Stirling").join("services"))
}
