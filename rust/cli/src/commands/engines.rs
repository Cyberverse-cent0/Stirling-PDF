use std::io::{IsTerminal, Write};
use std::path::Path;

use anyhow::Result;

use crate::cli::EnginesCmd;
use crate::output::{human, json};
use crate::services::engine_detector::{self, EngineKind, PDF_ENGINES};
use crate::services::engine_install;

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
        EnginesCmd::Install { engine, yes } => install(engine, *yes)?,
    }
    Ok(())
}

fn install(name: &str, yes: bool) -> Result<()> {
    let kind = engine_install::kind_named(name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown engine '{name}' — expected one of: {}",
            engine_detector::ALL_ENGINES
                .iter()
                .map(|k| k.label())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    if engine_detector::detect(kind).installed() {
        println!("{} is already installed", kind.label());
        return Ok(());
    }
    let Some(argv) = engine_install::install_spec(kind) else {
        anyhow::bail!(
            "no package-manager mapping for {} on {} — install it manually",
            kind.label(),
            crate::platform::os_name()
        );
    };
    let command = argv.join(" ");
    println!("{} is not installed.", kind.label());
    println!("Install command: {command}");

    if !yes {
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            anyhow::bail!("re-run with --yes to execute, or run the command above yourself");
        }
        print!("Run it? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("aborted");
            return Ok(());
        }
    }

    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run {}: {e}", argv[0]))?;
    anyhow::ensure!(status.success(), "package install failed: {status}");

    let after = engine_detector::detect(kind);
    if after.installed() {
        println!(
            "{} installed ({})",
            kind.label(),
            after
                .version
                .as_deref()
                .unwrap_or("version probe unavailable")
        );
    } else {
        println!(
            "{} install finished but the engine is still not on PATH — check the package output",
            kind.label()
        );
    }
    Ok(())
}
