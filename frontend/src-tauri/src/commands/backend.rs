use crate::state::connection_state::{AppConnectionState, ConnectionMode};
use crate::utils::{add_log, app_data_dir};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::Manager;
use tauri_plugin_shell::ShellExt;

/// Removes shutdown markers left by previous sessions. The current path cannot
/// exist yet, so it is never a sweep target.
fn sweep_stale_shutdown_files(work_dir: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(work_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with(".backend-stop") {
            let _ = std::fs::remove_file(&path);
        }
    }
}

// Store backend process handle and port globally
static BACKEND_PROCESS: Mutex<Option<tauri_plugin_shell::process::CommandChild>> = Mutex::new(None);
static BACKEND_STARTING: Mutex<bool> = Mutex::new(false);
static BACKEND_PORT: Mutex<Option<u16>> = Mutex::new(None);
// Set by the event loop when the backend exits, so cleanup can wait for it.
static BACKEND_EXITED: AtomicBool = AtomicBool::new(true);
static BACKEND_SHUTDOWN_FILE: Mutex<Option<PathBuf>> = Mutex::new(None);

// Helper function to reset starting flag
fn reset_starting_flag() {
    let mut starting_guard = BACKEND_STARTING.lock().unwrap();
    *starting_guard = false;
}

// Extract port number from "Stirling-PDF running on port: PORT" log line
fn extract_port_from_running_log(log_line: &str) -> Option<u16> {
    // Look for pattern: "running on port: PORT"
    if let Some(start) = log_line.find("running on port: ") {
        let after_prefix = &log_line[start + 17..]; // Skip "running on port: "
                                                    // Take digits until whitespace or end of line
        let port_str: String = after_prefix
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        return port_str.parse::<u16>().ok();
    }
    None
}

// Check if backend is already running or starting
fn check_backend_status() -> Result<(), String> {
    // Check if backend is already running
    {
        let process_guard = BACKEND_PROCESS.lock().unwrap();
        if process_guard.is_some() {
            add_log("⚠️ Backend process already running, skipping start".to_string());
            return Err("Backend already running".to_string());
        }
    }

    // Check and set starting flag to prevent multiple simultaneous starts
    {
        let mut starting_guard = BACKEND_STARTING.lock().unwrap();
        if *starting_guard {
            add_log("⚠️ Backend already starting, skipping duplicate start".to_string());
            return Err("Backend startup already in progress".to_string());
        }
        *starting_guard = true;
    }

    Ok(())
}

// Find the bundled Rust backend binary. Tauri's `externalBin` mechanism
// ships it as `stirling-server`/`stirling-server-<target-triple>[.exe]`
// next to the main executable in packaged installs and under
// `src-tauri/binaries/` in `tauri dev`; the resource dir and its `bin/`
// child cover layouts that place sidecars with the resources instead.
// `STIRLING_SERVER_PATH` pins an explicit binary for development.
fn find_rust_server(resource_dir: &Path) -> Option<PathBuf> {
    if let Ok(pinned) = std::env::var("STIRLING_SERVER_PATH") {
        let pinned = pinned.trim();
        if !pinned.is_empty() {
            let path = PathBuf::from(pinned);
            if path.is_file() {
                return Some(path);
            }
            add_log(format!("⚠️ STIRLING_SERVER_PATH={} is not a file", pinned));
        }
    }
    let exe = if cfg!(windows) {
        "stirling-server.exe"
    } else {
        "stirling-server"
    };
    let manifest_binaries = Path::new(env!("CARGO_MANIFEST_DIR")).join("binaries");
    // Tauri ships sidecars next to the main executable (Linux `/usr/bin`,
    // macOS `Contents/MacOS`), which is neither the resource dir nor its
    // `bin/` child — without this probe packaged installs fall back to the
    // JAR path and fail on the missing JRE.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let dirs: Vec<PathBuf> = [resource_dir.to_path_buf(), resource_dir.join("bin")]
        .into_iter()
        .chain(exe_dir)
        .chain([manifest_binaries])
        .collect();
    let mut candidates: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                let lower = name.to_ascii_lowercase();
                if lower == exe
                    || (lower.starts_with("stirling-server-")
                        && (lower.ends_with(".exe") || !cfg!(windows)))
                {
                    candidates.push(entry.path());
                }
            }
        }
    }
    candidates.sort();
    // Dev staging can hold several triples at once (macOS stages
    // aarch64 + x86_64 + universal). Prefer the unsuffixed packaged name,
    // then this host's arch, then the fat universal binary — alphabetical
    // order alone would pick x86_64 on Apple Silicon.
    let host_triple = host_triple_hint();
    let name_of = |p: &&PathBuf| {
        p.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
    };
    candidates
        .iter()
        .find(|p| name_of(p) == exe)
        .or_else(|| {
            host_triple
                .as_deref()
                .and_then(|t| candidates.iter().find(|p| name_of(p).contains(t)))
        })
        .or_else(|| {
            candidates
                .iter()
                .find(|p| name_of(p).contains("universal-apple-darwin"))
        })
        .or_else(|| candidates.last())
        .cloned()
}

