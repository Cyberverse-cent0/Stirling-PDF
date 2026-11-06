use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::platform;
use crate::services::filesystem;
use crate::services::process::{find_executable, spawn_detached};

fn resolve_program(name: &str) -> Result<PathBuf> {
    let path = Path::new(name);
    if name.contains('/') || name.contains('\\') || path.extension().is_some() {
        anyhow::ensure!(path.is_file(), "executable not found: {name}");
        return Ok(path.to_path_buf());
    }
    find_executable(name).with_context(|| format!("executable not found on PATH: {name}"))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StartSpec {
    argv: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: Vec<(String, String)>,
}

fn spec_path(root: &Path, name: &str) -> PathBuf {
    filesystem::run_dir(root).join(format!("{name}.cmd"))
}

fn parse_env(pairs: &[String]) -> Result<Vec<(String, String)>> {
    pairs
        .iter()
        .map(|p| {
            p.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .with_context(|| format!("--env expects KEY=VALUE, got '{p}'"))
        })
        .collect()
}

/// `stirling start <name> <argv...>` — replaces `scripts/run.sh start`.
/// The full spec is stored in `.run/<name>.cmd` (JSON) so `restart` can
/// re-spawn it with the same cwd and environment.
pub fn start(
    root: &Path,
    name: &str,
    argv: &[String],
    cwd: Option<&str>,
    env: &[String],
) -> Result<()> {
    anyhow::ensure!(!argv.is_empty(), "start requires a command");
    let program = resolve_program(&argv[0])?;
    let args = &argv[1..];
    let env = parse_env(env)?;

    let workdir = match cwd {
        Some(c) => {
            let p = Path::new(c);
            let resolved = if p.is_absolute() {
                p.to_path_buf()
            } else {
                root.join(p)
            };
            anyhow::ensure!(
                resolved.is_dir(),
                "cwd does not exist: {}",
                resolved.display()
            );
            Some(resolved)
        }
        None => None,
    };

    let pid_path = filesystem::pid_file(root, name);
    if let Some(pid) = filesystem::read_pid(&pid_path) {
        if platform::pid_alive(pid) {
            anyhow::bail!("component '{name}' already running (pid {pid})");
        }
        filesystem::remove_pid(&pid_path);
    }

    std::fs::create_dir_all(filesystem::log_dir(root))?;
    let log_path = filesystem::log_dir(root).join(format!("{name}.log"));
    let pid = spawn_detached(
        &program,
        args,
        workdir.as_deref().or(Some(root)),
        &env,
        &log_path,
    )
    .with_context(|| format!("failed to start '{name}'"))?;

    filesystem::write_pid(&pid_path, pid)?;
    let spec = StartSpec {
        argv: argv.to_vec(),
        cwd: cwd.map(str::to_string),
        env,
    };
    std::fs::write(spec_path(root, name), serde_json::to_string(&spec)?)?;
    println!("started '{name}' (pid {pid}); log: {}", log_path.display());
    Ok(())
}

pub fn stop(root: &Path, names: &[String]) -> Result<()> {
    let targets: Vec<String> = if names.is_empty() {
        filesystem::component_names(root)
    } else {
        names.to_vec()
    };
    for name in &targets {
        let pid_path = filesystem::pid_file(root, name);
        match filesystem::read_pid(&pid_path) {
            Some(pid) if platform::pid_alive(pid) => {
                platform::terminate(pid)?;
                println!("stopped '{name}' (pid {pid})");
            }
            Some(_) => println!("'{name}' was not running"),
            None => println!("'{name}' has no pid file"),
        }
        filesystem::remove_pid(&pid_path);
        let _ = std::fs::remove_file(spec_path(root, name));
    }
    Ok(())
}

pub fn restart(root: &Path, names: &[String]) -> Result<()> {
    let targets: Vec<String> = if names.is_empty() {
        filesystem::component_names(root)
    } else {
        names.to_vec()
    };
    for name in &targets {
        let stored = std::fs::read_to_string(spec_path(root, name))
            .map_err(|_| anyhow::anyhow!("no stored command for '{name}'; use `stirling start`"))?;
        let spec: StartSpec =
            serde_json::from_str(&stored).with_context(|| format!("corrupt spec for '{name}'"))?;
        let env: Vec<String> = spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        stop(root, std::slice::from_ref(name))?;
        start(root, name, &spec.argv, spec.cwd.as_deref(), &env)?;
    }
    Ok(())
}

/// Polls `url` until a 2xx response or `seconds` elapse. Replaces the
/// `run.sh wait-for` helper used by `task dev:strangler`.
pub fn wait(seconds: u64, url: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Ok((status, _)) = http_get(url, Duration::from_secs(3)) {
            if (200..300).contains(&status) {
                println!("{url} is up (HTTP {status})");
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("timed out after {seconds}s waiting for {url}");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// `stirling ports free 8080 5173` — replaces `scripts/find-free-port.sh`.
/// For each preferred port emits the port itself if bindable, otherwise a
/// distinct ephemeral port in 20000-49999.
pub fn ports_free(preferred: &[u16]) -> Result<()> {
    use std::net::{Ipv4Addr, SocketAddr, TcpListener};

    fn port_free(port: u16) -> bool {
        TcpListener::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port)).is_ok()
    }

    fn random_free_port(taken: &[u16]) -> Option<u16> {
        // Ephemeral binds almost never land in 20000-49999 on Linux, so probe
        // the range directly with a bounded number of attempts.
        let mut seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
        for _ in 0..512 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let port = 20000 + (seed % 30000) as u16;
            if !taken.contains(&port) && port_free(port) {
                return Some(port);
            }
        }
        None
    }

    let mut picked: Vec<u16> = Vec::new();
    for &port in preferred {
        let chosen = if port_free(port) && !picked.contains(&port) {
            port
        } else {
            random_free_port(&picked).with_context(|| {
                format!("could not find a free port in 20000-49999 for preferred {port}")
            })?
        };
        picked.push(chosen);
        println!("{chosen}");
    }
    Ok(())
}

pub fn status(root: &Path) -> Result<()> {
    let names = filesystem::component_names(root);
    if names.is_empty() {
        println!("no managed components (.run/ is empty)");
        return Ok(());
    }
    println!("{:<16} {:<8} PID", "NAME", "STATE");
    for name in names {
        let pid = filesystem::read_pid(&filesystem::pid_file(root, &name));
        let (state, pid_s) = match pid {
            Some(p) if platform::pid_alive(p) => ("running", p.to_string()),
            Some(p) => ("stale", p.to_string()),
            None => ("unknown", "-".to_string()),
        };
        println!("{name:<16} {state:<8} {pid_s}");
    }
    Ok(())
}

pub fn logs(root: &Path, name: &str, lines: usize) -> Result<()> {
    let path = filesystem::log_dir(root).join(format!("{name}.log"));
    print!("{}", filesystem::tail(&path, lines)?);
    Ok(())
}

/// Minimal HTTP/1.1 GET — health endpoints are plain HTTP on localhost.
/// HTTPS URLs are rejected rather than silently failing on TLS.
fn http_get(url: &str, timeout: Duration) -> Result<(u16, String)> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("only http:// URLs are supported"))?;
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().context("bad port")?),
        None => (authority, 80),
    };
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect_timeout(&addr.parse()?, timeout)
        .with_context(|| format!("cannot connect to {addr}"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .context("malformed HTTP response")?;
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    Ok((status, body))
}

pub fn health(url: &str) -> Result<()> {
    match http_get(url, Duration::from_secs(5)) {
        Ok((status, body)) => {
            println!("{url} -> HTTP {status}");
            let snippet: String = body.chars().take(200).collect();
            if !snippet.trim().is_empty() {
                println!("{snippet}");
            }
            anyhow::ensure!((200..300).contains(&status), "unhealthy: HTTP {status}");
            Ok(())
        }
        Err(e) => {
            println!("{url} -> unreachable: {e}");
            Err(e.context("health check failed"))
        }
    }
}
