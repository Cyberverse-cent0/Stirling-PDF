use std::path::Path;

use serde::Serialize;

use crate::platform;
use crate::services::engine_detector;
use crate::services::filesystem;
use crate::services::process::find_executable;

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct Diagnostics {
    pub os: String,
    pub arch: String,
    pub project_root: String,
    pub checks: Vec<Check>,
}

fn check(name: &str, ok: bool, detail: impl Into<String>) -> Check {
    Check {
        name: name.to_string(),
        ok,
        detail: detail.into(),
    }
}

fn tool_check(name: &str) -> Check {
    match find_executable(name) {
        Some(p) => check(name, true, p.display().to_string()),
        None => check(name, false, "not found on PATH"),
    }
}

fn dir_writable_check(name: &str, dir: &Path) -> Check {
    let probe = dir.join(".stirling-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            check(name, true, dir.display().to_string())
        }
        Err(e) => check(name, false, format!("{}: {e}", dir.display())),
    }
}

/// Collects the environment facts `stirling doctor` reports. Never installs
/// or mutates anything; missing tools are reported, not fixed.
pub fn collect(root: &Path) -> Diagnostics {
    let mut checks = vec![tool_check("cargo"), tool_check("rustc"), tool_check("git")];

    checks.push(match platform::user_config_dir() {
        Ok(d) => dir_writable_check("user config dir", &d),
        Err(e) => check("user config dir", false, e.to_string()),
    });
    checks.push(match platform::user_data_dir() {
        Ok(d) => dir_writable_check("user data dir", &d),
        Err(e) => check("user data dir", false, e.to_string()),
    });

    for kind in engine_detector::PDF_ENGINES {
        let status = engine_detector::detect(*kind);
        let detail = match (&status.path, &status.version) {
            (Some(p), Some(v)) => format!("{} — {}", p.display(), v),
            (Some(p), None) => p.display().to_string(),
            (None, _) => "not found on PATH".to_string(),
        };
        checks.push(check(
            &format!("engine:{}", status.label),
            status.installed(),
            detail,
        ));
    }

    Diagnostics {
        os: platform::os_name().to_string(),
        arch: platform::arch_name().to_string(),
        project_root: filesystem::repo_root(root).display().to_string(),
        checks,
    }
}
