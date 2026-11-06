//! Port of Java's `EndpointConfiguration`
//! (`app/common/.../SPDF/config/EndpointConfiguration.java`): the
//! endpoint/group enablement model that `EndpointInterceptor` enforces with
//! a 403. Rust reproduces the same rules so a `settings.yml`
//! `endpoints.toRemove`/`groupsToRemove` (or the equivalent env vars)
//! disables endpoints identically on both backends.
//!
//! Deliberate divergences, all matching what Java computes in an OSS build:
//! - `hasPaidPlan()` is always false — no license service exists in Rust, so
//!   the `enterprise` group (unpopulated in Java anyway) can never enable.
//! - `pdfUaAvailable` is false — the PDF/UA tagger is proprietary-only, so
//!   `pdf-to-ua`/`accessibility-report` start disabled exactly as Java does
//!   without the proprietary module.
//! - `form-detection` starts disabled (DEPENDENCY) — only the proprietary
//!   `FormDetectionModelManager` can enable it.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use crate::settings::Settings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisableReason {
    Config,
    Dependency,
    /// The endpoint key is known but this backend has no implementation —
    /// there is no Java proxy to fall through to, so it can never work.
    /// Serializes as the frontend's existing `NOT_SUPPORTED_LOCALLY`.
    NotSupported,
    Unknown,
}

/// Groups whose members may be satisfied by *any* enabled member — disabling
/// one does not disable the endpoint while an alternative remains. Mirrors
/// `isToolGroup`.
const TOOL_GROUPS: &[&str] = &[
    "qpdf",
    "OCRmyPDF",
    "Ghostscript",
    "LibreOffice",
    "tesseract",
    "CLI",
    "Python",
    "OpenCV",
    "Unoconvert",
    "Java",
    "Javascript",
    "Weasyprint",
    "Pdftohtml",
    "ImageMagick",
    "rar",
    "Calibre",
    "FFmpeg",
    "veraPDF",
];

/// `ExternalAppDepConfig.commandToGroupMapping`, restricted to the binary
/// names this backend's `EngineSpec` call sites and Java's map agree on.
/// Five of these resolve through `system.customPaths.operations`
/// (`RuntimePathConfig`); the rest probe the bare `PATH` name.
/// `ffmpeg` stays unprobed — Java commented its mapping out over CVEs.
const ENGINE_PROBES: &[(&str, &str)] = &[
    ("gs", "Ghostscript"),
    ("qpdf", "qpdf"),
    ("soffice", "LibreOffice"),
    ("unoconvert", "Unoconvert"),
    ("weasyprint", "Weasyprint"),
    ("pdftohtml", "Pdftohtml"),
    ("tesseract", "tesseract"),
    ("ocrmypdf", "OCRmyPDF"),
    ("rar", "rar"),
    ("magick", "ImageMagick"),
    ("ebook-convert", "Calibre"),
];

/// Minimum engine versions Java enforces after a successful probe
/// (`ExternalAppDepConfig.checkDependencyAndDisableGroup`).
const MIN_ENGINE_VERSIONS: &[(&str, &str)] = &[("weasyprint", "58.0"), ("qpdf", "12.0.0")];

const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn is_tool_group(group: &str) -> bool {
    TOOL_GROUPS.contains(&group)
}

