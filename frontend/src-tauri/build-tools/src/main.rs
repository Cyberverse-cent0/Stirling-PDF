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
        "stage-engines" => stage_engines(&args),
        "verify-bundled-jre" => verify_bundled_jre(&args),
        _ => Err(usage()),
    }
}

fn usage() -> String {
    "Usage: stirling-desktop-build <build-provisioner | stage-backend-jar <source-dir> <dest-dir> [version] | stage-rust-server <dest-dir> <source>[@<triple>]... | stage-engines [dest-dir] | verify-bundled-jre [release-file]>".to_string()
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
/// in `src-tauri/binaries/`. Stage release `stirling-server` binaries under
/// those names so `bundle.externalBin` picks them up.
///
/// `stage-rust-server <dest-dir> <source>[@<triple>]...` — a bare source uses
/// `STIRLING_SERVER_TARGET` or the rustc host triple; `source=triple` stages
/// under an explicit triple. Universal macOS builds need per-arch binaries
/// plus a `lipo`'d `universal-apple-darwin` one, so several specs may be
/// staged in a single call (the stale-binary sweep runs once up front).
fn stage_rust_server(args: &[String]) -> Result<(), String> {
    if args.len() < 2 {
        return Err(usage());
    }
    let destination_dir = PathBuf::from(&args[0]);

    let mut specs = Vec::with_capacity(args.len() - 1);
    for arg in &args[1..] {
        let (source, explicit_triple) = match arg.split_once('=') {
            Some((source, triple)) => (source.to_string(), Some(triple.to_string())),
            None => (arg.clone(), None),
        };
        let source = PathBuf::from(source);
        if !source.is_file() {
            return Err(format!(
                "Expected stirling-server binary at {}; build rust/ first.",
                source.display()
            ));
        }
        let triple = explicit_triple
            .or_else(|| env::var("STIRLING_SERVER_TARGET").ok())
            .filter(|t| !t.trim().is_empty())
            .or_else(rustc_host_triple)
            .ok_or_else(|| {
                "Cannot determine target triple; set STIRLING_SERVER_TARGET".to_string()
            })?;
        specs.push((source, triple));
    }

    let exe_suffix = if cfg!(windows) { ".exe" } else { "" };
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

    for (source, triple) in &specs {
        let destination_name = format!("stirling-server-{triple}{exe_suffix}");
        let destination = destination_dir.join(&destination_name);
        fs::copy(source, &destination).map_err(|error| {
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
    }
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

/// Tier-1 engine set from `migration/reports/desktop-phase-b-endpoint-audit.md`:
/// the binaries `endpointconfig::ENGINE_PROBES` and the `EngineSpec` call
/// sites resolve through `find_executable`'s bundled-dir search.
const TIER1_ENGINES: &[&str] = &[
    "gs",
    "qpdf",
    "pdftohtml",
    "rsvg-convert",
    "inkscape",
    "unrar",
    "unar",
    "bsdtar",
    "7z",
    "rar",
    "tesseract",
    "magick",
];

/// glibc core is never bundled: the build host's libc may be newer than a
/// user's, and the dynamic loader resolves these from the system anyway.
fn is_glibc_core(name: &str) -> bool {
    matches!(
        name,
        "linux-vdso.so.1"
            | "linux-gate.so.1"
            | "ld-linux-x86-64.so.2"
            | "ld-linux.so.2"
            | "ld-linux-aarch64.so.1"
            | "libc.so.6"
            | "libm.so.6"
            | "libdl.so.2"
            | "librt.so.1"
            | "libpthread.so.0"
            | "libresolv.so.2"
            | "libutil.so.1"
            | "libanl.so.1"
            | "libthread_db.so.1"
            | "libmemusage.so"
            | "libpcprofile.so"
            | "libSegFault.so"
            | "libcidn.so.1"
            | "libcrypt.so.1"
            | "libBrokenLocale.so.1"
            | "libnsl.so.1"
    ) || name.starts_with("libnss_")
        || name.starts_with("ld-musl-")
        || name.starts_with("ld64.")
}

/// Parses `ldd` output into resolved library paths. Lines look like
/// `\tlibz.so.1 => /lib64/libz.so.1 (0x…)` or bare `/lib64/ld-linux… (0x…)`.
fn parse_ldd_paths(output: &str) -> Vec<PathBuf> {
    output
        .lines()
        .filter_map(|line| {
            let path = if let Some((_, rest)) = line.split_once("=>") {
                rest.split_whitespace().next()?
            } else {
                line.split_whitespace().next()?
            };
            (path.starts_with('/')).then(|| PathBuf::from(path))
        })
        .collect()
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(windows)]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn find_on_path(program: &str) -> Option<PathBuf> {
    let dirs = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&dirs) {
        let candidate = dir.join(program);
        #[cfg(windows)]
        let candidates = ["exe", "com", "bat", "cmd"]
            .iter()
            .map(|ext| candidate.with_extension(ext))
            .collect::<Vec<_>>();
        #[cfg(not(windows))]
        let candidates = vec![candidate];
        for path in candidates {
            if is_executable_file(&path) {
                return Some(path);
            }
        }
    }
    None
}

fn set_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
    }
    #[cfg(windows)]
    let _ = path;
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn copy_dir_all(source: &Path, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| format!("Failed to create {}: {e}", dest.display()))?;
    for entry in
        fs::read_dir(source).map_err(|e| format!("Failed to read {}: {e}", source.display()))?
    {
        let entry =
            entry.map_err(|e| format!("Failed to read an entry in {}: {e}", source.display()))?;
        let target = dest.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|e| format!("Failed to inspect {}: {e}", entry.path().display()))?;
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &target).map_err(|e| {
                format!(
                    "Failed to copy {} to {}: {e}",
                    entry.path().display(),
                    target.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Stages the Tier-1 engine binaries plus their shared-library closure
/// into `<dest>/engines`-style layout consumed by `bundle.resources`.
/// Missing engines only warn: `endpointconfig` probes then disable the
/// tool group, same as a server without the binary. Always writes
/// `<dest>/.keep` so the `engines/**/*` resource glob matches even when
/// nothing was found.
fn stage_engines(args: &[String]) -> Result<(), String> {
    if args.len() > 1 {
        return Err(usage());
    }
    let dest = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_dir().join("engines"));
    fs::create_dir_all(&dest).map_err(|e| format!("Failed to create {}: {e}", dest.display()))?;
    clean_engines_dest(&dest)?;
    fs::create_dir_all(dest.join("lib"))
        .map_err(|e| format!("Failed to create {}: {e}", dest.display()))?;
    fs::write(dest.join(".keep"), b"")
        .map_err(|e| format!("Failed to write {}: {e}", dest.display()))?;

    match std::env::consts::OS {
        "linux" => stage_engines_linux(&dest)?,
        "macos" => stage_engines_macos(&dest)?,
        "windows" => stage_engines_windows(&dest)?,
        other => eprintln!("warn: unsupported OS '{other}'; engines resolve from PATH only"),
    }

    stage_ghostscript_share(&dest);
    stage_tessdata(&dest);
    mirror_for_dev_sidecar(&dest);
    println!("done: {}", dest.display());
    Ok(())
}

/// Removes previously staged output so re-runs neither collide with nor
/// ship stale copies of earlier engines (an engine since uninstalled
/// would otherwise linger, its lib closure already wiped). Only managed
/// names are touched — `lib/`, `share/`, and files named after a
/// TIER1 engine (with or without `.exe`) — so `.keep`, `README.md` and
/// anything unrecognized survive.
fn clean_engines_dest(dest: &Path) -> Result<(), String> {
    for entry in
        fs::read_dir(dest).map_err(|e| format!("Failed to read {}: {e}", dest.display()))?
    {
        let entry =
            entry.map_err(|e| format!("Failed to read an entry in {}: {e}", dest.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let stem = name.strip_suffix(".exe").unwrap_or(name);
        if name != "lib" && name != "share" && !TIER1_ENGINES.contains(&stem) {
            continue;
        }
        let path = entry.path();
        let result = if entry
            .file_type()
            .map_err(|e| format!("Failed to inspect {}: {e}", path.display()))?
            .is_dir()
        {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        result.map_err(|e| format!("Failed to remove {}: {e}", path.display()))?;
    }
    Ok(())
}

fn stage_engines_linux(dest: &Path) -> Result<(), String> {
    let mut queue: std::collections::VecDeque<PathBuf> = Default::default();
    let mut seen: std::collections::HashSet<PathBuf> = Default::default();

    for name in TIER1_ENGINES {
        let Some(src) = find_on_path(name) else {
            eprintln!("warn: {name} not found on PATH, skipping");
            continue;
        };
        let src = canonical(&src);
        let target = dest.join(name);
        fs::copy(&src, &target).map_err(|e| {
            format!(
                "Failed to copy {} to {}: {e}",
                src.display(),
                target.display()
            )
        })?;
        set_executable(&target);
        queue.push_back(target.clone());
        println!("staged {name} <- {}", src.display());
    }

    while let Some(file) = queue.pop_front() {
        let real = canonical(&file);
        if !seen.insert(real) {
            continue;
        }
        let output = Command::new("ldd").arg(&file).output();
        let Ok(output) = output else { continue };
        if !output.status.success() {
            continue;
        }
        for dep in parse_ldd_paths(&String::from_utf8_lossy(&output.stdout)) {
            let Some(base) = dep.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            if is_glibc_core(&base) || !dep.is_file() {
                continue;
            }
            let dep_real = canonical(&dep);
            if seen.contains(&dep_real) {
                continue;
            }
            seen.insert(dep_real);
            let target = dest.join("lib").join(&base);
            if target.exists() {
                eprintln!(
                    "warn: lib basename collision: {} -> {base} (kept first)",
                    dep.display()
                );
                continue;
            }
            if let Err(e) = fs::copy(&dep, &target) {
                eprintln!("warn: cannot copy {}: {e}", dep.display());
                continue;
            }
            set_executable(&target);
            queue.push_back(target);
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn stage_engines_macos(dest: &Path) -> Result<(), String> {
    let mut queue: std::collections::VecDeque<PathBuf> = Default::default();
    let mut done: std::collections::HashSet<PathBuf> = Default::default();

    for name in TIER1_ENGINES {
        let Some(src) = find_on_path(name) else {
            eprintln!("warn: {name} not found on PATH, skipping");
            continue;
        };
        let src = canonical(&src);
        let target = dest.join(name);
        fs::copy(&src, &target).map_err(|e| {
            format!(
                "Failed to copy {} to {}: {e}",
                src.display(),
                target.display()
            )
        })?;
        set_executable(&target);
        queue.push_back(target.clone());
        println!("staged {name} <- {}", src.display());
    }

    // Deps under the Homebrew prefixes are copied into engines/lib and
    // every referencing file is rewritten to @loader_path-relative names,
    // then adhoc re-signed: install_name_tool invalidates signatures and
    // arm64 requires at least adhoc.
    while let Some(file) = queue.pop_front() {
        if !done.insert(file.clone()) {
            continue;
        }
        set_executable(&file);
        let in_lib = file.parent() == Some(dest.join("lib").as_path());
        let file_id = command_stdout("otool", &["-D", &file.to_string_lossy()])
            .and_then(|out| otool_entries(&out).into_iter().next())
            .unwrap_or_default();

        let deps = command_stdout("otool", &["-L", &file.to_string_lossy()])
            .map(|out| otool_entries(&out))
            .unwrap_or_default();

        for dep in deps {
            if dep.starts_with('@')
                || dep.starts_with("/usr/lib/")
                || dep.starts_with("/System/")
                || dep.starts_with("/usr/libexec/")
                || !(dep.starts_with("/opt/homebrew/") || dep.starts_with("/usr/local/"))
            {
                continue;
            }
            let Some(base) = Path::new(&dep)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
            else {
                continue;
            };
            let target = dest.join("lib").join(&base);
            if !target.exists() {
                let dep_path = PathBuf::from(&dep);
                if fs::copy(&dep_path, &target).is_err() {
                    eprintln!("warn: cannot copy {dep}");
                    continue;
                }
                set_executable(&target);
                queue.push_back(target.clone());
            }
            let rel = if in_lib {
                format!("@loader_path/{base}")
            } else {
                format!("@loader_path/lib/{base}")
            };
            let (flag, old) = if !file_id.is_empty() && dep == file_id {
                ("-id", dep.clone())
            } else {
                ("-change", dep.clone())
            };
            let _ = Command::new("install_name_tool")
                .args([flag, &old, &rel, &file.to_string_lossy()])
                .status();
        }
    }

    if Command::new("codesign").arg("--version").output().is_ok() {
        for entry in walk_files(dest)? {
            if entry.file_name().is_some_and(|n| n == ".keep") {
                continue;
            }
            let _ = Command::new("codesign")
                .args(["--force", "--sign", "-", &entry.to_string_lossy()])
                .status();
        }
    }
    Ok(())
}

/// Windows engine executable names differ from the unix probe names the
/// Rust backend uses (`gs` -> `gswin64c`); staged copies are renamed back
/// to `<probe>.exe` so `find_executable`'s PATHEXT candidates match.
#[cfg(target_os = "windows")]
fn windows_engine_exe(name: &str) -> &str {
    match name {
        "gs" => "gswin64c",
        other => other,
    }
}

/// The Windows loader only searches the exe's own directory (then
/// System32/PATH), so each engine lands flat in `engines/` with every DLL
/// it might need copied next to it. DLLs are collected from the exe's
/// directory and its `lib`/`bin` siblings — the layout choco/official
/// installers use. Deps living elsewhere degrade at exec time like an
/// engine-less install.
#[cfg(target_os = "windows")]
fn stage_engines_windows(dest: &Path) -> Result<(), String> {
    for name in TIER1_ENGINES {
        let exe_name = windows_engine_exe(name);
        let found = find_on_path(name).or_else(|| find_on_path(exe_name));
        let Some(src) = found else {
            eprintln!("warn: {name} not found on PATH, skipping");
            continue;
        };
        let src = canonical(&src);
        let target = dest.join(format!("{name}.exe"));
        fs::copy(&src, &target).map_err(|e| {
            format!(
                "Failed to copy {} to {}: {e}",
                src.display(),
                target.display()
            )
        })?;
        println!("staged {name} <- {}", src.display());

        let Some(exe_dir) = src.parent() else {
            continue;
        };
        let mut dll_dirs = vec![exe_dir.to_path_buf()];
        for sibling in ["lib", "bin"] {
            let dir = exe_dir.join("..").join(sibling);
            if dir.is_dir() {
                dll_dirs.push(canonical(&dir));
            }
        }
        let mut dlls = Vec::new();
        for dir in dll_dirs {
            collect_dlls(&dir, 0, &mut dlls);
        }
        for path in dlls {
            let Some(base) = path.file_name() else {
                continue;
            };
            let dll_target = dest.join(base);
            // First wins on basename collisions — same policy as the
            // Linux closure walk.
            if dll_target.exists() {
                continue;
            }
            if let Err(e) = fs::copy(&path, &dll_target) {
                eprintln!("warn: cannot copy {}: {e}", path.display());
            }
        }
    }
    Ok(())
}

/// Recursive `*.dll` walk, depth-capped: GTK-style installs nest loadable
/// modules several levels under `lib/` and the loader only finds them
/// beside the exe if they are flattened into `engines/`.
#[cfg(target_os = "windows")]
fn collect_dlls(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    const MAX_DEPTH: u32 = 4;
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_dlls(&path, depth + 1, out);
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "dll") {
            out.push(path);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn stage_engines_windows(_dest: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn stage_engines_macos(_dest: &Path) -> Result<(), String> {
    Ok(())
}

/// `otool -L`/`-D` print a `<file>:` header line, then one load command
/// per line whose first field is the path. Returns just the paths.
#[cfg(target_os = "macos")]
fn otool_entries(output: &str) -> Vec<String> {
    let mut lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.first().is_some_and(|l| l.trim_end().ends_with(':')) {
        lines.remove(0);
    }
    lines
        .iter()
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .collect()
}

#[cfg(target_os = "macos")]
fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(target_os = "macos")]
fn walk_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in
            fs::read_dir(&dir).map_err(|e| format!("Failed to read {}: {e}", dir.display()))?
        {
            let entry =
                entry.map_err(|e| format!("Failed to read an entry in {}: {e}", dir.display()))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|e| format!("Failed to inspect {}: {e}", path.display()))?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                files.push(path);
            }
        }
    }
    Ok(files)
}

/// Copies the Ghostscript share tree (Resource/, lib/, iccprofiles/) when
/// the build host has one. gs expands GS_LIB itself into Resource/Init,
/// lib and Resource/Font, so the shell only needs the parent dir.
fn stage_ghostscript_share(dest: &Path) {
    let mut candidates: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "windows")]
    {
        // Versioned installs nest under C:\Program Files\gs\gs<ver>\;
        // `lib` carries the Resource tree on Windows layouts.
        if let Ok(entries) = fs::read_dir(r"C:\Program Files\gs") {
            for entry in entries.flatten() {
                let root = entry.path();
                candidates.push(root.join("share").join("ghostscript"));
                candidates.push(root.join("lib"));
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let fixed: &[&str] = if cfg!(target_os = "macos") {
            &[
                "/opt/homebrew/share/ghostscript",
                "/usr/local/share/ghostscript",
            ]
        } else {
            &[
                "/usr/share/ghostscript",
                "/usr/local/share/ghostscript",
                "/opt/homebrew/share/ghostscript",
            ]
        };
        candidates.extend(fixed.iter().map(PathBuf::from));
        // Versioned layouts nest the tree one level down: distro packages
        // use /usr/share/ghostscript/<version>/, brew kegs use
        // <prefix>/Cellar/ghostscript/<version>/share/ghostscript.
        for keg in [
            "/opt/homebrew/Cellar/ghostscript",
            "/usr/local/Cellar/ghostscript",
        ] {
            if let Ok(entries) = fs::read_dir(keg) {
                for entry in entries.flatten() {
                    candidates.push(entry.path().join("share").join("ghostscript"));
                }
            }
        }
        if let Ok(entries) = fs::read_dir("/usr/share/ghostscript") {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    candidates.push(entry.path());
                }
            }
        }
    }

    let Some(src) = candidates
        .into_iter()
        .find(|dir| dir.join("Resource").is_dir())
    else {
        return;
    };
    let target = dest.join("share").join("ghostscript");
    match copy_dir_all(&src, &target) {
        Ok(()) => println!("staged ghostscript share <- {}", src.display()),
        Err(e) => eprintln!("warn: {e}"),
    }
}

/// `*.traineddata` files live outside the tesseract binary's package on
/// most distros; the Rust backend resolves them via `TESSDATA_PREFIX`,
/// so the staged copy lands in `share/tessdata` next to the ghostscript
/// tree and the shell exports it the same way as `GS_LIB`.
fn stage_tessdata(dest: &Path) {
    let mut candidates: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "windows")]
    {
        for root in [
            r"C:\Program Files\Tesseract-OCR",
            r"C:\ProgramData\chocolatey\lib\tesseract",
        ] {
            // UB-Mannheim installs tessdata directly under the root;
            // choco nests it under <pkg>/tools/ or a versioned subdir.
            candidates.push(PathBuf::from(root).join("tessdata"));
            if let Ok(entries) = fs::read_dir(root) {
                for entry in entries.flatten() {
                    candidates.push(entry.path().join("tessdata"));
                    candidates.push(entry.path().join("tools").join("tessdata"));
                }
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Some(prefix) = std::env::var_os("TESSDATA_PREFIX") {
            candidates.push(PathBuf::from(prefix));
        }
        // Versioned layouts nest one level down: Debian/Ubuntu ship
        // <distro>/tesseract-ocr/<ver>/tessdata, this host uses
        // /usr/share/tesseract/tessdata.
        for parent in [
            "/usr/share/tesseract-ocr",
            "/usr/share/tesseract",
            "/usr/local/share/tesseract-ocr",
            "/usr/local/share/tesseract",
        ] {
            if let Ok(entries) = fs::read_dir(parent) {
                for entry in entries.flatten() {
                    if entry.path().is_dir() {
                        candidates.push(entry.path().join("tessdata"));
                    }
                }
            }
        }
        for fixed in [
            "/usr/share/tesseract/tessdata",
            "/usr/local/share/tessdata",
            "/opt/homebrew/share/tessdata",
            "/usr/local/opt/tesseract/share/tessdata",
        ] {
            candidates.push(PathBuf::from(fixed));
        }
    }

    let Some(src) = candidates.into_iter().find(|dir| has_traineddata(dir)) else {
        return;
    };
    let target = dest.join("share").join("tessdata");
    if let Err(e) = fs::create_dir_all(&target) {
        eprintln!("warn: cannot create {}: {e}", target.display());
        return;
    }
    let Ok(entries) = fs::read_dir(&src) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "traineddata") {
            continue;
        }
        let Some(base) = path.file_name() else {
            continue;
        };
        if let Err(e) = fs::copy(&path, target.join(base)) {
            eprintln!("warn: cannot copy {}: {e}", path.display());
        }
    }
    println!("staged tessdata <- {}", src.display());
}

fn has_traineddata(dir: &Path) -> bool {
    dir.is_dir()
        && fs::read_dir(dir).is_ok_and(|mut entries| {
            entries.any(|entry| {
                entry.is_ok_and(|e| e.path().extension().is_some_and(|ext| ext == "traineddata"))
            })
        })
}

/// `tauri dev` runs the sidecar from `src-tauri/binaries/`, where the Rust
/// resolver looks for `<exe>/engines`; mirror the staged tree there.
fn mirror_for_dev_sidecar(dest: &Path) {
    let Some(parent) = dest.parent() else { return };
    if !parent.file_name().is_some_and(|n| n == "src-tauri") {
        return;
    }
    let dev_dir = parent.join("binaries").join("engines");
    let _ = fs::remove_dir_all(&dev_dir);
    match copy_dir_all(dest, &dev_dir) {
        Ok(()) => println!(
            "mirrored engines -> {} (dev sidecar lookup)",
            dev_dir.display()
        ),
        Err(e) => eprintln!("warn: {e}"),
    }
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
    use super::{
        is_glibc_core, parse_java_major, parse_ldd_paths, resolve_version, stage_backend_jar,
        stage_rust_server,
    };
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
    fn parse_ldd_paths_extracts_resolved_libraries() {
        let ldd = "\tlinux-vdso.so.1 (0x00007ffd)\n\
                   \tlibz.so.1 => /lib64/libz.so.1 (0x00007f00)\n\
                   \tlibc.so.6 => /lib64/libc.so.6 (0x00007f01)\n\
                   \t/lib64/ld-linux-x86-64.so.2 (0x00007f02)\n";
        let paths = parse_ldd_paths(ldd);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/lib64/libz.so.1"),
                PathBuf::from("/lib64/libc.so.6"),
                PathBuf::from("/lib64/ld-linux-x86-64.so.2"),
            ]
        );
    }

    #[test]
    fn glibc_core_is_never_bundled() {
        for name in [
            "libc.so.6",
            "libm.so.6",
            "ld-linux-x86-64.so.2",
            "libnss_files.so.2",
            "linux-vdso.so.1",
        ] {
            assert!(is_glibc_core(name), "{name} should be excluded");
        }
        for name in ["libgs.so.10", "libqpdf.so.30", "libstdc++.so.6"] {
            assert!(!is_glibc_core(name), "{name} should be bundled");
        }
    }

    #[test]
    fn stages_rust_server_under_externalbin_name() {
        let temporary = TemporaryDirectory::new();
        let source = temporary.0.join("stirling-server");
        let destination = temporary.0.join("binaries");
        fs::write(&source, "binary").unwrap();

        stage_rust_server(&[
            destination.to_string_lossy().into_owned(),
            source.to_string_lossy().into_owned(),
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

    #[test]
    fn stages_multiple_triples_in_one_call() {
        let temporary = TemporaryDirectory::new();
        let arm = temporary.0.join("stirling-server-arm");
        let intel = temporary.0.join("stirling-server-intel");
        let universal = temporary.0.join("stirling-server-universal");
        let destination = temporary.0.join("binaries");
        fs::write(&arm, "arm").unwrap();
        fs::write(&intel, "intel").unwrap();
        fs::write(&universal, "universal").unwrap();

        stage_rust_server(&[
            destination.to_string_lossy().into_owned(),
            format!("{}=aarch64-apple-darwin", arm.display()),
            format!("{}=x86_64-apple-darwin", intel.display()),
            format!("{}=universal-apple-darwin", universal.display()),
        ])
        .unwrap();

        for name in [
            "stirling-server-aarch64-apple-darwin",
            "stirling-server-x86_64-apple-darwin",
            "stirling-server-universal-apple-darwin",
        ] {
            assert!(destination.join(name).is_file(), "missing {name}");
        }
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 3);
    }
}
