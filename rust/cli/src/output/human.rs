/// Renders diagnostics as an aligned text table.
pub fn diagnostics_table(diag: &crate::services::diagnostics::Diagnostics) -> String {
    let mut out = String::new();
    out.push_str(&format!("OS: {} ({})\n", diag.os, diag.arch));
    out.push_str(&format!("Project root: {}\n\n", diag.project_root));
    out.push_str(&format!("{:<22} {:<6} {}\n", "CHECK", "OK", "DETAIL"));
    out.push_str(&"-".repeat(70));
    out.push('\n');
    for c in &diag.checks {
        out.push_str(&format!(
            "{:<22} {:<6} {}\n",
            c.name,
            if c.ok { "yes" } else { "NO" },
            c.detail
        ));
    }
    out
}

pub fn engines_table(statuses: &[crate::services::engine_detector::EngineStatus]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<16} {:<6} {}\n",
        "ENGINE", "FOUND", "VERSION / PATH"
    ));
    out.push_str(&"-".repeat(70));
    out.push('\n');
    for s in statuses {
        let detail = match (&s.version, &s.path) {
            (Some(v), Some(p)) => format!("{v}  ({})", p.display()),
            (None, Some(p)) => p.display().to_string(),
            _ => "-".to_string(),
        };
        out.push_str(&format!(
            "{:<16} {:<6} {}\n",
            s.label,
            if s.installed() { "yes" } else { "NO" },
            detail
        ));
    }
    out
}