/// `EndpointConfiguration.init()` group membership, transcribed verbatim.
/// An endpoint may appear in several groups (functional + tool); membership
/// in a disabled *functional* group disables it, membership in tool groups
/// only matters through the alternatives/single-dependency rules.
fn group_table() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        (
            "PageOps",
            &[
                "remove-pages",
                "merge-pdfs",
                "split-pages",
                "rearrange-pages",
                "rotate-pdf",
                "auto-rotate-pdf",
                "multi-page-layout",
                "booklet-imposition",
                "scale-pages",
                "crop",
                "pdf-to-single-page",
                "auto-split-pdf",
                "split-by-size-or-count",
                "overlay-pdf",
                "split-pdf-by-sections",
                "split-pdf-by-chapters",
                "add-page-numbers",
                "extract-pages",
            ],
        ),
        (
            "Convert",
            &[
                "pdf-to-img",
                "img-to-pdf",
                "pdf-to-pdfa",
                "pdf-to-ua",
                "file-to-pdf",
                "pdf-to-word",
                "pdf-to-presentation",
                "pdf-to-text",
                "pdf-to-html",
                "pdf-to-xml",
                "html-to-pdf",
                "url-to-pdf",
                "markdown-to-pdf",
                "pdf-to-csv",
                "pdf-to-markdown",
                "eml-to-pdf",
                "pdf-to-epub",
                "ebook-to-pdf",
                "pdf-to-vector",
                "vector-to-pdf",
                "pdf-to-video",
                "cbz-to-pdf",
                "pdf-to-cbz",
                "pdf-to-json",
                "json-to-pdf",
                "pdf-to-rtf",
            ],
        ),
        (
            "Security",
            &[
                "add-password",
                "remove-password",
                "change-permissions",
                "add-watermark",
                "cert-sign",
                "remove-cert-sign",
                "sanitize-pdf",
                "timestamp-pdf",
                "auto-redact",
                "validate-signature",
                "add-stamp",
                "unlock-pdf-forms",
                "redact",
                "verify-pdf",
                "accessibility-report",
                "validate-compliance",
                "sign",
            ],
        ),
        (
            "Other",
            &[
                "ocr-pdf",
                "extract-images",
                "update-metadata",
                "flatten",
                "remove-blanks",
                "remove-annotations",
                "get-info-on-pdf",
                "add-attachments",
                "batch-process-attachments",
                "list-attachments",
                "extract-single-attachment",
                "replace-invert-pdf",
                "edit-table-of-contents",
                "add-image",
                "compare",
                "view-pdf",
                "multi-tool",
                "fields",
                "modify-fields",
                "delete-fields",
                "fill",
            ],
        ),
        (
            "Advance",
            &[
                "compress-pdf",
                "extract-image-scans",
                "repair",
                "auto-rename",
                "scanner-effect",
                "overlay-pdf",
                "adjust-contrast",
            ],
        ),
        ("Automation", &["handleData", "automate", "pipeline"]),
        ("DocParse", &["ingest"]),
        ("DeveloperTools", &["show-javascript"]),
        (
            "DeveloperDocs",
            &[
                "dev-api-docs",
                "dev-folder-scanning-docs",
                "dev-sso-guide-docs",
                "dev-airgapped-docs",
            ],
        ),
        (
            "CLI",
            &[
                "compress-pdf",
                "extract-image-scans",
                "repair",
                "pdf-to-pdfa",
                "file-to-pdf",
                "pdf-to-word",
                "pdf-to-presentation",
                "pdf-to-html",
                "pdf-to-xml",
                "ocr-pdf",
                "html-to-pdf",
                "url-to-pdf",
                "pdf-to-rtf",
            ],
        ),
        ("Python", &["html-to-pdf", "url-to-pdf", "file-to-pdf"]),
        (
            "LibreOffice",
            &[
                "file-to-pdf",
                "pdf-to-word",
                "pdf-to-presentation",
                "pdf-to-rtf",
                "pdf-to-html",
                "pdf-to-xml",
                "pdf-to-pdfa",
            ],
        ),
        ("Unoconvert", &["file-to-pdf"]),
        (
            "Java",
            &[
                "merge-pdfs",
                "remove-pages",
                "split-pages",
                "rearrange-pages",
                "rotate-pdf",
                "pdf-to-img",
                "img-to-pdf",
                "add-password",
                "remove-password",
                "change-permissions",
                "add-watermark",
                "add-stamp",
                "add-image",
                "extract-images",
                "update-metadata",
                "cert-sign",
                "remove-cert-sign",
                "multi-page-layout",
                "booklet-imposition",
                "scale-pages",
                "add-page-numbers",
                "auto-rename",
                "auto-split-pdf",
                "sanitize-pdf",
                "timestamp-pdf",
                "crop",
                "get-info-on-pdf",
                "pdf-to-single-page",
                "markdown-to-pdf",
                "show-javascript",
                "auto-redact",
                "redact",
                "pdf-to-csv",
                "split-by-size-or-count",
                "overlay-pdf",
                "split-pdf-by-sections",
                "split-pdf-by-chapters",
                "remove-blanks",
                "remove-annotations",
                "pdf-to-text",
                "pdf-to-markdown",
                "add-attachments",
                "batch-process-attachments",
                "list-attachments",
                "extract-single-attachment",
                "compress-pdf",
                "cbz-to-pdf",
                "pdf-to-cbz",
                "pdf-to-json",
                "json-to-pdf",
                "pdf-to-video",
                "verify-pdf",
                "pdf-to-ua",
                "accessibility-report",
                "validate-compliance",
                "flatten",
                "unlock-pdf-forms",
                "validate-signature",
                "edit-table-of-contents",
                "pdf-to-epub",
                "ebook-to-pdf",
                "eml-to-pdf",
                "handleData",
                "form-detection",
            ],
        ),
        ("rar", &["pdf-to-cbr"]),
        (
            "Javascript",
            &["rearrange-pages", "sign", "compare", "adjust-contrast"],
        ),
        ("qpdf", &["repair", "compress-pdf"]),
        (
            "Ghostscript",
            &[
                "repair",
                "compress-pdf",
                "crop",
                "replace-invert-pdf",
                "scanner-effect",
                "pdf-to-vector",
                "vector-to-pdf",
            ],
        ),
        ("ImageMagick", &["compress-pdf"]),
        ("tesseract", &["ocr-pdf", "auto-rotate-pdf"]),
        ("OCRmyPDF", &["ocr-pdf"]),
        (
            "Weasyprint",
            &["html-to-pdf", "url-to-pdf", "markdown-to-pdf", "eml-to-pdf"],
        ),
        (
            "veraPDF",
            &[
                "verify-pdf",
                "pdf-to-ua",
                "accessibility-report",
                "validate-compliance",
            ],
        ),
        ("Pdftohtml", &["pdf-to-html", "pdf-to-markdown"]),
        ("Calibre", &["pdf-to-epub", "ebook-to-pdf"]),
    ]
}