/// Best-effort `<arch>-<os>` triple for the running build, used only to rank
/// sidecar candidates — a miss just falls through to the universal/last pick.
fn host_triple_hint() -> Option<String> {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        _ => return None,
    };
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        "windows" => "pc-windows-msvc",
        _ => return None,
    };
    Some(format!("{arch}-{os}"))
}

// Normalize path to remove Windows UNC prefix
fn normalize_path(path: &Path) -> PathBuf {
    if cfg!(windows) {
        let path_str = path.to_string_lossy();
        if let Some(stripped) = path_str.strip_prefix(r"\\?\") {
            return PathBuf::from(stripped);
        }
    }
    path.to_path_buf()
}

fn migrate_legacy_workspace(legacy_dir: &Path, target_root: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(legacy_dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dest_path = target_root.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else if file_type.is_file() {
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src_path, &dest_path)?;
        }
    }

    Ok(())
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else if file_type.is_file() {
            std::fs::copy(&src_path, &dest_path)?;
        }
    }

    Ok(())
}

// Create, configure and run the Rust `stirling-server` sidecar. The binary
// reads PORT/HOST/STIRLING_PDF_TAURI_MODE/STIRLING_BASE_PATH from the
// environment rather than JVM -D properties.
fn run_stirling_server(app: &tauri::AppHandle, server_path: &Path) -> Result<(), String> {
    let app_data_dir = app_data_dir();
    let config_dir = app_data_dir.join("configs");
    let log_dir = app_data_dir.join("logs");
    let work_dir = app_data_dir.clone();
    let legacy_work_dir = app_data_dir.join("workspace");

    std::fs::create_dir_all(&app_data_dir).ok();
    std::fs::create_dir_all(&log_dir).ok();
    std::fs::create_dir_all(&work_dir).ok();
    std::fs::create_dir_all(&config_dir).ok();

    if legacy_work_dir.exists() {
        add_log(format!(
            "📦 Migrating legacy workspace from {}",
            legacy_work_dir.display()
        ));
        if let Err(err) = migrate_legacy_workspace(&legacy_work_dir, &app_data_dir) {
            add_log(format!("⚠️ Failed to migrate legacy workspace: {}", err));
        } else {
            let _ = std::fs::remove_dir_all(&legacy_work_dir);
        }
    }

    add_log(format!("📁 App data directory: {}", app_data_dir.display()));

    let shutdown_file = work_dir.join(format!(
        ".backend-stop-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0)
    ));
    sweep_stale_shutdown_files(&work_dir, &shutdown_file);
    *BACKEND_SHUTDOWN_FILE.lock().unwrap() = Some(shutdown_file.clone());

    // Engines bundled as installer resources live in `<resource>/engines`;
    // the Rust side searches that dir (and PATH) for gs/qpdf/etc. `tauri
    // dev` stages the same tree under src-tauri/binaries/engines next to
    // the sidecar copy, so both roots feed the search path.
    let engines_roots: Vec<PathBuf> = [
        app.path()
            .resource_dir()
            .ok()
            .map(|dir| dir.join("engines")),
        Some(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("binaries")
                .join("engines"),
        ),
    ]
    .into_iter()
    .flatten()
    .filter(|dir| dir.is_dir())
    .collect();
    let engines_dir = std::env::join_paths(&engines_roots)
        .map(|joined| joined.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut sidecar_command = app
        .shell()
        .command(server_path.to_str().unwrap())
        .current_dir(&work_dir)
        .env("TAURI_PARENT_PID", std::process::id().to_string())
        .env("PORT", "0")
        .env("HOST", "127.0.0.1")
        .env("STIRLING_PDF_TAURI_MODE", "true")
        .env("STIRLING_BASE_PATH", app_data_dir.to_str().unwrap())
        .env("STIRLING_ENGINES_DIR", engines_dir)
        .env(
            "STIRLING_PDF_SHUTDOWN_FILE",
            shutdown_file.to_str().unwrap(),
        );

    // The staged engine binaries carry their shared-library closure in
    // engines/lib (see build-tools `stage-engines`) — the loader must
    // search it or every bundled engine fails to exec. GS_LIB points gs
    // at its staged Resource/Init tree the same way. Both are inherited by spawned engines.
    if !engines_roots.is_empty() {
        let lib_dirs: Vec<PathBuf> = engines_roots
            .iter()
            .map(|root| root.join("lib"))
            .filter(|dir| dir.is_dir())
            .collect();
        let gs_dirs: Vec<PathBuf> = engines_roots
            .iter()
            .map(|root| root.join("share").join("ghostscript"))
            .filter(|dir| dir.is_dir())
            .collect();
        let tessdata_dirs: Vec<PathBuf> = engines_roots
            .iter()
            .map(|root| root.join("share").join("tessdata"))
            .filter(|dir| dir.is_dir())
            .collect();
        #[cfg(target_os = "linux")]
        if !lib_dirs.is_empty() {
            let mut paths = lib_dirs.clone();
            if let Some(existing) = std::env::var_os("LD_LIBRARY_PATH") {
                paths.extend(std::env::split_paths(&existing));
            }
            if let Ok(joined) = std::env::join_paths(paths) {
                sidecar_command = sidecar_command.env("LD_LIBRARY_PATH", joined);
            }
        }
        #[cfg(target_os = "macos")]
        if !lib_dirs.is_empty() {
            if let Ok(joined) = std::env::join_paths(&lib_dirs) {
                sidecar_command = sidecar_command.env("DYLD_FALLBACK_LIBRARY_PATH", joined);
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let _ = &lib_dirs;
        if !gs_dirs.is_empty() {
            // gs splits GS_LIB on the platform path separator (';' on
            // Windows), same convention as PATH-style env vars.
            if let Ok(joined) = std::env::join_paths(&gs_dirs) {
                sidecar_command = sidecar_command.env("GS_LIB", joined);
            }
        }
        // TESSDATA_PREFIX is the fallback the Rust backend consults after
        // SYSTEM_TESSDATADIR/settings, so a user-configured tessdata dir
        // still wins over the bundled one.
        if !tessdata_dirs.is_empty() {
            if let Ok(joined) = std::env::join_paths(&tessdata_dirs) {
                sidecar_command = sidecar_command.env("TESSDATA_PREFIX", joined);
            }
        }
    }

    add_log("⚙️ Starting Rust backend sidecar...".to_string());

    let (rx, child) = sidecar_command.spawn().map_err(|e| {
        let error_msg = format!("❌ Failed to spawn stirling-server: {}", e);
        add_log(error_msg.clone());
        error_msg
    })?;

    BACKEND_EXITED.store(false, Ordering::Release);
    {
        let mut process_guard = BACKEND_PROCESS.lock().unwrap();
        *process_guard = Some(child);
    }

    add_log("✅ Rust backend started, monitoring output...".to_string());
    monitor_backend_output(rx);
    Ok(())
}

// Monitor backend output in a separate task
fn monitor_backend_output(
    mut rx: tauri::async_runtime::Receiver<tauri_plugin_shell::process::CommandEvent>,
) {
    tokio::spawn(async move {
        let mut _startup_detected = false;
        let mut error_count = 0;

        while let Some(event) = rx.recv().await {
            match event {
                tauri_plugin_shell::process::CommandEvent::Stdout(output) => {
                    let output_str = String::from_utf8_lossy(&output);
                    // Strip exactly one trailing newline to avoid double newlines
                    let output_str = output_str.strip_suffix('\n').unwrap_or(&output_str);
                    add_log(format!("📤 Backend: {}", output_str));

                    // Look for actual runtime port from web server initialization
                    // Format: "Stirling-PDF running on port: PORT"
                    if output_str.contains("running on port:") {
                        _startup_detected = true;
                        if let Some(port) = extract_port_from_running_log(output_str) {
                            let mut port_guard = BACKEND_PORT.lock().unwrap();
                            *port_guard = Some(port);
                            add_log(format!("🎉 Backend started on port: {}", port));
                            add_log(format!("🔌 Navigate to: http://127.0.0.1:{}/", port));
                        }
                    }

                    if output_str.contains("Started SPDFApplication") {
                        _startup_detected = true;
                        add_log(format!("🎉 Backend startup completed: {}", output_str));
                    }
                }
                tauri_plugin_shell::process::CommandEvent::Stderr(output) => {
                    let output_str = String::from_utf8_lossy(&output);
                    // Strip exactly one trailing newline to avoid double newlines
                    let output_str = output_str.strip_suffix('\n').unwrap_or(&output_str);
                    add_log(format!("📥 Backend Error: {}", output_str));

                    // Look for error indicators
                    if output_str.contains("ERROR")
                        || output_str.contains("Exception")
                        || output_str.contains("FATAL")
                    {
                        error_count += 1;
                        add_log(format!("⚠️ Backend error #{}: {}", error_count, output_str));
                    }

                    // Look for specific common issues
                    if output_str.contains("Address already in use") {
                        add_log(
                            "🚨 CRITICAL: Port 8080 is already in use by another process!"
                                .to_string(),
                        );
                    }
                    if output_str.contains("java.lang.ClassNotFoundException") {
                        add_log("🚨 CRITICAL: Missing Java dependencies!".to_string());
                    }
                    if output_str.contains("java.io.FileNotFoundException") {
                        add_log("🚨 CRITICAL: Required file not found!".to_string());
                    }
                }
                tauri_plugin_shell::process::CommandEvent::Error(error) => {
                    add_log(format!("❌ Backend process error: {}", error));
                }
                tauri_plugin_shell::process::CommandEvent::Terminated(payload) => {
                    add_log(format!(
                        "💀 Backend terminated with code: {:?}",
                        payload.code
                    ));
                    if let Some(code) = payload.code {
                        match code {
                            0 => println!("✅ Process terminated normally"),
                            1 => println!("❌ Process terminated with generic error"),
                            2 => println!("❌ Process terminated due to misuse"),
                            126 => println!("❌ Command invoked cannot execute"),
                            127 => println!("❌ Command not found"),
                            128 => println!("❌ Invalid exit argument"),
                            130 => println!("❌ Process terminated by Ctrl+C"),
                            _ => println!("❌ Process terminated with code: {}", code),
                        }
                    }
                    BACKEND_EXITED.store(true, Ordering::Release);
                    let mut process_guard = BACKEND_PROCESS.lock().unwrap();
                    *process_guard = None;
                }
                _ => {
                    println!("🔍 Unknown command event: {:?}", event);
                }
            }
        }

        if error_count > 0 {
            println!(
                "⚠️ Backend process ended with {} errors detected",
                error_count
            );
        }
    });
}

// Command to start the backend with bundled JRE
#[tauri::command]
pub async fn start_backend(
    app: tauri::AppHandle,
    connection_state: tauri::State<'_, AppConnectionState>,
) -> Result<String, String> {
    add_log(
        "🚀 start_backend() called - Attempting to start backend with bundled JRE...".to_string(),
    );

    // Check connection mode
    let mode = {
        let state = connection_state.0.lock().map_err(|e| {
            let error_msg = format!("❌ Failed to access connection state: {}", e);
            add_log(error_msg.clone());
            error_msg
        })?;
        state.mode.clone()
    };

    match mode {
        ConnectionMode::SaaS => {
            add_log("☁️ Running in SaaS mode - starting local backend".to_string());
        }
        ConnectionMode::SelfHosted => {
            add_log("🌐 Running in Self-Hosted mode - starting local backend (for hybrid execution support)".to_string());
        }
        ConnectionMode::Local => {
            add_log("💻 Running in Local-only mode - starting local backend".to_string());
        }
    }

    // Check if backend is already running or starting
    if let Err(msg) = check_backend_status() {
        return Ok(msg);
    }

    // The desktop bundle ships only the Rust `stirling-server` sidecar;
    // there is no Java fallback to select.
    let resource_dir = app.path().resource_dir().map_err(|e| {
        let error_msg = format!("❌ Failed to get resource directory: {}", e);
        add_log(error_msg.clone());
        reset_starting_flag();
        error_msg
    })?;

    add_log(format!("🔍 Resource directory: {:?}", resource_dir));

    let server_path = find_rust_server(&resource_dir)
        .ok_or_else(|| "stirling-server binary not found in app resources".to_string())
        .inspect_err(|e| {
            add_log(format!("❌ {}", e));
            reset_starting_flag();
        })?;
    let normalized = normalize_path(&server_path);
    add_log(format!("🦀 Using Rust backend: {:?}", normalized));
    run_stirling_server(&app, &normalized).inspect_err(|_| reset_starting_flag())?;

    reset_starting_flag();
    add_log("✅ Backend startup sequence completed, starting flag cleared".to_string());

    Ok("Backend startup initiated successfully (stirling-server)".to_string())
}

// Get the dynamically assigned backend port
#[tauri::command]
pub fn get_backend_port() -> Option<u16> {
    let port_guard = BACKEND_PORT.lock().unwrap();
    *port_guard
}

// Stop the backend on app exit: request a graceful Spring shutdown, then force.
pub fn cleanup_backend() {
    let child = BACKEND_PROCESS.lock().unwrap().take();
    let Some(child) = child else { return };
    let pid = child.pid();
    add_log(format!(
        "App shutting down, stopping backend (PID: {})",
        pid
    ));

    if let Some(path) = BACKEND_SHUTDOWN_FILE.lock().unwrap().clone() {
        if let Err(e) = std::fs::write(&path, b"stop") {
            add_log(format!("Could not write shutdown file: {}", e));
        }
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    while !BACKEND_EXITED.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }

    if BACKEND_EXITED.load(Ordering::Acquire) {
        add_log(format!("Backend (PID: {}) stopped gracefully", pid));
    } else {
        add_log(format!(
            "Backend (PID: {}) did not stop in time, killing",
            pid
        ));
        let _ = child.kill();
    }
}
