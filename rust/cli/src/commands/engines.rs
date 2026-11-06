use std::path::Path;

use anyhow::Result;

use crate::cli::EnginesCmd;
use crate::output::{human, json};
use crate::services::engine_detector::{self, EngineKind, PDF_ENGINES};

fn detect_kinds(kinds: &[EngineKind]) -> Vec<engine_detector::EngineStatus> {
    kinds.iter().copied().map(engine_detector::detect).collect()
}

pub fn run(root: &Path, cmd: &EnginesCmd, json_flag: bool) -> Result<()> {
    let _ = root;
    match cmd {
        EnginesCmd::List => {
            let statuses = engine_detector::detect_all();
            let value = serde_json::to_value(&statuses)?;
            json::emit(json_flag, &human::engines_table(&statuses), Some(&value));
        }
        EnginesCmd::Check => {
            let missing: Vec<_> = detect_kinds(PDF_ENGINES)
                .into_iter()
                .filter(|s| !s.installed())
                .collect();
            if missing.is_empty() {
                println!("all PDF engines present");
            } else {
                for m in &missing {
                    eprintln!("missing engine: {}", m.label);
                }
                anyhow::bail!("{} PDF engine(s) missing", missing.len());
            }
        }
        EnginesCmd::Versions => {
            for s in detect_kinds(PDF_ENGINES) {
                match (&s.version, &s.path) {
                    (Some(v), _) => println!("{:<14} {v}", s.label),
                    (None, Some(p)) => {
                        println!("{:<14} {} (version probe failed)", s.label, p.display())
                    }
                    (None, None) => println!("{:<14} NOT FOUND", s.label),
                }
            }
        }
        EnginesCmd::Validate => {
            let mut failed = false;
            for s in detect_kinds(PDF_ENGINES) {
                if s.installed() && s.version.is_some() {
                    println!("ok      {}", s.label);
                } else {
                    failed = true;
                    println!(
                        "FAIL    {} ({})",
                        s.label,
                        s.version_error.as_deref().unwrap_or("not found")
                    );
                }
            }
            anyhow::ensure!(!failed, "engine validation failed");
        }
        EnginesCmd::Report => {
            let statuses = engine_detector::detect_all();
            let value = serde_json::json!({
                "platform": crate::platform::os_name(),
                "arch": crate::platform::arch_name(),
                "engines": statuses,
            });
            json::emit(json_flag, "", Some(&value));
        }
    }
    Ok(())
}
