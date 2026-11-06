use std::net::SocketAddr;

use crate::settings::Settings;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub host: String,
    /// Merged `configs/settings.yml` + `configs/custom_settings.yml`.
    pub settings: Settings,
    /// `system.corsAllowedOrigins` equivalent: comma-separated origin
    /// patterns. `None` means nothing configured — Java then allows all
    /// origins.
    pub cors_origins: Option<Vec<String>>,
    /// `STIRLING_PDF_TAURI_MODE` — restricts CORS to localhost/Tauri origins.
    pub tauri_mode: bool,
    /// Per-part multipart byte cap, mirroring
    /// `spring.servlet.multipart.max-file-size` (default 2000 MB).
    pub max_upload_bytes: u64,
    /// `metrics.enabled` — gates the `/api/v1/info/*` operational endpoints.
    pub metrics_enabled: bool,
}

/// Origins Java always appends for desktop-app compatibility, from
/// `WebMvcConfig.addCorsMappings`.
const TAURI_ORIGINS: &[&str] = &[
    "tauri://localhost",
    "http://tauri.localhost",
    "https://tauri.localhost",
];

/// The `allowedOriginPatterns` list Java would compute, in precedence order:
/// Tauri mode wins outright; configured origins get the Tauri extras; no
/// configuration means allow-all.
pub fn cors_origin_patterns(config: &Config) -> Vec<String> {
    if config.tauri_mode {
        return [
            "http://localhost:*",
            "https://localhost:*",
            "tauri://*",
            "tauri://localhost",
            "http://tauri.localhost",
            "https://tauri.localhost",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
    }
    match &config.cors_origins {
        Some(origins) if !origins.is_empty() => {
            let mut all = origins.clone();
            for origin in TAURI_ORIGINS {
                if !all.iter().any(|o| o == origin) {
                    all.push(origin.to_string());
                }
            }
            all
        }
        _ => vec!["*".to_string()],
    }
}

/// Spring's `allowedOriginPatterns` semantics: `*` is a wildcard, everything
/// else matches literally.
fn origin_matches(pattern: &str, origin: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == origin;
    }
    let mut rest = origin;
    let mut parts = pattern.split('*').peekable();
    // A leading non-wildcard segment must match the start.
    if let Some(first) = parts.next() {
        if !first.is_empty() {
            if !rest.starts_with(first) {
                return false;
            }
            rest = &rest[first.len()..];
        }
    }
    for part in parts {
        if part.is_empty() {
            continue;
        }
        match rest.find(part) {
            Some(index) => rest = &rest[index + part.len()..],
            None => return false,
        }
    }
    // A trailing non-wildcard segment must reach the end of the origin.
    pattern
        .rsplit('*')
        .next()
        .is_none_or(|tail| tail.is_empty() || origin.ends_with(tail))
}

pub fn cors_origin_allowed(patterns: &[String], origin: &str) -> bool {
    patterns.iter().any(|p| origin_matches(p, origin))
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let settings = crate::settings::load();
        // `CORS_ALLOWED_ORIGINS`/`SYSTEM_CORSALLOWEDORIGINS` are the env
        // spellings; the settings.yml list wins when both exist, matching
        // Java's property-source precedence closely enough for operators.
        let env_origins = std::env::var("CORS_ALLOWED_ORIGINS")
            .ok()
            .or_else(|| std::env::var("SYSTEM_CORSALLOWEDORIGINS").ok())
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|v: &Vec<String>| !v.is_empty());
        let file_origins = (!settings.system.cors_allowed_origins.is_empty())
            .then(|| settings.system.cors_allowed_origins.clone());
        let cors_origins = file_origins.or(env_origins);
        let max_upload_bytes = std::env::var("RUST_MAX_UPLOAD_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| crate::settings::upload_limit_bytes(&settings))
            .unwrap_or(2000 * 1024 * 1024);
        Ok(Self {
            host: std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".into()),
            port: std::env::var("PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(8081),
            metrics_enabled: settings.metrics.enabled,
            settings,
            cors_origins,
            tauri_mode: std::env::var("STIRLING_PDF_TAURI_MODE")
                .map(|v| v.trim().eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            max_upload_bytes,
        })
    }

    pub fn bind_addr(&self) -> SocketAddr {
        format!("{}:{}", self.host, self.port)
            .parse()
            .expect("HOST/PORT must form a valid socket address")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(cors_origins: Option<Vec<String>>, tauri_mode: bool) -> Config {
        Config {
            port: 8081,
            host: "127.0.0.1".into(),
            settings: Settings::default(),
            cors_origins,
            tauri_mode,
            max_upload_bytes: 0,
            metrics_enabled: true,
        }
    }

    #[test]
    fn no_configuration_allows_all_origins() {
        let patterns = cors_origin_patterns(&config(None, false));
        assert_eq!(patterns, vec!["*".to_string()]);
        assert!(cors_origin_allowed(&patterns, "https://anything.example"));
    }

    #[test]
    fn configured_origins_get_tauri_extras() {
        let patterns = cors_origin_patterns(&config(
            Some(vec!["https://app.example".to_string()]),
            false,
        ));
        assert!(patterns.contains(&"https://app.example".to_string()));
        assert!(patterns.contains(&"tauri://localhost".to_string()));
        assert!(!patterns.contains(&"*".to_string()));
        // Tauri origins are appended once, not duplicated.
        let tauri = patterns
            .iter()
            .filter(|p| p.as_str() == "tauri://localhost")
            .count();
        assert_eq!(tauri, 1);
    }

    #[test]
    fn tauri_mode_restricts_to_localhost_patterns() {
        let patterns =
            cors_origin_patterns(&config(Some(vec!["https://app.example".to_string()]), true));
        assert!(!patterns.iter().any(|p| p == "https://app.example"));
        assert!(patterns.contains(&"tauri://*".to_string()));
        assert!(cors_origin_allowed(&patterns, "tauri://localhost"));
        assert!(cors_origin_allowed(&patterns, "http://localhost:5173"));
        assert!(!cors_origin_allowed(&patterns, "https://evil.example"));
    }

    #[test]
    fn wildcard_matching_follows_spring_semantics() {
        assert!(origin_matches(
            "https://*.example.com",
            "https://api.example.com"
        ));
        assert!(!origin_matches(
            "https://*.example.com",
            "https://example.com"
        ));
        assert!(origin_matches(
            "http://localhost:*",
            "http://localhost:8080"
        ));
        assert!(!origin_matches("http://localhost:*", "http://localhost"));
        // Literal patterns have no wildcard behaviour.
        assert!(origin_matches("https://a.b", "https://a.b"));
        assert!(!origin_matches("https://a.b", "https://a.b.c"));
        // Trailing wildcard matches any suffix.
        assert!(origin_matches("tauri://*", "tauri://anything"));
        // Multiple wildcards.
        assert!(origin_matches("*://*.example.com", "https://x.example.com"));
    }

    #[test]
    fn bind_addr_uses_host_and_port() {
        let mut c = config(None, false);
        c.host = "0.0.0.0".into();
        c.port = 9999;
        assert_eq!(c.bind_addr().to_string(), "0.0.0.0:9999");
    }
}
