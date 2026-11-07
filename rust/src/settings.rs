//! `configs/settings.yml` + `configs/custom_settings.yml` loading, mirroring
//! Java's `SPDFApplication`/`ConfigInitializer` wiring: the base file comes
//! from the install's `settings.yml` (seeded from the template on first boot),
//! `custom_settings.yml` layers on top, and Spring-style env vars bind last.
//! Only the keys Rust-owned behaviour reads are extracted; everything else
//! stays Java-side.
//!
//! `STIRLING_BASE_PATH` relocates the whole state tree exactly like Java's
//! `InstallationPathConfig`; unset means the working directory.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub system: System,
    pub endpoints: Endpoints,
    pub metrics: Metrics,
    pub legal: Legal,
    pub ui: Ui,
    pub security: Security,
    pub mail: Mail,
    pub storage: Storage,
    pub premium: Premium,
    pub ai_engine: AiEngine,
    pub policies: Policies,
    pub internal_api: InternalApi,
    /// YAML key is capitalised (`AutomaticallyGenerated:`); serde's
    /// camelCase would produce `automaticallyGenerated`.
    #[serde(rename = "AutomaticallyGenerated")]
    pub automatically_generated: AutomaticallyGenerated,
}

// YAML keys are camelCase (`corsAllowedOrigins`, `toRemove`); without
// `rename_all` serde expects the Rust field names verbatim and every
// multi-word key silently deserialises to its default.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct System {
    /// `system.corsAllowedOrigins` — empty list means Java falls back to
    /// allow-all-with-credentials, same as no configuration.
    pub cors_allowed_origins: Vec<String>,
    /// `system.enableUrlToPDF` — defaults false; Java disables `url-to-pdf`
    /// unless this is true. Explicit rename: serde's camelCase would produce
    /// `enableUrlToPdf`, but the YAML key keeps the `PDF` acronym capitalised.
    #[serde(rename = "enableUrlToPDF")]
    pub enable_url_to_pdf: bool,
    /// `system.fileUploadLimit` — "100MB"/"1GB" style; empty = no limit.
    pub file_upload_limit: String,
    /// `system.defaultLocale` — "" auto-detects; used by the login-disclaimer
    /// locale fallback chain.
    pub default_locale: String,
    /// `system.tessdataDir` — blank falls through to `TESSDATA_PREFIX` then
    /// the compiled-in default, matching `RuntimePathConfig`.
    pub tessdata_dir: String,
    /// `system.enableAnalytics` — tri-state: `null` prompts the admin on
    /// first launch, which is why this is `Option<bool>` rather than `bool`.
    pub enable_analytics: Option<bool>,
    pub custom_paths: CustomPaths,
    /// `system.backendUrl` — SSO/callback base; blank falls back to
    /// `http://localhost` in `AppConfig.getBackendUrl`.
    pub backend_url: String,
    /// `system.frontendUrl` — public web-app URL for links handed to
    /// browsers; blank triggers request-host then local-IP resolution.
    pub frontend_url: String,
    /// `system.enableMobileScanner` / `enableMobileSignature`.
    pub enable_mobile_scanner: bool,
    pub enable_mobile_signature: bool,
    pub mobile_scanner_settings: MobileScannerSettings,
    pub show_update: bool,
    pub show_settings_when_no_login: bool,
    pub enable_alpha_functionality: bool,
    /// `system.stirlingOfficeConversion` — routes office↔PDF requests to
    /// the Stirling Office Convert engine instead of LibreOffice.
    pub stirling_office_conversion: bool,
    /// `system.maxDPI` — safety ceiling for user/engine-chosen raster
    /// resolutions; Java defaults to 500 and clamps e.g. the
    /// extract-image-scans 300 DPI render with `Math.min`.
    #[serde(rename = "maxDPI")]
    pub max_dpi: i64,
    /// `enablePosthog`/`enableScarf` are nullable in Java and emitted raw.
    pub enable_posthog: Option<bool>,
    pub enable_scarf: Option<bool>,
    /// Java field initialises to `true`; explicit `null` stays `None`.
    pub enable_desktop_install_slide: Option<bool>,
    pub enable_easter_eggs: bool,
    /// `system.disableSanitize` — HTML sanitisation for the weasyprint
    /// converters; `false` keeps the jsoup allowlist (ported as the
    /// `data`-only protocol restriction where the policy itself is not).
    pub disable_sanitize: bool,
    /// `system.googlevisibility` — the YAML key is all-lowercase, unlike
    /// every other camelCase key; `false` (Java's default) serves a
    /// disallow-all `/robots.txt`.
    #[serde(rename = "googlevisibility")]
    pub google_visibility: bool,
}

