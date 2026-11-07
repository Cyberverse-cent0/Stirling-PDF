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

/// OS/kernel identity plus tool versions for the diagnose bundle —
/// the Rust-scoped counterpart of the shell script's system-info file.
pub fn system_info() -> String {
    let mut out = String::new();
    if let Ok(release) = std::fs::read_to_string("/etc/os-release") {
        out.push_str(&release);
        out.push('\n');
    }
    if let Ok(uname) =
        crate::services::process::run(std::path::Path::new("uname"), &["-a".to_string()], None)
    {
        out.push_str(&String::from_utf8_lossy(&uname.stdout));
    }
    for (label, exe, arg) in [
        ("rustc", "rustc", "--version"),
        ("cargo", "cargo", "--version"),
        ("git", "git", "--version"),
    ] {
        if let Some(path) = find_executable(exe) {
            if let Ok(o) = crate::services::process::run(&path, &[arg.to_string()], None) {
                let text = String::from_utf8_lossy(&o.stdout);
                out.push_str(&format!("{label}: {}\n", text.trim()));
            }
        }
    }
    for status in engine_detector::detect_all() {
        let detail = match (&status.path, &status.version) {
            (Some(p), Some(v)) => format!("{} ({})", p.display(), v),
            (Some(p), None) => p.display().to_string(),
            (None, _) => "not found".to_string(),
        };
        out.push_str(&format!("engine {}: {detail}\n", status.label));
    }
    out
}

/// Settings files can carry API keys and passwords; the bundle keeps the
/// structure but blanks values on sensitive-looking keys.
pub fn redact_settings(text: &str) -> String {
    const SENSITIVE: &[&str] = &[
        "key",
        "secret",
        "password",
        "passwd",
        "token",
        "apikey",
        "api_key",
        "credential",
    ];
    text.lines()
        .map(|line| {
            let lower = line.to_lowercase();
            if SENSITIVE.iter().any(|s| lower.contains(s)) {
                match line.split_once(':') {
                    Some((key, _)) => format!("{key}: ***"),
                    None => line.to_string(),
                }
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