/// Endpoint keys Java implements but this backend has no route for —
/// verified against `handlers::router` and the Java controller inventory
/// (`migration/reports/desktop-phase-b-endpoint-audit.md`). Applied
/// unconditionally: with no Java proxy, an unrouted endpoint 404s for
/// every deployment, so availability must say so.
///
/// Deliberately absent: keys with no Java controller either
/// (`extract-pages`, `compare`, `multi-tool`, `adjust-contrast`,
/// `automate`, `pipeline`, `change-permissions`, `remove-annotations`,
/// `pdf-to-json`, `json-to-pdf`, `pdf-to-rtf`, `pdf-to-video`, `sign`).
/// Several are frontend-composed tools that decompose into routed calls —
/// disabling the key would hide a working tool.
const UNSUPPORTED_ENDPOINTS: &[&str] = &[
    // CMS/PKCS#7 signing, RFC 3161 timestamps, signature validation —
    // needs a crypto stack Rust does not have.
    "cert-sign",
    "timestamp-pdf",
    "validate-signature",
    "verify-pdf",
    "validate-compliance",
    // Proprietary modules with no Rust port.
    "ingest",
    "restore-pdf-protection",
    // Proprietary/AI/integration surface queried by path-derived keys.
    "orchestrate",
    "tools",
    "external-api-call",
    "purview-apply-label",
    "purview-read-label",
];

/// `addEndpointAlternative` calls from `init()`.
fn alternatives() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        ("repair", &["qpdf", "Ghostscript"]),
        ("compress-pdf", &["qpdf", "Ghostscript", "Java"]),
        ("crop", &["Ghostscript", "Java"]),
        ("ocr-pdf", &["tesseract", "OCRmyPDF"]),
        ("file-to-pdf", &["LibreOffice", "Unoconvert"]),
        ("pdf-to-html", &["LibreOffice", "Pdftohtml"]),
        ("pdf-to-markdown", &["Pdftohtml"]),
    ]
}