// `ApplicationProperties.System` field initialisers, mirrored so a missing
// YAML key produces the same value Java's bean would hold.
impl Default for System {
    fn default() -> Self {
        Self {
            cors_allowed_origins: Vec::new(),
            enable_url_to_pdf: false,
            file_upload_limit: String::new(),
            default_locale: String::new(),
            tessdata_dir: String::new(),
            enable_analytics: None,
            custom_paths: CustomPaths::default(),
            backend_url: String::new(),
            frontend_url: String::new(),
            enable_mobile_scanner: true,
            enable_mobile_signature: true,
            mobile_scanner_settings: MobileScannerSettings::default(),
            show_update: true,
            show_settings_when_no_login: true,
            enable_alpha_functionality: false,
            stirling_office_conversion: false,
            max_dpi: 500,
            enable_posthog: None,
            enable_scarf: None,
            enable_desktop_install_slide: Some(true),
            enable_easter_eggs: true,
            disable_sanitize: false,
            google_visibility: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MobileScannerSettings {
    pub convert_to_pdf: bool,
    pub image_resolution: String,
    pub page_format: String,
    pub stretch_to_fit: bool,
}

impl Default for MobileScannerSettings {
    fn default() -> Self {
        Self {
            convert_to_pdf: true,
            image_resolution: "full".to_string(),
            page_format: "A4".to_string(),
            stretch_to_fit: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Ui {
    /// Blank collapses to `null` in the Java getter.
    pub app_name_navbar: String,
    /// No Java-side default: absent key stays `None` → JSON `null`.
    pub languages: Option<Vec<String>>,
    /// Raw stored value; the Java getters normalise at read time.
    pub default_tool_panel_mode: String,
    pub default_startup_view: String,
    pub default_hide_unavailable_tools: bool,
    pub default_hide_unavailable_conversions: bool,
    pub hide_disabled_tools: HideDisabledTools,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            app_name_navbar: String::new(),
            languages: None,
            default_tool_panel_mode: "sidebar".to_string(),
            default_startup_view: "tools".to_string(),
            default_hide_unavailable_tools: false,
            default_hide_unavailable_conversions: false,
            hide_disabled_tools: HideDisabledTools::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HideDisabledTools {
    pub google_drive: bool,
    /// camelCase gives `mobileQrScanner`; the YAML key keeps `QR` capitalised.
    #[serde(rename = "mobileQRScanner")]
    pub mobile_qr_scanner: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Security {
    pub enable_login: bool,
    pub honor_owner_passwords: bool,
    pub oauth2: OAuth2Settings,
    pub saml2: Saml2Settings,
    pub timestamp: TimestampSettings,
    pub validation: ValidationSettings,
}

impl Default for Security {
    fn default() -> Self {
        Self {
            enable_login: false,
            honor_owner_passwords: true,
            oauth2: OAuth2Settings::default(),
            saml2: Saml2Settings::default(),
            timestamp: TimestampSettings::default(),
            validation: ValidationSettings::default(),
        }
    }
}

/// `security.validation` — PDF signature validation settings, mirroring
/// Java's `ApplicationProperties.Security.Validation` defaults.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ValidationSettings {
    pub trust: ValidationTrust,
    pub allow_aia: bool,
    pub revocation: ValidationRevocation,
}

impl Default for ValidationSettings {
    fn default() -> Self {
        Self {
            trust: ValidationTrust::default(),
            allow_aia: false,
            revocation: ValidationRevocation::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ValidationTrust {
    pub server_as_anchor: bool,
    pub use_system_trust: bool,
    pub use_mozilla_bundle: bool,
    pub use_aatl: bool,
    pub use_eutl: bool,
}

impl Default for ValidationTrust {
    fn default() -> Self {
        Self {
            server_as_anchor: true,
            use_system_trust: true,
            use_mozilla_bundle: true,
            use_aatl: false,
            use_eutl: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ValidationRevocation {
    pub mode: String,
    pub hard_fail: bool,
}

impl Default for ValidationRevocation {
    fn default() -> Self {
        Self {
            mode: "none".to_string(),
            hard_fail: false,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OAuth2Settings {
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Saml2Settings {
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TimestampSettings {
    pub default_tsa_url: String,
    pub custom_tsa_urls: Vec<String>,
}

impl Default for TimestampSettings {
    fn default() -> Self {
        Self {
            default_tsa_url: "http://timestamp.digicert.com".to_string(),
            custom_tsa_urls: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Mail {
    pub enabled: bool,
    pub enable_invites: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Storage {
    pub enabled: bool,
    pub sharing: SharingSettings,
    pub signing: SigningSettings,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            enabled: true,
            sharing: SharingSettings::default(),
            signing: SigningSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SharingSettings {
    pub enabled: bool,
    pub link_enabled: bool,
    pub email_enabled: bool,
}

impl Default for SharingSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            link_enabled: true,
            email_enabled: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SigningSettings {
    pub enabled: bool,
}

impl Default for SigningSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Premium {
    pub pro_features: ProFeatures,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProFeatures {
    pub google_drive: GoogleDriveSettings,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GoogleDriveSettings {
    pub enabled: bool,
    pub client_id: String,
    pub api_key: String,
    pub app_id: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AiEngine {
    pub enabled: bool,
    pub features: AiFeatures,
}

/// `aiEngine.features.*` — every capability defaults to `true` in Java.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AiFeatures {
    pub chat: bool,
    pub document_questions: bool,
    pub create_pdf: bool,
    pub math_auditor: bool,
    pub pdf_comment: bool,
    pub classify: bool,
}

impl Default for AiFeatures {
    fn default() -> Self {
        Self {
            chat: true,
            document_questions: true,
            create_pdf: true,
            math_auditor: true,
            pdf_comment: true,
            classify: true,
        }
    }
}

/// Only `appVersion` is needed (for `isNewServer`); Java's `InitialSetup`
/// also generates `key`/`UUID`, which Rust does not consume.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AutomaticallyGenerated {
    pub app_version: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CustomPaths {
    pub pipeline: PipelinePaths,
    pub operations: OperationsPaths,
}

/// `system.customPaths.operations.*` — `RuntimePathConfig`'s overrides for
/// the engine binaries that accept an install-path setting. Blank means
/// the plain command name, matching Java's non-Docker defaults.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct OperationsPaths {
    pub weasyprint: String,
    pub unoconvert: String,
    pub calibre: String,
    pub ocrmypdf: String,
    pub soffice: String,
}

impl OperationsPaths {
    /// The configured path for `program`, or `None` when unset — callers
    /// fall back to the bare command name. `ebook-convert` is Calibre's
    /// default binary, so it resolves through `calibre` like Java does.
    pub fn for_program(&self, program: &str) -> Option<&str> {
        let custom = match program {
            "weasyprint" => &self.weasyprint,
            "unoconvert" => &self.unoconvert,
            "calibre" | "ebook-convert" => &self.calibre,
            "ocrmypdf" => &self.ocrmypdf,
            "soffice" => &self.soffice,
            _ => return None,
        };
        (!custom.trim().is_empty()).then_some(custom.trim())
    }
}

/// The engine binary `program` resolves to: `customPaths.operations`
/// override when configured, else the bare name `PATH` lookup uses.
pub fn engine_program(program: &str) -> String {
    current()
        .system
        .custom_paths
        .operations
        .for_program(program)
        .unwrap_or(program)
        .to_string()
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PipelinePaths {
    /// `system.customPaths.pipeline.pipelineDir` — replaces
    /// `<base>/pipeline` wholesale when set.
    pub pipeline_dir: String,
    /// `system.customPaths.pipeline.webUIConfigsDir` — replaces
    /// `<pipelineDir>/defaultWebUIConfigs` when set.
    pub web_ui_configs_dir: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Legal {
    pub terms_and_conditions: String,
    pub privacy_policy: String,
    pub accessibility_statement: String,
    pub cookie_policy: String,
    pub impressum: String,
    pub login_agreement: LoginAgreement,
}

/// `legal.loginAgreement` — Java defaults `showInAnonymousMode` to `true`,
/// so `Default` is implemented by hand rather than derived.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LoginAgreement {
    pub enabled: bool,
    pub show_in_anonymous_mode: bool,
    pub fallback_text: String,
}

impl Default for LoginAgreement {
    fn default() -> Self {
        Self {
            enabled: false,
            show_in_anonymous_mode: true,
            fallback_text: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Endpoints {
    pub to_remove: Vec<String>,
    pub groups_to_remove: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Metrics {
    pub enabled: bool,
}

impl Default for Metrics {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// `policies.*` — the per-run caps `PipelineLimits` enforces for pipelines,
/// watched folders and policy runs. A limit of 0 or less is unlimited.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Policies {
    pub max_steps: i64,
    pub max_files_per_step: i64,
}

impl Default for Policies {
    fn default() -> Self {
        Self {
            max_steps: 50,
            max_files_per_step: 10000,
        }
    }
}

/// `internalApi.*` — timeouts for the pipeline's loopback dispatch, matching
/// `ApplicationProperties.InternalApi`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct InternalApi {
    pub connect_timeout_seconds: u64,
    pub read_timeout_seconds: u64,
}

impl Default for InternalApi {
    fn default() -> Self {
        Self {
            connect_timeout_seconds: 10,
            read_timeout_seconds: 300,
        }
    }
}

/// `POLICIES_MAXSTEPS`-style env spellings of the limits, applied over the
/// file values like Spring relaxed binding does.
pub fn policy_limits() -> (i64, i64) {
    let settings = current();
    let max_steps = std::env::var("POLICIES_MAXSTEPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(settings.policies.max_steps);
    let max_files = std::env::var("POLICIES_MAXFILESPERSTEP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(settings.policies.max_files_per_step);
    (max_steps, max_files)
}

/// `INTERNALAPI_*` env spellings over the file values.
pub fn internal_api_timeouts() -> (Duration, Duration) {
    let api = &current().internal_api;
    let connect = std::env::var("INTERNALAPI_CONNECTTIMEOUTSECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(api.connect_timeout_seconds);
    let read = std::env::var("INTERNALAPI_READTIMEOUTSECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(api.read_timeout_seconds);
    (Duration::from_secs(connect), Duration::from_secs(read))
}

pub(crate) fn base_dir() -> PathBuf {
    match std::env::var("STIRLING_BASE_PATH") {
        Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => PathBuf::from("."),
    }
}

fn read_yaml(path: &PathBuf) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_yml::from_str::<Value>(&text).ok()
}

/// Deep-merge `overlay` onto `base`: maps merge recursively, scalars and
/// sequences in `overlay` replace `base` wholesale — the same precedence
/// Spring Boot's property sources give `custom_settings.yml` over
/// `settings.yml`.
fn merge(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(b), Value::Object(o)) => {
            for (key, value) in o {
                match b.get_mut(key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        b.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (base, overlay) => *base = overlay.clone(),
    }
}

/// Load both files and fold them into `Settings`. Missing or unparsable
/// files contribute nothing — Java logs a warning and continues, and an
/// absent `settings.yml` must not stop Rust from booting.
pub fn load() -> Settings {
    let configs = base_dir().join("configs");
    let mut merged = Value::Object(Map::new());
    for name in ["settings.yml", "custom_settings.yml"] {
        if let Some(doc) = read_yaml(&configs.join(name)) {
            merge(&mut merged, &doc);
        }
    }
    serde_json::from_value(merged).unwrap_or_default()
}

/// `SYSTEMFILEUPLOADLIMIT` / `SYSTEM_MAXFILESIZE` env fallbacks, matching
/// `ApplicationProperties.initializeFileUploadLimitFromEnv`: the former takes
/// "100MB"-style values, the latter a bare MB count. Returns bytes, `None`
/// meaning no configured limit.
pub fn upload_limit_bytes(settings: &Settings) -> Option<u64> {
    let raw = std::env::var("SYSTEMFILEUPLOADLIMIT")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("SYSTEM_MAXFILESIZE")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(|mb| format!("{mb}MB"))
        })
        .unwrap_or_else(|| settings.system.file_upload_limit.clone());
    parse_size(&raw)
}

/// `MobileScannerController.checkFeatureEnabled`: the session API stays up
/// while *either* the scanner or signature-draw feature is enabled. The
/// `SYSTEM_ENABLEMOBILE*` env spellings are Spring relaxed-binding overrides
/// and win over settings.yml when set.
pub fn mobile_features_enabled(system: &System) -> bool {
    let env_or = |env: &str, yaml: bool| {
        std::env::var(env)
            .ok()
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "true" | "1" | "yes" | "on"
                )
            })
            .unwrap_or(yaml)
    };
    env_or("SYSTEM_ENABLEMOBILESCANNER", system.enable_mobile_scanner)
        || env_or(
            "SYSTEM_ENABLEMOBILESIGNATURE",
            system.enable_mobile_signature,
        )
}

/// "100MB"/"1GB"/"512KB" → bytes; bare numbers are megabytes, matching the
/// settings template's documented format. `None`/empty means unlimited.
fn parse_size(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let split = raw
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(raw.len());
    let (number, unit) = raw.split_at(split);
    let value: f64 = number.trim().parse().ok()?;
    let multiplier = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "MB" => 1024.0 * 1024.0,
        "KB" => 1024.0,
        "GB" => 1024.0 * 1024.0 * 1024.0,
        "B" => 1.0,
        _ => return None,
    };
    Some((value * multiplier) as u64)
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Publish the startup-loaded settings for request handlers. Called from
/// `main` next to `endpointconfig::init`; tests that never call it see
/// `Settings::default()` (everything disabled/blank), matching how
/// `endpointconfig::config()` degrades.
pub fn init(settings: &Settings) {
    let _ = SETTINGS.set(settings.clone());
}

pub fn current() -> &'static Settings {
    SETTINGS.get_or_init(Settings::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_formats() {
        assert_eq!(parse_size("100MB"), Some(100 * 1024 * 1024));
        assert_eq!(parse_size("1GB"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size("512KB"), Some(512 * 1024));
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("bogus"), None);
    }

    #[test]
    fn merge_prefers_overlay() {
        let mut base: Value = serde_json::from_str(r#"{"a":1,"nested":{"x":1}}"#).unwrap();
        let overlay: Value = serde_json::from_str(r#"{"nested":{"y":2}}"#).unwrap();
        merge(&mut base, &overlay);
        assert_eq!(
            base,
            serde_json::from_str::<Value>(r#"{"a":1,"nested":{"x":1,"y":2}}"#).unwrap()
        );
    }
}
