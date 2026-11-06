use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::services::filesystem;

/// Runs cargo with inherited stdio so build output streams to the terminal.
fn cargo(rust_dir: &Path, args: &[&str]) -> Result<()> {
    let exe =
        crate::services::process::find_executable("cargo").context("cargo not found on PATH")?;
    let status = Command::new(exe)
        .args(args)
        .current_dir(rust_dir)
        .status()
        .context("failed to run cargo")?;
    anyhow::ensure!(
        status.success(),
        "cargo {} failed: {status}",
        args.join(" ")
    );
    Ok(())
}

pub fn build(root: &Path, release: bool) -> Result<()> {
    let dir = filesystem::rust_dir(root);
    let mut args = vec!["build", "--workspace"];
    if release {
        args.push("--release");
    }
    cargo(&dir, &args)
}

pub fn check(root: &Path) -> Result<()> {
    let dir = filesystem::rust_dir(root);
    cargo(&dir, &["fmt", "--all", "--", "--check"])?;
    cargo(
        &dir,
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    cargo(&dir, &["test", "--workspace"])
}

pub fn lint(root: &Path) -> Result<()> {
    cargo(
        &filesystem::rust_dir(root),
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    )
}

pub fn fmt(root: &Path) -> Result<()> {
    cargo(&filesystem::rust_dir(root), &["fmt", "--all"])
}

pub fn clean(root: &Path) -> Result<()> {
    let dir = filesystem::rust_dir(root);
    cargo(&dir, &["clean", "--workspace"])?;
    // engine is a separate workspace with its own target dir
    let engine = dir.join("engine");
    if engine.join("Cargo.toml").is_file() {
        cargo(&engine, &["clean"])?;
    }
    Ok(())
}

pub fn watch(root: &Path) -> Result<()> {
    let dir = filesystem::rust_dir(root);
    let cargo_watch = crate::services::process::find_executable("cargo-watch")
        .or_else(|| crate::services::process::find_executable("watch"));
    let exe = match cargo_watch {
        Some(p) => p,
        None => anyhow::bail!(
            "cargo-watch not installed; run `cargo install cargo-watch` or use `task frontend:dev` etc."
        ),
    };
    let status = Command::new(exe)
        .args(["-w", "."])
        .arg("check")
        .current_dir(&dir)
        .status()?;
    anyhow::ensure!(status.success(), "watch exited: {status}");
    Ok(())
}