struct EndpointConfig {
    /// group name → member endpoint keys
    groups: HashMap<&'static str, HashSet<&'static str>>,
    /// endpoint key → alternative tool groups that can satisfy it
    alternatives: HashMap<&'static str, HashSet<&'static str>>,
    /// explicitly disabled endpoints (settings + env + built-in rules)
    disabled_endpoints: HashMap<String, DisableReason>,
    /// disabled group names
    disabled_groups: HashSet<String>,
    group_reasons: HashMap<String, DisableReason>,
    /// Raw request-path matchers from `RUST_DISABLED_ENDPOINTS`: exact
    /// paths and `prefix*` globs, checked against the URI before
    /// endpoint-key translation — the pre-settings guard's semantics.
    disabled_paths: Vec<(String, bool)>,
}

fn normalize(endpoint: &str) -> &str {
    endpoint.strip_prefix('/').unwrap_or(endpoint)
}

impl EndpointConfig {
    fn new(settings: &Settings) -> Self {
        let mut groups: HashMap<&'static str, HashSet<&'static str>> = HashMap::new();
        for (group, endpoints) in group_table() {
            groups.insert(group, endpoints.iter().copied().collect());
        }
        let alternatives = alternatives()
            .iter()
            .map(|&(ep, tgs)| (ep, tgs.iter().copied().collect()))
            .collect();

        let mut config = Self {
            groups,
            alternatives,
            disabled_endpoints: HashMap::new(),
            disabled_groups: HashSet::new(),
            group_reasons: HashMap::new(),
            disabled_paths: Vec::new(),
        };

        // `processEnvironmentConfigs`: settings.yml lists, then the same
        // env-var spellings Spring relaxed binding accepts
        // (`ENDPOINTS_TOREMOVE`/`ENDPOINTS_GROUPSTOREMOVE`), then the
        // Rust-only `RUST_DISABLED_ENDPOINTS` escape hatch (paths or
        // `prefix*` globs, resolved to endpoint keys below).
        for ep in &settings.endpoints.to_remove {
            config.disable_endpoint(ep, DisableReason::Config);
        }
        for group in &settings.endpoints.groups_to_remove {
            config.disable_group(group, DisableReason::Config);
        }
        if let Ok(list) = std::env::var("ENDPOINTS_TOREMOVE") {
            for ep in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                config.disable_endpoint(ep, DisableReason::Config);
            }
        }
        if let Ok(list) = std::env::var("ENDPOINTS_GROUPSTOREMOVE") {
            for group in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                config.disable_group(group, DisableReason::Config);
            }
        }
        if let Ok(list) = std::env::var("RUST_DISABLED_ENDPOINTS") {
            for pattern in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                config.disable_pattern(pattern);
            }
        }

        // Built-in dependency rules from `processEnvironmentConfigs`.
        // Rust never ships the proprietary PDF/UA tagger or the form-detection
        // model manager.
        config.disable_endpoint("pdf-to-ua", DisableReason::Dependency);
        config.disable_endpoint("accessibility-report", DisableReason::Dependency);
        config.disable_endpoint("form-detection", DisableReason::Dependency);
        for endpoint in UNSUPPORTED_ENDPOINTS {
            config.disable_endpoint(endpoint, DisableReason::NotSupported);
        }
        if !settings.system.enable_url_to_pdf {
            config.disable_endpoint("url-to-pdf", DisableReason::Config);
        }
        config
    }

    /// `ExternalAppDepConfig.checkDependencies`: probe every engine binary
    /// in parallel and disable its tool group when absent or under-versioned,
    /// so `api_guard` 403s the way Java's interceptor does. Java also runs a
    /// Calibre sandbox-confinement check that only applies inside its Docker
    /// image; there is no Rust equivalent to confine, so it is skipped.
    fn probe_engines(&mut self, settings: &Settings) {
        let operations = &settings.system.custom_paths.operations;
        let probes: Vec<(String, &str)> = ENGINE_PROBES
            .iter()
            .map(|&(cmd, group)| {
                (
                    operations.for_program(cmd).unwrap_or(cmd).to_string(),
                    group,
                )
            })
            .collect();
        let results: Vec<(String, &str, ProbeOutcome)> = std::thread::scope(|scope| {
            let handles: Vec<_> = probes
                .iter()
                .map(|(cmd, group)| {
                    let cmd = cmd.clone();
                    scope.spawn(move || {
                        let outcome = probe_engine(&cmd);
                        (cmd, *group, outcome)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("engine probe thread panicked"))
                .collect()
        });
        for (cmd, group, outcome) in results {
            match outcome {
                ProbeOutcome::Missing => {
                    tracing::warn!("Missing dependency: {cmd} - Disabling group: {group}");
                    self.disable_group(group, DisableReason::Dependency);
                }
                ProbeOutcome::Available => {}
                ProbeOutcome::Version(version) => {
                    if let Some((_, min)) = MIN_ENGINE_VERSIONS
                        .iter()
                        .find(|(name, _)| cmd.contains(name))
                    {
                        if version_lt(&version, min) {
                            tracing::warn!(
                                "{cmd} version {version} is below required {min} - \
                                 Disabling group: {group}"
                            );
                            self.disable_group(group, DisableReason::Dependency);
                        }
                    }
                }
            }
        }
        self.probe_python();
    }

    /// `checkPythonAndOpenCV`: no interpreter disables `Python` and `OpenCV`;
    /// a missing `cv2` import disables `OpenCV` only.
    fn probe_python(&mut self) {
        let python = ["python3", "python"]
            .iter()
            .find(|cmd| engine_available(cmd))
            .copied();
        let Some(python) = python else {
            tracing::warn!(
                "Missing dependency: Python interpreter not found on PATH - \
                 Disabling groups: Python, OpenCV"
            );
            self.disable_group("Python", DisableReason::Dependency);
            self.disable_group("OpenCV", DisableReason::Dependency);
            return;
        };
        let cv2 = crate::convert::run(
            &crate::convert::EngineSpec::new(python)
                .args(["-c", "import cv2"])
                .timeout(PROBE_TIMEOUT),
        );
        let ok = cv2
            .map(|out| !out.timed_out && out.code == Some(0))
            .unwrap_or(false);
        if !ok {
            tracing::warn!("OpenCV not available in Python - Disabling group: OpenCV");
            self.disable_group("OpenCV", DisableReason::Dependency);
        }
    }

    fn disable_endpoint(&mut self, endpoint: &str, reason: DisableReason) {
        self.disabled_endpoints
            .insert(normalize(endpoint).to_string(), reason);
    }

    fn disable_group(&mut self, group: &str, reason: DisableReason) {
        let group = normalize(group);
        self.disabled_groups.insert(group.to_string());
        self.group_reasons.insert(group.to_string(), reason);
        // Functional groups cascade to members; tool groups deliberately do
        // not — their endpoints survive on alternatives.
        if !is_tool_group(group) {
            if let Some(members) = self.groups.get(group) {
                let members: Vec<&'static str> = members.iter().copied().collect();
                for ep in members {
                    self.disable_endpoint(ep, reason);
                }
            }
        }
    }

    /// `RUST_DISABLED_ENDPOINTS` accepts bare endpoint keys, exact request
    /// paths, and `prefix*` globs. Globs and `/api/` paths are kept as raw
    /// URI matchers; bare keys go through the endpoint table.
    fn disable_pattern(&mut self, pattern: &str) {
        if pattern.ends_with('*') {
            self.disabled_paths
                .push((pattern.trim_end_matches('*').to_string(), true));
        } else if pattern.starts_with("/api/") {
            self.disabled_paths.push((pattern.to_string(), false));
        } else {
            self.disable_endpoint(pattern, DisableReason::Config);
        }
    }

    /// `EndpointConfiguration.isEndpointEnabled`, rules 1–4 in order.
    fn is_enabled(&self, endpoint: &str) -> bool {
        let endpoint = normalize(endpoint);
        // `enterprise` is unpopulated in OSS and there is no paid plan in
        // Rust; the check is kept so a future populated group can't leak.
        if self
            .groups
            .get("enterprise")
            .is_some_and(|eps| eps.contains(endpoint))
        {
            return false;
        }
        if self.disabled_endpoints.contains_key(endpoint) {
            return false;
        }
        for (group, members) in &self.groups {
            if self.disabled_groups.contains(*group)
                && members.contains(endpoint)
                && !is_tool_group(group)
            {
                return false;
            }
        }
        if let Some(alts) = self.alternatives.get(endpoint) {
            if !alts.is_empty() {
                return alts.iter().any(|tg| !self.disabled_groups.contains(*tg));
            }
        }
        for (group, members) in &self.groups {
            if is_tool_group(group)
                && self.disabled_groups.contains(*group)
                && members.contains(endpoint)
            {
                return false;
            }
        }
        true
    }

    fn is_group_enabled(&self, group: &str) -> bool {
        let group = normalize(group);
        if group == "enterprise" {
            return false;
        }
        if self.disabled_groups.contains(group) {
            return false;
        }
        if is_tool_group(group) {
            return true;
        }
        let Some(members) = self.groups.get(group) else {
            return false;
        };
        members.iter().all(|ep| {
            // `isEndpointEnabledDirectly`: explicit flag + functional-group
            // membership, without the tool-group alternatives rules.
            !self.disabled_endpoints.contains_key(*ep)
                && !self.groups.iter().any(|(g, eps)| {
                    self.disabled_groups.contains(*g) && eps.contains(*ep) && !is_tool_group(g)
                })
        })
    }

    /// `determineDisableReason`: explicit endpoint disable first, then any
    /// disabled group containing the endpoint (tool groups included — Java
    /// does not filter them here), else `UNKNOWN`.
    fn disable_reason(&self, endpoint: &str) -> DisableReason {
        let normalized = normalize(endpoint);
        if let Some(reason) = self.disabled_endpoints.get(normalized) {
            return *reason;
        }
        for (group, members) in &self.groups {
            if self.disabled_groups.contains(*group) && members.contains(normalized) {
                return self
                    .group_reasons
                    .get(*group)
                    .copied()
                    .unwrap_or(DisableReason::Config);
            }
        }
        DisableReason::Unknown
    }
}

enum ProbeOutcome {
    Missing,
    /// `--version` ran and yielded a version string.
    Version(String),
    /// Available, but no version could be parsed — Java logs a warning and
    /// keeps the group enabled.
    Available,
}

/// Java's `isCommandAvailable`: OS lookup first, then a `--version` run for
/// shim launchers `which`-style lookups miss.
fn engine_available(cmd: &str) -> bool {
    if crate::convert::find_executable(cmd, &[]).is_some() {
        return true;
    }
    crate::convert::run(
        &crate::convert::EngineSpec::new(cmd)
            .arg("--version")
            .timeout(PROBE_TIMEOUT),
    )
    .map(|out| !out.timed_out && out.code == Some(0))
    .unwrap_or(false)
}

fn probe_engine(cmd: &str) -> ProbeOutcome {
    if !engine_available(cmd) {
        return ProbeOutcome::Missing;
    }
    let out = crate::convert::run(
        &crate::convert::EngineSpec::new(cmd)
            .arg("--version")
            .timeout(PROBE_TIMEOUT),
    );
    let Ok(out) = out else {
        return ProbeOutcome::Available;
    };
    if out.timed_out || out.code != Some(0) {
        return ProbeOutcome::Available;
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    match first_version(&text) {
        Some(v) => ProbeOutcome::Version(v),
        None => ProbeOutcome::Available,
    }
}

/// Java's `VERSION_PATTERN` (`\d+(\.\d+){0,2}`) — the first dotted-numeric
/// run in the version output, greedy up to two dot groups.
fn first_version(text: &str) -> Option<String> {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        for _ in 0..2 {
            if i >= b.len() || b[i] != b'.' {
                break;
            }
            let dot = i;
            i += 1;
            let digits = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i == digits {
                i = dot;
                break;
            }
        }
        return Some(text[start..i].to_string());
    }
    None
}

/// Numeric dotted-version compare; missing components count as 0.
fn version_lt(version: &str, minimum: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> { v.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    let (a, b) = (parse(version), parse(minimum));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x < y;
        }
    }
    false
}

static CONFIG: OnceLock<EndpointConfig> = OnceLock::new();

/// Build the shared config once from the loaded settings. Called from
/// `main` before the router is assembled; tests can call it with their own
/// settings via [`config_for`]. Engine probing runs here so a missing
/// sidecar disables its group before the first request — except under
/// `cfg(test)`, where host PATH contents must not change assertions.
pub fn init(settings: &Settings) {
    let mut config = EndpointConfig::new(settings);
    if !cfg!(test) {
        config.probe_engines(settings);
    }
    let _ = CONFIG.set(config);
}

fn config() -> &'static EndpointConfig {
    // Tests and embedded uses may query before `init`; the default settings
    // reproduce Java's out-of-the-box enablement.
    CONFIG.get_or_init(|| EndpointConfig::new(&Settings::default()))
}

