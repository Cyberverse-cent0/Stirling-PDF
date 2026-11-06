use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::platform;
use crate::services::filesystem;
use crate::services::process::{find_executable, spawn_detached};

/// Builds the server, boots it detached, and exercises one endpoint per
/// migrated family against `rust/tests/fixtures`. Replaces the former
/// `tests/scripts/*.sh` shell harnesses.
pub fn runtime(root: &Path) -> Result<()> {
    let rust = filesystem::rust_dir(root);
    let port = smoke_port(18097);
    let base = format!("http://127.0.0.1:{port}");

    generate_fixtures(&rust)?;
    let server_pid = boot_server(&rust, port)?;
    let result = run_checks(&base, &rust.join("tests/fixtures"));
    platform::terminate(server_pid).ok();
    result
}

fn smoke_port(default: u16) -> u16 {
    std::env::var("SMOKE_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Shared harness for the per-domain smokes (`test general`,
/// `test security`): unit-test filters, fixture generation, a detached
/// server on `SMOKE_PORT`/`default_port`, then the caller's endpoint checks.
fn domain(
    root: &Path,
    default_port: u16,
    unit_filters: &[&str],
    checks: impl FnOnce(&mut Ctx) -> Result<()>,
) -> Result<()> {
    let rust = filesystem::rust_dir(root);
    let cargo = find_executable("cargo").context("cargo not found")?;
    for filter in unit_filters {
        let status = Command::new(&cargo)
            .args(["test", "--bin", "stirling-server", filter])
            .current_dir(&rust)
            .status()?;
        anyhow::ensure!(status.success(), "cargo test {filter} failed: {status}");
    }

    generate_fixtures(&rust)?;
    let port = smoke_port(default_port);
    let server_pid = boot_server(&rust, port)?;
    let result = (|| {
        let mut ctx = Ctx {
            base: format!("http://127.0.0.1:{port}"),
            fixtures: rust.join("tests/fixtures"),
            client: client()?,
            passed: 0,
        };
        checks(&mut ctx)?;
        println!("SMOKE PASSED ({} checks)", ctx.passed);
        Ok(())
    })();
    platform::terminate(server_pid).ok();
    result
}

/// `test general`: GeneralApi unit tests plus a live
/// `remove-image-pdf` round-trip.
pub fn general(root: &Path) -> Result<()> {
    domain(root, 18099, &["general::", "testkit::"], |ctx| {
        ctx.expect_pdf(
            "general/remove-image-pdf",
            "/api/v1/general/remove-image-pdf",
            &["images.pdf"],
            &[],
        )
    })
}

/// `test security`: SecurityApi unit tests plus live `sanitize-pdf`
/// and `get-info-on-pdf` checks.
pub fn security(root: &Path) -> Result<()> {
    domain(root, 18099, &["security::", "testkit::"], |ctx| {
        ctx.expect_pdf(
            "security/sanitize-pdf",
            "/api/v1/security/sanitize-pdf",
            &["sanitize-target.pdf"],
            &[
                ("removeXMPMetadata", "true"),
                ("removeMetadata", "true"),
                ("removeLinks", "true"),
                ("removeFonts", "true"),
            ],
        )?;
        ctx.post_form_json(
            "security/get-info-on-pdf",
            "/api/v1/security/get-info-on-pdf",
            &["basic.pdf"],
            &[],
            |v| v.get("Metadata").is_some(),
        )
    })
}

fn generate_fixtures(rust: &Path) -> Result<()> {
    // Same as the script's `|| true`: fixture generation is best-effort here;
    // missing fixtures surface as request failures below.
    let cargo = find_executable("cargo").context("cargo not found")?;
    let _ = Command::new(cargo)
        .args(["test", "--bin", "stirling-server", "testkit::fixture_tests"])
        .current_dir(rust)
        .status();
    Ok(())
}

fn boot_server(rust: &Path, port: u16) -> Result<u32> {
    let exe = match std::env::var_os("STIRLING_SERVER_BIN") {
        Some(p) => PathBuf::from(p),
        None => {
            let cargo = find_executable("cargo").context("cargo not found")?;
            let status = Command::new(cargo)
                .args(["build", "-p", "stirling-server"])
                .current_dir(rust)
                .status()?;
            anyhow::ensure!(status.success(), "stirling-server build failed");
            rust.join("target/debug").join(if cfg!(windows) {
                "stirling-server.exe"
            } else {
                "stirling-server"
            })
        }
    };

    std::fs::create_dir_all(filesystem::log_dir(rust))?;
    let log = filesystem::log_dir(rust).join("smoke-server.log");
    anyhow::ensure!(exe.is_file(), "{} missing", exe.display());

    let pid = spawn_detached(
        &exe,
        &[],
        Some(rust),
        &[("PORT".into(), port.to_string())],
        &log,
    )?;

    let client = client()?;
    let health = format!("http://127.0.0.1:{port}/health");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if platform::pid_alive(pid) {
            if client.get(&health).send().is_ok() {
                return Ok(pid);
            }
        } else {
            anyhow::bail!("server exited during startup; see {}", log.display());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("server did not start within 60s; see {}", log.display());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("failed to build HTTP client")
}

struct Ctx {
    base: String,
    fixtures: PathBuf,
    client: reqwest::blocking::Client,
    passed: usize,
}

impl Ctx {
    fn pass(&mut self, name: &str) {
        self.passed += 1;
        println!("PASS: {name}");
    }

    fn get(&self, path: &str) -> Result<reqwest::blocking::Response> {
        self.client
            .get(format!("{}{path}", self.base))
            .send()
            .with_context(|| format!("GET {path}"))
    }

    fn post_json(&self, path: &str, body: &str) -> Result<reqwest::blocking::Response> {
        self.client
            .post(format!("{}{path}", self.base))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .with_context(|| format!("POST {path}"))
    }

    fn post_form(
        &self,
        path: &str,
        files: &[&str],
        fields: &[(&str, &str)],
    ) -> Result<reqwest::blocking::Response> {
        self.post_form_named(path, "fileInput", files, fields)
    }

    /// `file_field` is the multipart name the endpoint binds — most routes
    /// use `fileInput`, the /api/v1/form/* family binds `file`.
    fn post_form_named(
        &self,
        path: &str,
        file_field: &str,
        files: &[&str],
        fields: &[(&str, &str)],
    ) -> Result<reqwest::blocking::Response> {
        let mut form = reqwest::blocking::multipart::Form::new();
        for f in files {
            let path = self.fixtures.join(f);
            form = form.part(
                file_field.to_string(),
                reqwest::blocking::multipart::Part::file(&path)
                    .with_context(|| format!("cannot read fixture {}", path.display()))?,
            );
        }
        for (k, v) in fields {
            form = form.text((*k).to_string(), (*v).to_string());
        }
        self.client
            .post(format!("{}{path}", self.base))
            .multipart(form)
            .send()
            .with_context(|| format!("POST {path}"))
    }

    fn expect_json(
        &mut self,
        name: &str,
        path: &str,
        check: impl Fn(&serde_json::Value) -> bool,
    ) -> Result<()> {
        let resp = self.get(path)?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().with_context(|| format!("{name}: bad JSON"))?;
        anyhow::ensure!(status.is_success(), "{}: HTTP {status}", name);
        anyhow::ensure!(check(&v), "{}: assertion failed", name);
        self.pass(name);
        Ok(())
    }

    fn post_form_json(
        &mut self,
        name: &str,
        path: &str,
        files: &[&str],
        fields: &[(&str, &str)],
        check: impl Fn(&serde_json::Value) -> bool,
    ) -> Result<()> {
        let resp = self.post_form(path, files, fields)?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().with_context(|| format!("{name}: bad JSON"))?;
        anyhow::ensure!(status.is_success(), "{}: HTTP {status}", name);
        anyhow::ensure!(check(&v), "{}: assertion failed", name);
        self.pass(name);
        Ok(())
    }

    fn expect_pdf(
        &mut self,
        name: &str,
        path: &str,
        files: &[&str],
        fields: &[(&str, &str)],
    ) -> Result<()> {
        let resp = self.post_form(path, files, fields)?;
        let status = resp.status();
        let bytes = resp.bytes().with_context(|| format!("{name}: read body"))?;
        anyhow::ensure!(status.is_success(), "{}: HTTP {status}", name);
        anyhow::ensure!(bytes.starts_with(b"%PDF"), "{}: body is not a PDF", name);
        self.pass(name);
        Ok(())
    }
}

fn run_checks(base: &str, fixtures: &Path) -> Result<()> {
    let mut ctx = Ctx {
        base: base.to_string(),
        fixtures: fixtures.to_path_buf(),
        client: client()?,
        passed: 0,
    };

    // --- platform / meta ---
    ctx.expect_json("info/version", "/api/v1/info/version", |v| {
        v.get("runtime").and_then(|r| r.as_str()) == Some("rust")
    })?;

    {
        let resp = ctx.post_json("/api/v1/ping", "{\"ping\":1}")?;
        let status = resp.status();
        let raw = resp.text()?;
        // The endpoint wraps the payload: {"echo":{"ping":1}}.
        anyhow::ensure!(status.is_success() && raw.contains("\"ping\":1"), "ping");
        ctx.pass("ping");
    }

    // --- analysis ---
    ctx.post_form_json(
        "analysis/basic-info",
        "/api/v1/analysis/basic-info",
        &["basic.pdf"],
        &[],
        |v| v.get("pageCount").and_then(|p| p.as_i64()) == Some(1),
    )?;
    ctx.post_form_json(
        "analysis/page-dimensions",
        "/api/v1/analysis/page-dimensions",
        &["basic.pdf"],
        &[],
        |v| {
            v.as_array()
                .is_some_and(|pages| pages.iter().any(|p| p.get("width").is_some()))
        },
    )?;

    // --- general ---
    ctx.expect_pdf(
        "general/rotate-pdf",
        "/api/v1/general/rotate-pdf",
        &["basic.pdf"],
        &[("angle", "90")],
    )?;
    ctx.expect_pdf(
        "general/merge-pdfs",
        "/api/v1/general/merge-pdfs",
        &["basic.pdf", "images.pdf"],
        &[],
    )?;

    // --- misc (async job path) ---
    {
        let resp = ctx.post_form(
            "/api/v1/misc/decompress-pdf?async=true",
            &["basic.pdf"],
            &[],
        )?;
        let v: serde_json::Value = resp.json()?;
        let job_id = v
            .get("jobId")
            .and_then(|j| j.as_str())
            .context("async job submission: no jobId")?
            .to_string();
        let status_path = format!("/api/v1/general/job/{job_id}");
        let result_path = format!("/api/v1/general/job/{job_id}/result");
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut complete = false;
        while Instant::now() < deadline {
            let resp = ctx.get(&status_path)?;
            let v: serde_json::Value = resp.json()?;
            if v.get("complete").and_then(|c| c.as_bool()) == Some(true) {
                complete = true;
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        anyhow::ensure!(complete, "job did not complete");
        let resp = ctx.get(&result_path)?;
        let bytes = resp.bytes()?;
        anyhow::ensure!(bytes.starts_with(b"%PDF"), "job result is not a PDF");
        ctx.pass("misc/decompress-pdf (async job)");
    }

    // --- security ---
    ctx.expect_pdf(
        "security/sanitize-pdf",
        "/api/v1/security/sanitize-pdf",
        &["sanitize-target.pdf"],
        &[("removeXMPMetadata", "true"), ("removeMetadata", "true")],
    )?;

    // --- form ---
    // /api/v1/form/* binds `file`, not `fileInput` (see handlers/form.rs).
    {
        let resp = ctx.post_form_named("/api/v1/form/fields", "file", &["basic.pdf"], &[])?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().with_context(|| "form/fields: bad JSON")?;
        anyhow::ensure!(status.is_success(), "form/fields: HTTP {status}");
        anyhow::ensure!(v.get("fields").is_some(), "form/fields: assertion failed");
        ctx.pass("form/fields");
    }

    // --- filter ---
    {
        let resp = ctx.post_form(
            "/api/v1/filter/filter-contains-text",
            &["basic.pdf"],
            &[("text", "definitely-not-present")],
        )?;
        let code = resp.status().as_u16();
        anyhow::ensure!(
            code == 204 || code == 200,
            "filter-contains-text status {code}"
        );
        ctx.pass("filter/filter-contains-text");
    }

    // --- fallback behaviour ---
    {
        let resp = ctx
            .client
            .post(format!("{}/api/v1/convert/pdf/word", ctx.base))
            .send()?;
        let code = resp.status().as_u16();
        // The route may now be registered and reject the empty body with 400;
        // the invariant is "no hang, no 5xx".
        anyhow::ensure!(
            (400..500).contains(&code),
            "unmigrated route returned {code}"
        );
        ctx.pass("unmigrated route 404s with proxy off");
    }
    {
        let resp = ctx.client.get(format!("{}/", ctx.base)).send()?;
        anyhow::ensure!(
            resp.status().as_u16() == 404,
            "SPA fallback returned {} without static dir",
            resp.status()
        );
        ctx.pass("non-api path 404s without static dir");
    }

    println!("SMOKE PASSED ({} checks)", ctx.passed);
    Ok(())
}
