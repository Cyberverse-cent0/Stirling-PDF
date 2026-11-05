use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub host: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            host: std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".into()),
            port: std::env::var("PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(8081),
        })
    }

    pub fn bind_addr(&self) -> SocketAddr {
        format!("{}:{}", self.host, self.port)
            .parse()
            .expect("HOST/PORT must form a valid socket address")
    }
}

/// The Java/Spring backend that still owns every endpoint Rust hasn't
/// migrated. `JAVA_BACKEND_URL` overrides the default; set it to `off` (or an
/// empty value) to disable the fallback proxy entirely.
pub struct JavaBackend {
    pub client: reqwest::Client,
    pub base_url: String,
}

impl JavaBackend {
    pub fn from_env() -> Option<Arc<Self>> {
        let base_url = match std::env::var("JAVA_BACKEND_URL") {
            Ok(url) => {
                let url = url.trim();
                if url.is_empty() || url == "off" {
                    return None;
                }
                url.trim_end_matches('/').to_string()
            }
            Err(_) => "http://localhost:8080".to_string(),
        };
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        Some(Arc::new(Self { client, base_url }))
    }
}