/// `/api/v1/<group>/<endpoint>` → endpoint key. `/api/v1/convert/pdf/img`
/// maps to `pdf-to-img` (and `img/pdf` to `img-to-pdf`), matching
/// `endpointKeyForUri`; other paths yield their fourth segment, and
/// non-API URIs yield `None` so callers fall back to the raw URI.
pub fn endpoint_key_for_uri(uri: &str) -> Option<String> {
    if !uri.contains("/api/v1") {
        return None;
    }
    let parts: Vec<&str> = uri.split('/').collect();
    if parts.len() <= 4 {
        return None;
    }
    if parts[3] == "convert" && parts.len() > 5 {
        return Some(format!("{}-to-{}", parts[4], parts[5]));
    }
    Some(parts[4].to_string())
}

/// `isEndpointEnabledForUri`: raw `RUST_DISABLED_ENDPOINTS` path matchers
/// first, then translate the request path to an endpoint key and apply the
/// enablement rules. Unknown keys default to enabled, like Java.
pub fn is_endpoint_enabled_for_uri(uri: &str) -> bool {
    let cfg = config();
    if cfg.disabled_paths.iter().any(|(p, glob)| {
        if *glob {
            uri.starts_with(p.as_str())
        } else {
            uri == p
        }
    }) {
        return false;
    }
    let key = endpoint_key_for_uri(uri);
    cfg.is_enabled(key.as_deref().unwrap_or(uri))
}

