use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let command = args.next().ok_or_else(usage)?;
    let args: Vec<String> = args.collect();

    match command.as_str() {
        "build-provisioner" if args.is_empty() => build_provisioner(),
        "stage-backend-jar" => stage_backend_jar(&args),
        "stage-rust-server" => stage_rust_server(&args),
        "verify-bundled-jre" => verify_bundled_jre(&args),
        _ => Err(usage()),
    }
}

fn usage() -> String {
    "Usage: stirling-desktop-build <build-provisioner | stage-backend-jar <source-dir> <dest-dir> [version] | stage-rust-server <source-binary> <dest-dir> | verify-bundled-jre [release-file]>".to_string()
}

fn workspace_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("build-tools is a direct child of the Tauri workspace")
        .to_path_buf()
}

fn build_provisioner() -> Result<(), String> {
    if !cfg!(windows) {
        return Ok(());
    }

    let workspace = workspace_dir();
    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--package",
            "stirling-provisioner",
            "--package",
            "stirling-thumbnail-handler",
        ])
        .current_dir(&workspace)
        .status()
        .map_err(|error| format!("Failed to build Windows installer artifacts: {error}"))?;
    require_success(status, "Cargo failed to build Windows installer artifacts")?;

    let target_dir = cargo_target_dir(&workspace);
    let wix_dir = workspace.join("windows").join("wix");
    fs::create_dir_all(&wix_dir)
        .map_err(|error| format!("Failed to create {}: {error}", wix_dir.display()))?;

    copy_required_artifact(
        &target_dir.join("release").join("stirling-provisioner.exe"),
        &wix_dir.join("stirling-provision.exe"),
    )?;
    copy_required_artifact(
        &target_dir
            .join("release")
            .join("stirling_thumbnail_handler.dll"),
        &wix_dir.join("stirling_thumbnail_handler.dll"),
    )
}

fn cargo_target_dir(workspace: &Path) -> PathBuf {
    match env::var_os("CARGO_TARGET_DIR") {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            }
        }
        None => workspace.join("target"),
    }
}

