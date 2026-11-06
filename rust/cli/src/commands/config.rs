use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::ConfigCmd;
use crate::output::json;

fn settings_paths(root: &Path) -> (PathBuf, PathBuf) {
    let dir = root.join("rust").join("configs");
    (dir.join("settings.yml"), dir.join("custom_settings.yml"))
}

pub fn run(root: &Path, cmd: &ConfigCmd, json_flag: bool) -> Result<()> {
    let (settings, custom) = settings_paths(root);
    match cmd {
        ConfigCmd::Init => {
            if custom.exists() {
                println!("{} already exists", custom.display());
                return Ok(());
            }
            std::fs::create_dir_all(custom.parent().unwrap())?;
            std::fs::write(
                &custom,
                "# Local overrides merged over configs/settings.yml.\n",
            )?;
            println!("created {}", custom.display());
            Ok(())
        }
        ConfigCmd::Validate => {
            let mut ok = true;
            for path in [&settings, &custom] {
                if !path.exists() {
                    continue;
                }
                match std::fs::read_to_string(path)
                    .map_err(anyhow::Error::from)
                    .and_then(|s| {
                        serde_yml::from_str::<serde_yml::Value>(&s)
                            .map(|_| ())
                            .map_err(|e| anyhow::anyhow!("{e}"))
                    }) {
                    Ok(()) => println!("ok      {}", path.display()),
                    Err(e) => {
                        ok = false;
                        eprintln!("FAIL    {}: {e}", path.display());
                    }
                }
            }
            anyhow::ensure!(ok, "config validation failed");
            Ok(())
        }
        ConfigCmd::Show => {
            let mut merged = serde_yml::Mapping::new();
            for path in [&settings, &custom] {
                if !path.exists() {
                    continue;
                }
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("cannot read {}", path.display()))?;
                if let serde_yml::Value::Mapping(map) =
                    serde_yml::from_str(&text).context("invalid YAML")?
                {
                    for (k, v) in map {
                        merged.insert(k, v);
                    }
                }
            }
            let value = serde_json::to_value(serde_yml::Value::Mapping(merged))?;
            let text = serde_json::to_string_pretty(&value)?;
            json::emit(json_flag, &text, Some(&value));
            Ok(())
        }
    }
}
