use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::ServiceCmd;
use crate::platform;
use crate::services::filesystem;
use crate::services::process::{find_executable, run};

const BINARIES: &[&str] = &["stirling-server", "stirling"];

fn user_bin_dir() -> Result<PathBuf> {
    match platform::os_name() {
        "linux" | "macos" => Ok(platform::user_data_dir()?.join("bin")),
        _ => Ok(platform::user_data_dir()?.join("Programs").join("stirling")),
    }
}

fn release_dir(root: &Path) -> PathBuf {
    filesystem::rust_dir(root).join("target").join("release")
}

pub fn install(root: &Path, release: bool) -> Result<()> {
    if release {
        crate::commands::dev::build(root, true)?;
    }
    let src = release_dir(root);
    let dest_dir = user_bin_dir()?;
    std::fs::create_dir_all(&dest_dir)?;
    for name in BINARIES {
        let src_path = src.join(platform_exe(name));
        if !src_path.is_file() {
            anyhow::bail!(
                "{} not built; run `stirling build{}` first",
                src_path.display(),
                if release { " --release" } else { "" }
            );
        }
        let dest = dest_dir.join(platform_exe(name));
        std::fs::copy(&src_path, &dest)
            .with_context(|| format!("cannot install {}", dest.display()))?;
        println!("installed {}", dest.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in BINARIES {
            let p = dest_dir.join(platform_exe(name));
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
        }
    }
    println!("bin dir: {}", dest_dir.display());
    Ok(())
}

fn platform_exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

pub fn uninstall() -> Result<()> {
    let dir = user_bin_dir()?;
    for name in BINARIES {
        let p = dir.join(platform_exe(name));
        if p.exists() {
            std::fs::remove_file(&p)?;
            println!("removed {}", p.display());
        }
    }
    Ok(())
}

pub fn package(root: &Path, archive: bool) -> Result<()> {
    crate::commands::dev::build(root, true)?;
    let dir = release_dir(root);
    for name in BINARIES {
        let p = dir.join(platform_exe(name));
        if p.is_file() {
            println!("artifact: {}", p.display());
        }
    }
    if archive {
        archive_binaries(root, &dir)?;
    }
    Ok(())
}

fn archive_binaries(root: &Path, dir: &Path) -> Result<()> {
    let out_dir = root.join("dist");
    std::fs::create_dir_all(&out_dir)?;
    let staging = out_dir.join("stirling-pkg");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    for name in BINARIES {
        let p = dir.join(platform_exe(name));
        if p.is_file() {
            std::fs::copy(&p, staging.join(platform_exe(name)))?;
        }
    }
    let zip_path = out_dir.join(format!(
        "stirling-{}-{}-{}.zip",
        env!("CARGO_PKG_VERSION"),
        platform::os_name(),
        platform::arch_name()
    ));
    let zip_exe = find_executable("zip");
    match zip_exe {
        Some(zip) => {
            let status = std::process::Command::new(zip)
                .args(["-r", &zip_path.to_string_lossy(), "."])
                .current_dir(&staging)
                .status()?;
            anyhow::ensure!(status.success(), "zip failed: {status}");
        }
        None => {
            // Fall back to a plain directory bundle; `tar` exists on unix and
            // bsdtar on modern Windows, but we don't assume either.
            println!(
                "zip not found; leaving staged bundle at {}",
                staging.display()
            );
            return Ok(());
        }
    }
    println!("archive: {}", zip_path.display());
    Ok(())
}

// ---------- service management ----------

fn service_name() -> &'static str {
    "stirling-server"
}

fn service_exec(root: &Path) -> Result<PathBuf> {
    let installed = user_bin_dir()?.join(platform_exe("stirling-server"));
    if installed.is_file() {
        return Ok(installed);
    }
    Ok(release_dir(root).join(platform_exe("stirling-server")))
}

pub fn service(root: &Path, cmd: &ServiceCmd) -> Result<()> {
    match platform::os_name() {
        "linux" => service_systemd(root, cmd),
        "macos" => service_launchd(root, cmd),
        _ => anyhow::bail!(
            "service management is not implemented for {} yet; use your platform's service manager",
            platform::os_name()
        ),
    }
}

fn systemctl(args: &[&str]) -> Result<()> {
    let exe = find_executable("systemctl").context("systemctl not found")?;
    let out = run(
        &exe,
        &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        None,
    )?;
    anyhow::ensure!(out.status.success(), "systemctl {} failed", args.join(" "));
    Ok(())
}

fn service_systemd(root: &Path, cmd: &ServiceCmd) -> Result<()> {
    let name = service_name();
    let unit = format!("{name}.service");
    match cmd {
        ServiceCmd::Install => {
            let dir = platform::user_service_dir()?;
            std::fs::create_dir_all(&dir)?;
            let exe = service_exec(root)?;
            let contents = format!(
                "[Unit]\nDescription=Stirling PDF server\nAfter=network-online.target\n\n\
                 [Service]\nExecStart={}\nRestart=on-failure\nWorkingDirectory={}\n\n\
                 [Install]\nWantedBy=default.target\n",
                exe.display(),
                filesystem::rust_dir(root).display()
            );
            std::fs::write(dir.join(&unit), contents)?;
            systemctl(&["--user", "daemon-reload"])?;
            println!("installed {unit}");
        }
        ServiceCmd::Remove => {
            systemctl(&["--user", "disable", "--now", name])?;
            let p = platform::user_service_dir()?.join(&unit);
            let _ = std::fs::remove_file(p);
            systemctl(&["--user", "daemon-reload"])?;
            println!("removed {unit}");
        }
        ServiceCmd::Enable => {
            systemctl(&["--user", "enable", name])?;
            println!("enabled {name}");
        }
        ServiceCmd::Disable => {
            systemctl(&["--user", "disable", name])?;
            println!("disabled {name}");
        }
        ServiceCmd::Status => {
            systemctl(&["--user", "status", name])?;
        }
    }
    Ok(())
}

fn service_launchd(root: &Path, cmd: &ServiceCmd) -> Result<()> {
    let name = service_name();
    let plist_name = format!("{name}.plist");
    let agents = platform::user_service_dir()?;
    let plist = agents.join(&plist_name);
    match cmd {
        ServiceCmd::Install => {
            std::fs::create_dir_all(&agents)?;
            let exe = service_exec(root)?;
            let contents = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
                 \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
                 <plist version=\"1.0\"><dict>\n\
                 \t<key>Label</key><string>{name}</string>\n\
                 \t<key>ProgramArguments</key><array><string>{}</string></array>\n\
                 \t<key>WorkingDirectory</key><string>{}</string>\n\
                 \t<key>RunAtLoad</key><true/>\n\
                 \t<key>KeepAlive</key><true/>\n\
                 </dict></plist>\n",
                exe.display(),
                filesystem::rust_dir(root).display()
            );
            std::fs::write(&plist, contents)?;
            println!("installed {}", plist.display());
        }
        ServiceCmd::Remove => {
            let _ = std::fs::remove_file(&plist);
            println!("removed {plist_name}");
        }
        ServiceCmd::Enable | ServiceCmd::Disable | ServiceCmd::Status => {
            anyhow::bail!("use `launchctl` directly for {cmd:?} on macOS")
        }
    }
    Ok(())
}