fn copy_required_artifact(source: &Path, destination: &Path) -> Result<(), String> {
    if !source.is_file() {
        return Err(format!(
            "Required build artifact not found at {}",
            source.display()
        ));
    }
    fs::copy(source, destination).map_err(|error| {
        format!(
            "Failed to copy {} to {}: {error}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

fn require_success(status: ExitStatus, message: &str) -> Result<(), String> {
    if status.success() {
        Ok(())
    } else {
        Err(format!("{message}: {status}"))
    }
}

fn stage_backend_jar(args: &[String]) -> Result<(), String> {
    if !(2..=3).contains(&args.len()) {
        return Err(usage());
    }

    let source_dir = PathBuf::from(&args[0]);
    let destination_dir = PathBuf::from(&args[1]);
    let version = match args.get(2) {
        Some(version) => version.clone(),
        None => resolve_version()?,
    };
    let jar_name = format!("stirling-pdf-{version}.jar");
    let source = source_dir.join(&jar_name);
    if !source.is_file() {
        return Err(format!(
            "Expected {jar_name} in {}; the backend build did not produce it.",
            source_dir.display()
        ));
    }

    fs::create_dir_all(&destination_dir)
        .map_err(|error| format!("Failed to create {}: {error}", destination_dir.display()))?;
    for entry in fs::read_dir(&destination_dir)
        .map_err(|error| format!("Failed to read {}: {error}", destination_dir.display()))?
    {
        let entry = entry.map_err(|error| {
            format!(
                "Failed to read an entry in {}: {error}",
                destination_dir.display()
            )
        })?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("stirling-pdf-")
            && name.ends_with(".jar")
            && entry
                .file_type()
                .map_err(|error| format!("Failed to inspect {}: {error}", entry.path().display()))?
                .is_file()
        {
            fs::remove_file(entry.path())
                .map_err(|error| format!("Failed to remove {}: {error}", entry.path().display()))?;
        }
    }

    let destination = destination_dir.join(&jar_name);
    fs::copy(&source, &destination).map_err(|error| {
        format!(
            "Failed to copy {} to {}: {error}",
            source.display(),
            destination.display()
        )
    })?;
    println!("Staged {jar_name} in {}", destination_dir.display());
    Ok(())
}

/// Tauri `externalBin` sidecars must be named `<name>-<target-triple>[.exe]`
/// in `src-tauri/binaries/`. Stage the release `stirling-server` binary
/// under that name so `bundle.externalBin` picks it up.
fn stage_rust_server(args: &[String]) -> Result<(), String> {
    if args.len() != 2 {
        return Err(usage());
    }
    let source = PathBuf::from(&args[0]);
    let destination_dir = PathBuf::from(&args[1]);
    if !source.is_file() {
        return Err(format!(
            "Expected stirling-server binary at {}; build rust/ first.",
            source.display()
        ));
    }

    let triple = env::var("STIRLING_SERVER_TARGET")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .or_else(rustc_host_triple)
        .ok_or_else(|| "Cannot determine target triple; set STIRLING_SERVER_TARGET".to_string())?;
    let exe_suffix = if cfg!(windows) { ".exe" } else { "" };
    let destination_name = format!("stirling-server-{triple}{exe_suffix}");

    fs::create_dir_all(&destination_dir)
        .map_err(|error| format!("Failed to create {}: {error}", destination_dir.display()))?;
    // Drop stale stagings for other triples so a previous target can't ship.
    for entry in fs::read_dir(&destination_dir)
        .map_err(|error| format!("Failed to read {}: {error}", destination_dir.display()))?
    {
        let entry = entry.map_err(|error| {
            format!(
                "Failed to read an entry in {}: {error}",
                destination_dir.display()
            )
        })?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("stirling-server-")
            && entry
                .file_type()
                .map_err(|error| format!("Failed to inspect {}: {error}", entry.path().display()))?
                .is_file()
        {
            fs::remove_file(entry.path())
                .map_err(|error| format!("Failed to remove {}: {error}", entry.path().display()))?;
        }
    }

    let destination = destination_dir.join(&destination_name);
    fs::copy(&source, &destination).map_err(|error| {
        format!(
            "Failed to copy {} to {}: {error}",
            source.display(),
            destination.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&destination, fs::Permissions::from_mode(0o755));
    }
    println!("Staged {destination_name} in {}", destination_dir.display());
    Ok(())
}

fn rustc_host_triple() -> Option<String> {
    let output = Command::new("rustc").arg("-vV").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            line.strip_prefix("host: ")
                .map(str::trim)
                .map(str::to_owned)
        })
}

fn resolve_version() -> Result<String, String> {
    let version_file = repository_dir().join("app/common/src/main/resources/version.properties");
    let contents = fs::read_to_string(&version_file).map_err(|error| {
        format!(
            "Missing {}; run the backend build or pass a version: {error}",
            version_file.display()
        )
    })?;
    contents
        .lines()
        .find_map(|line| line.strip_prefix("version="))
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("No version= entry in {}", version_file.display()))
}

fn verify_bundled_jre(args: &[String]) -> Result<(), String> {
    if args.len() > 1 {
        return Err(usage());
    }

    let release_file = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("runtime/jre/release"));
    let required = env::var("REQUIRED_JAVA")
        .unwrap_or_else(|_| "25".to_string())
        .parse::<u32>()
        .map_err(|error| format!("REQUIRED_JAVA must be a positive integer: {error}"))?;
    if required == 0 {
        return Err("REQUIRED_JAVA must be a positive integer".to_string());
    }
    let raw = fs::read_to_string(&release_file).map_err(|error| {
        format!(
            "FATAL: cannot read bundled JRE release file at \"{}\": {error}. Is the runtime built? Run 'task desktop:jlink'.",
            release_file.display()
        )
    })?;
    let major = parse_java_major(&raw);

    println!(
        "Bundled JRE major: {} (required >= {required})",
        major.map_or_else(|| "unknown".to_string(), |major| major.to_string())
    );
    if !matches!(major, Some(major) if major >= required) {
        return Err(format!(
            "FATAL: bundled runtime/jre is Java {} but the app JAR requires Java {required}. Run 'task desktop:jlink:clean' and rebuild with JDK {required} active (check 'java -version' / JAVA_HOME).",
            major.map_or_else(|| "unknown".to_string(), |major| major.to_string())
        ));
    }
    Ok(())
}

fn parse_java_major(release: &str) -> Option<u32> {
    release.lines().find_map(|line| {
        let value = line.strip_prefix("JAVA_VERSION=")?.trim_matches('"');
        let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    })
}

fn repository_dir() -> PathBuf {
    workspace_dir()
        .parent()
        .and_then(Path::parent)
        .expect("Tauri workspace is nested under frontend in the repository")
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::{parse_java_major, resolve_version, stage_backend_jar, stage_rust_server};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "stirling-desktop-build-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn reads_java_major_from_jlink_release_format() {
        assert_eq!(parse_java_major("JAVA_VERSION=\"25.0.2\"\n"), Some(25));
        assert_eq!(parse_java_major("JAVA_VERSION=21\n"), Some(21));
        assert_eq!(parse_java_major("IMPLEMENTOR=\"Eclipse\"\n"), None);
    }

    #[test]
    fn reads_the_backend_version_from_properties() {
        let version = resolve_version().unwrap();
        assert!(!version.is_empty());
    }

    #[test]
    fn stages_only_the_requested_backend_jar() {
        let temporary = TemporaryDirectory::new();
        let source = temporary.0.join("source");
        let destination = temporary.0.join("destination");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(source.join("stirling-pdf-1.2.3.jar"), "current").unwrap();
        fs::write(destination.join("stirling-pdf-1.2.2.jar"), "stale").unwrap();
        fs::write(destination.join("other.jar"), "keep").unwrap();

        stage_backend_jar(&[
            source.to_string_lossy().into_owned(),
            destination.to_string_lossy().into_owned(),
            "1.2.3".to_string(),
        ])
        .unwrap();

        assert_eq!(
            fs::read(destination.join("stirling-pdf-1.2.3.jar")).unwrap(),
            b"current"
        );
        assert!(!destination.join("stirling-pdf-1.2.2.jar").exists());
        assert_eq!(fs::read(destination.join("other.jar")).unwrap(), b"keep");
    }

    #[test]
    fn stages_rust_server_under_externalbin_name() {
        let temporary = TemporaryDirectory::new();
        let source = temporary.0.join("stirling-server");
        let destination = temporary.0.join("binaries");
        fs::write(&source, "binary").unwrap();

        stage_rust_server(&[
            source.to_string_lossy().into_owned(),
            destination.to_string_lossy().into_owned(),
        ])
        .unwrap();

        let staged: Vec<_> = fs::read_dir(&destination)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(staged.len(), 1);
        assert!(staged[0].starts_with("stirling-server-"));
    }
}