/// `isGroupEnabled` — used by the config endpoints when they are migrated.
pub fn is_group_enabled(group: &str) -> bool {
    config().is_group_enabled(group)
}

/// `EndpointConfiguration.isEndpointEnabled` for a bare endpoint key (the
/// `/api/v1/config/endpoint-enabled` family passes keys, not URIs). Full
/// `/api/v1/...` paths are translated first — desktop callers occasionally
/// hand paths where keys are expected.
pub fn is_endpoint_enabled(endpoint: &str) -> bool {
    config().is_enabled(&lookup_key(endpoint))
}

/// `EndpointConfiguration.getAllEndpoints` — every endpoint key in any
/// group, deduplicated. Order is unspecified, like Java's `HashSet` union.
pub fn all_endpoints() -> Vec<String> {
    let mut all = std::collections::BTreeSet::new();
    for (_, endpoints) in group_table() {
        for ep in *endpoints {
            all.insert((*ep).to_string());
        }
    }
    all.into_iter().collect()
}

/// `EndpointConfiguration.getEndpointStatuses` — the sparse map Java
/// exposes: only endpoints touched by `enableEndpoint`/`disableEndpoint`,
/// which in an OSS build means exactly the disabled set. Entries are
/// always `false`; a `true` only appears if something explicitly enables
/// an endpoint (proprietary-only today).
pub fn disabled_endpoint_statuses() -> Vec<(String, bool)> {
    let mut statuses: Vec<(String, bool)> = config()
        .disabled_endpoints
        .keys()
        .map(|k| (k.clone(), false))
        .collect();
    statuses.sort();
    statuses
}

