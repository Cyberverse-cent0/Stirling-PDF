use std::path::Path;

use anyhow::Result;

use crate::output::{human, json};
use crate::services::diagnostics;

pub fn doctor(root: &Path, json_flag: bool) -> Result<()> {
    let diag = diagnostics::collect(root);
    let value = serde_json::to_value(&diag)?;
    let text = format!(
        "{}\n{} failing check(s)",
        human::diagnostics_table(&diag),
        diag.checks.iter().filter(|c| !c.ok).count()
    );
    json::emit(json_flag, &text, Some(&value));
    Ok(())
}

/// Writes a diagnostic bundle under `.run/diagnostics/` — the Rust
/// replacement for `scripts/stirling-diagnostics.sh` (which targeted the
/// Java/proprietary deployment and its /configs volume).
pub fn diagnose(root: &Path) -> Result<()> {
    let diag = diagnostics::collect(root);
    let dir = root.join(".run").join("diagnostics");
    std::fs::create_dir_all(&dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let report = dir.join(format!("diagnostics-{stamp}.txt"));
    std::fs::write(&report, human::diagnostics_table(&diag))?;
    let engines = serde_json::to_string_pretty(&crate::services::engine_detector::detect_all())?;
    std::fs::write(dir.join(format!("engines-{stamp}.json")), engines)?;
    println!("wrote {}", report.display());
    Ok(())
}

pub fn report(root: &Path, json_flag: bool) -> Result<()> {
    let diag = diagnostics::collect(root);
    let total = diag.checks.len();
    let ok = diag.checks.iter().filter(|c| c.ok).count();
    let text = format!(
        "stirling tooling report\nos={} arch={}\nchecks: {ok}/{total} passing\nroot: {}",
        diag.os, diag.arch, diag.project_root
    );
    let value = serde_json::json!({
        "os": diag.os,
        "arch": diag.arch,
        "project_root": diag.project_root,
        "checks_passing": ok,
        "checks_total": total,
    });
    json::emit(json_flag, &text, Some(&value));
    Ok(())
}
