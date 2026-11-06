use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::cli::TestKind;
use crate::services::filesystem;
use crate::services::process::find_executable;

fn cargo(rust_dir: &Path, args: &[&str]) -> Result<()> {
    let exe = find_executable("cargo").context("cargo not found on PATH")?;
    let status = Command::new(exe)
        .args(args)
        .current_dir(rust_dir)
        .status()?;
    anyhow::ensure!(
        status.success(),
        "cargo {} failed: {status}",
        args.join(" ")
    );
    Ok(())
}

/// `stirling test` with no subcommand runs the full Rust gate: fmt,
/// clippy, then the workspace test suite.
pub fn run(root: &Path, kind: Option<&TestKind>) -> Result<()> {
    let rust = filesystem::rust_dir(root);
    match kind {
        None => {
            cargo(&rust, &["fmt", "--all", "--", "--check"])?;
            cargo(
                &rust,
                &[
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--",
                    "-D",
                    "warnings",
                ],
            )?;
            cargo(&rust, &["test", "--workspace"])
        }
        Some(TestKind::Unit { filter }) => {
            let mut args = vec!["test", "--bin", "stirling-server"];
            if let Some(f) = filter {
                args.push(f.as_str());
            }
            cargo(&rust, &args)
        }
        Some(TestKind::Integration) => cargo(&rust, &["test", "--workspace"]),
        Some(TestKind::Engine) => {
            let engine = rust.join("engine");
            anyhow::ensure!(engine.join("Cargo.toml").is_file(), "rust/engine missing");
            cargo(&engine, &["test", "--all-targets"])
        }
        Some(TestKind::Fixtures) => {
            // Same fixture-generation step the smoke harness runs.
            let exe = find_executable("cargo").context("cargo not found")?;
            let status = Command::new(exe)
                .args(["test", "--bin", "stirling-server", "testkit::fixture_tests"])
                .current_dir(&rust)
                .status()?;
            anyhow::ensure!(status.success(), "fixture tests failed: {status}");
            Ok(())
        }
        Some(TestKind::Runtime) => crate::commands::smoke::runtime(root),
        Some(TestKind::General) => crate::commands::smoke::general(root),
        Some(TestKind::Security) => crate::commands::smoke::security(root),
        Some(TestKind::Coverage) => coverage(&rust),
        Some(TestKind::Conversions) => {
            anyhow::bail!(
                "no dedicated conversions smoke exists yet; \
                 `stirling test runtime` covers the multi-endpoint smoke"
            )
        }
    }
}

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// `coverage-report.sh`: per-source-file count of `#[test]` /
/// `#[tokio::test]` attributes across `src/` and `engine/src/`.
fn coverage(rust: &Path) -> Result<()> {
    let mut files = Vec::new();
    rust_files(&rust.join("src"), &mut files);
    rust_files(&rust.join("engine/src"), &mut files);
    files.sort();

    println!("{:<58} {:>5}  COVERAGE", "SOURCE FILE", "TESTS");
    println!("{} {}  {}", "-".repeat(58), "-".repeat(5), "-".repeat(24));
    for file in &files {
        let text = std::fs::read_to_string(file)?;
        let n = text
            .lines()
            .filter(|l| l.contains("#[test]") || l.contains("#[tokio::test]"))
            .count();
        let rel = file.strip_prefix(rust).unwrap_or(file);
        let coverage = if rel.starts_with("engine") {
            "engine unit tests"
        } else if n > 0 {
            "unit tests in-file"
        } else {
            "see TESTING.md disposition"
        };
        println!("{:<58} {:>5}  {}", rel.display(), n, coverage);
    }

    println!();
    println!("Integration tests: engine/tests/*.rs");
    println!("Line coverage (optional): cargo llvm-cov --all-targets  (rust/)");
    Ok(())
}
