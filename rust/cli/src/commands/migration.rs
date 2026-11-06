use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::cli::MigrationCmd;
use crate::output::json;
use crate::services::filesystem;

const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    ".venv",
    "__pycache__",
];

fn classify(path: &Path, root: &Path) -> &'static str {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let s = rel.to_string_lossy().replace('\\', "/");
    if s.starts_with("scripts/") {
        "A-runtime/B-dev (container entrypoints & root dev scripts)"
    } else if s.starts_with("rust/tests/scripts/") {
        "D-testing (rust backend)"
    } else if s.starts_with("rust/engine/scripts/") {
        "B-dev (engine codegen/parity)"
    } else if s.starts_with("testing/") {
        "D-testing (e2e/cucumber)"
    } else if s.starts_with("runtime/") {
        "G-container (archived runtime)"
    } else if s.starts_with("frontend/scripts/") {
        "F-packaging (Tauri/JRE)"
    } else if s.starts_with(".devcontainer/") {
        "B-dev (devcontainer)"
    } else {
        "H-unclassified"
    }
}

fn collect_scripts(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                collect_scripts(&path, out);
            }
        } else if name.ends_with(".sh") {
            out.push(path);
        }
    }
}

fn audit(root: &Path) -> (Vec<PathBuf>, std::collections::BTreeMap<String, usize>) {
    let mut scripts = Vec::new();
    collect_scripts(root, &mut scripts);
    scripts.sort();
    let mut by_class = std::collections::BTreeMap::new();
    for s in &scripts {
        *by_class.entry(classify(s, root).to_string()).or_insert(0) += 1;
    }
    (scripts, by_class)
}

/// Counts `.sh` files per area — the living version of
/// `migration/reports/SHELL-SCRIPT-INVENTORY.md`.
pub fn run(root: &Path, cmd: &MigrationCmd, json_flag: bool) -> Result<()> {
    let (scripts, by_class) = audit(root);
    match cmd {
        MigrationCmd::Audit => {
            let mut text = format!("{} shell script(s) found\n\n", scripts.len());
            for (class, n) in &by_class {
                text.push_str(&format!("{n:>3}  {class}\n"));
            }
            text.push('\n');
            for s in &scripts {
                text.push_str(&format!(
                    "  {}\n",
                    s.strip_prefix(root).unwrap_or(s).display()
                ));
            }
            let value = serde_json::json!({
                "total": scripts.len(),
                "by_class": by_class,
                "scripts": scripts.iter()
                    .map(|p| p.strip_prefix(root).unwrap_or(p).display().to_string())
                    .collect::<Vec<_>>(),
            });
            json::emit(json_flag, &text, Some(&value));
        }
        MigrationCmd::Report => {
            let rust_dir = filesystem::rust_dir(root);
            let cli_exists = rust_dir.join("cli/Cargo.toml").is_file();
            let value = serde_json::json!({
                "total_scripts": scripts.len(),
                "by_class": by_class,
                "rust_cli_present": cli_exists,
                "note": "see migration/reports/SHELL-SCRIPT-INVENTORY.md for per-script detail",
            });
            let text = format!(
                "{} shell script(s); rust cli present: {cli_exists}\nsee SHELL-SCRIPT-INVENTORY.md",
                scripts.len()
            );
            json::emit(json_flag, &text, Some(&value));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_known_paths() {
        let root = Path::new("/repo");
        assert!(classify(Path::new("/repo/scripts/run.sh"), root).contains("runtime"));
        assert!(
            classify(Path::new("/repo/rust/tests/scripts/smoke.sh"), root).contains("rust backend")
        );
        assert!(
            classify(Path::new("/repo/runtime/docker/x/entrypoint.sh"), root).contains("container")
        );
        assert!(classify(Path::new("/repo/frontend/scripts/a.sh"), root).contains("packaging"));
    }
}
