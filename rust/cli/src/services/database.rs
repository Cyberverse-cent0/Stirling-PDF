use std::path::Path;

/// The Rust backend currently has no embedded database; persistence lives in
/// the proprietary Java service. This module reports that state and is the
/// seam where a future Rust persistence layer plugs in.
#[derive(Debug, serde::Serialize)]
pub struct DatabaseStatus {
    pub embedded: bool,
    pub database_url: Option<String>,
    pub note: String,
}

pub fn status(root: &Path) -> DatabaseStatus {
    let _ = root;
    DatabaseStatus {
        embedded: false,
        database_url: std::env::var("DATABASE_URL").ok(),
        note: "stirling-server has no embedded database; data lives in the \
               proprietary service. db migrate/backup/restore/verify/vacuum \
               are not applicable until Rust persistence lands."
            .to_string(),
    }
}