/// `EndpointConfiguration.getEndpointAvailability` — the enabled flag plus
/// the reason Java reports when disabled (`null` reason when enabled).
pub fn endpoint_availability(endpoint: &str) -> (bool, Option<DisableReason>) {
    let cfg = config();
    let key = lookup_key(endpoint);
    if cfg.is_enabled(&key) {
        return (true, None);
    }
    (false, Some(cfg.disable_reason(&key)))
}

/// Bare keys pass through; `/api/v1/...` paths resolve to their endpoint
/// key so availability checks behave identically either way.
fn lookup_key(raw: &str) -> String {
    endpoint_key_for_uri(raw).unwrap_or_else(|| raw.trim_start_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;

    /// The shared `CONFIG` is a OnceLock, so one test owns initialisation
    /// and asserts the whole rule set against default settings.
    #[test]
    fn default_settings_enable_migrated_endpoints() {
        init(&Settings::default());

        // Ordinary migrated endpoints stay enabled.
        assert!(is_endpoint_enabled_for_uri("/api/v1/general/merge-pdfs"));

        assert!(is_endpoint_enabled_for_uri("/api/v1/analysis/page-count"));

        assert!(is_endpoint_enabled_for_uri("/api/v1/security/sanitize-pdf"));

        // Built-in dependency rules: no PDF/UA tagger, no form-detection
        // model, and url-to-pdf needs system.enableUrlToPDF.
        assert!(!is_endpoint_enabled_for_uri("/api/v1/convert/pdf/ua"));
        assert!(!is_endpoint_enabled_for_uri("/api/v1/convert/url/pdf"));

        // Unknown endpoint keys default to enabled, like Java.
        assert!(is_endpoint_enabled_for_uri(
            "/api/v1/general/not-a-real-endpoint"
        ));
        // Non-API paths bypass the table entirely.
        assert!(is_endpoint_enabled_for_uri("/health"));
    }

    #[test]
    fn version_extraction_matches_java_pattern() {
        assert_eq!(
            first_version("WeasyPrint version 60.1").as_deref(),
            Some("60.1")
        );
        assert_eq!(
            first_version("qpdf version 12.0.0").as_deref(),
            Some("12.0.0")
        );
        assert_eq!(first_version("gs 10.04.0").as_deref(), Some("10.04.0"));
        // Greedy up to two dot groups; a third is not part of the match.
        assert_eq!(first_version("1.2.3.4").as_deref(), Some("1.2.3"));
        // A dot with no following digit is not consumed.
        assert_eq!(first_version("version 2.").as_deref(), Some("2"));
        assert_eq!(first_version("no digits here"), None);
    }

    #[test]
    fn version_compare_treats_missing_components_as_zero() {
        assert!(version_lt("57.9", "58.0"));
        assert!(!version_lt("58.0", "58.0"));
        assert!(!version_lt("12.0.0", "12.0.0"));
        assert!(version_lt("11.9.9", "12.0.0"));
        assert!(!version_lt("60", "58.0"));
    }

    #[test]
    fn uri_to_endpoint_key_translation() {
        // `/api/v1/<area>/<endpoint>` → endpoint key.
        assert_eq!(
            endpoint_key_for_uri("/api/v1/general/merge-pdfs"),
            Some("merge-pdfs".to_string())
        );
        // Convert paths flatten to `<in>-to-<out>`.
        assert_eq!(
            endpoint_key_for_uri("/api/v1/convert/pdf/img"),
            Some("pdf-to-img".to_string())
        );
        assert_eq!(
            endpoint_key_for_uri("/api/v1/convert/img/pdf"),
            Some("img-to-pdf".to_string())
        );
        // Non-API and too-short paths yield None.
        assert_eq!(endpoint_key_for_uri("/health"), None);
        assert_eq!(endpoint_key_for_uri("/api/v1"), None);
    }

    /// Endpoints Java implements but Rust has no route for report
    /// unavailable with `NotSupported`, whether queried by key or path —
    /// with no Java proxy they can never succeed.
    #[test]
    fn unrouted_endpoints_report_not_supported() {
        init(&Settings::default());
        for key in [
            "cert-sign",
            "timestamp-pdf",
            "validate-signature",
            "verify-pdf",
            "validate-compliance",
            "ingest",
            "restore-pdf-protection",
        ] {
            let (enabled, reason) = endpoint_availability(key);
            assert!(!enabled, "{key} should be disabled");
            assert_eq!(reason, Some(DisableReason::NotSupported), "{key}");
        }
        // Path spellings resolve to the same key.
        let (enabled, reason) = endpoint_availability("/api/v1/security/cert-sign");
        assert!(!enabled);
        assert_eq!(reason, Some(DisableReason::NotSupported));
        // Hardware-signing subpaths collapse to the `cert-sign` key.
        assert!(!is_endpoint_enabled("/api/v1/security/cert-sign/sessions"));
        // Routed endpoints and frontend-composed keys stay enabled.
        for key in [
            "merge-pdfs",
            "ocr-pdf",
            "extract-pages",
            "compare",
            "pipeline",
        ] {
            assert!(is_endpoint_enabled(key), "{key} should stay enabled");
        }
    }
}
