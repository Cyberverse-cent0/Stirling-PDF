//! SQLite-backed persistence for async jobs, the Rust counterpart of Java's
//! job rows in the H2 datasource. `handlers::jobs` keeps its in-memory maps
//! as the hot path; every mutation is written through here so a restart
//! loses nothing. File payloads live in `file_data` separately from the
//! `job_files` metadata rows so re-saving a job never rewrites blobs.
//!
//! The database sits next to Java's own at `configs/stirling-jobs.db`
//! under `STIRLING_BASE_PATH` — a separate file because H2 and SQLite
//! formats are not interchangeable.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::handlers::jobs::{JobRecord, StoredFile, StoredFileRef};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS jobs (
    job_id TEXT PRIMARY KEY,
    complete INTEGER NOT NULL,
    error TEXT,
    result TEXT,
    created_at INTEGER NOT NULL,
    completed_at INTEGER,
    primary_file_id TEXT
);
CREATE TABLE IF NOT EXISTS job_files (
    file_id TEXT PRIMARY KEY,
    job_id TEXT NOT NULL,
    file_name TEXT NOT NULL,
    content_type TEXT NOT NULL,
    file_size INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_job_files_job ON job_files(job_id);
CREATE TABLE IF NOT EXISTS file_data (
    file_id TEXT PRIMARY KEY,
    data BLOB NOT NULL
);
";

pub struct JobStore {
    conn: Mutex<Connection>,
}

/// One hydrated job: the record plus its stored file payloads keyed by
/// `file_id`, matching what `AppState.files` holds in memory.
pub struct StoredJob {
    pub job_id: String,
    pub record: JobRecord,
    pub files: Vec<(StoredFileRef, StoredFile)>,
}

impl JobStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("open job store {}", path.display()))?;
        conn.execute_batch(SCHEMA)
            .context("init job store schema")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Upserts the job row and replaces its file-reference list. File bytes
    /// are written separately by `put_file_data`.
    pub fn save_job(&self, job_id: &str, record: &JobRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO jobs (job_id, complete, error, result, created_at,
                               completed_at, primary_file_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(job_id) DO UPDATE SET
                complete = excluded.complete,
                error = excluded.error,
                result = excluded.result,
                completed_at = excluded.completed_at,
                primary_file_id = excluded.primary_file_id",
            params![
                job_id,
                record.complete as i64,
                record.error,
                record.result.as_ref().map(|v| v.to_string()),
                record.created_at as i64,
                record.completed_at.map(|t| t as i64),
                record.file.as_ref().map(|f| f.file_id.as_str()),
            ],
        )?;
        conn.execute("DELETE FROM job_files WHERE job_id = ?1", params![job_id])?;
        for f in &record.files {
            conn.execute(
                "INSERT OR REPLACE INTO job_files
                    (file_id, job_id, file_name, content_type, file_size)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    f.file_id,
                    job_id,
                    f.file_name,
                    f.content_type,
                    f.file_size as i64
                ],
            )?;
        }
        Ok(())
    }

    pub fn put_file_data(&self, file_id: &str, stored: &StoredFile) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT OR REPLACE INTO file_data (file_id, data) VALUES (?1, ?2)",
            params![file_id, stored.bytes],
        )?;
        Ok(())
    }

    /// Deletes the job, its file references, and the referenced blobs —
    /// file rows are only reachable through a job, so nothing else can
    /// hold them.
    pub fn delete_job(&self, job_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let file_ids: Vec<String> = {
            let mut stmt = conn.prepare("SELECT file_id FROM job_files WHERE job_id = ?1")?;
            let rows = stmt.query_map(params![job_id], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for file_id in &file_ids {
            conn.execute("DELETE FROM file_data WHERE file_id = ?1", params![file_id])?;
        }
        conn.execute("DELETE FROM job_files WHERE job_id = ?1", params![job_id])?;
        conn.execute("DELETE FROM jobs WHERE job_id = ?1", params![job_id])?;
        Ok(())
    }

    /// Every persisted job with its file payloads, for hydration into the
    /// in-memory maps at startup.
    pub fn load_all(&self) -> Result<Vec<StoredJob>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT job_id, complete, error, result, created_at, completed_at,
                    primary_file_id
             FROM jobs",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        let mut out = Vec::new();
        for (job_id, complete, error, result, created_at, completed_at, primary) in rows {
            let record = JobRecord {
                complete: complete != 0,
                error,
                result: result.and_then(|r| serde_json::from_str(&r).ok()),
                file: None,
                files: Vec::new(),
                created_at: created_at as u64,
                completed_at: completed_at.map(|t| t as u64),
            };
            let mut file_stmt = conn.prepare(
                "SELECT f.file_id, f.file_name, f.content_type, f.file_size, d.data
                 FROM job_files f LEFT JOIN file_data d ON d.file_id = f.file_id
                 WHERE f.job_id = ?1",
            )?;
            let mut files = Vec::new();
            let mut refs = Vec::new();
            for row in file_stmt.query_map(params![job_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<Vec<u8>>>(4)?,
                ))
            })? {
                let (file_id, file_name, content_type, file_size, data) = row?;
                let reference = StoredFileRef {
                    file_id: file_id.clone(),
                    file_name: file_name.clone(),
                    content_type: content_type.clone(),
                    file_size: file_size as u64,
                };
                refs.push(reference.clone());
                if let Some(bytes) = data {
                    files.push((
                        reference,
                        StoredFile {
                            bytes,
                            file_name,
                            content_type,
                        },
                    ));
                }
            }
            drop(file_stmt);
            let record = JobRecord {
                file: primary.and_then(|pid| refs.iter().find(|f| f.file_id == pid).cloned()),
                files: refs,
                ..record
            };
            out.push(StoredJob {
                job_id,
                record,
                files,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(complete: bool) -> JobRecord {
        JobRecord {
            complete,
            error: None,
            result: Some(json!({"ok": true})),
            file: None,
            files: vec![StoredFileRef {
                file_id: "f1".into(),
                file_name: "out.pdf".into(),
                content_type: "application/pdf".into(),
                file_size: 4,
            }],
            created_at: 100,
            completed_at: complete.then_some(200),
        }
    }

    #[test]
    fn save_load_delete_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(&dir.path().join("jobs.db")).unwrap();

        let mut rec = record(true);
        rec.file = rec.files.first().cloned();
        store.save_job("job-1", &rec).unwrap();
        store
            .put_file_data(
                "f1",
                &StoredFile {
                    bytes: b"%PDF".to_vec(),
                    file_name: "out.pdf".into(),
                    content_type: "application/pdf".into(),
                },
            )
            .unwrap();

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].job_id, "job-1");
        assert!(loaded[0].record.complete);
        assert_eq!(loaded[0].record.result, Some(json!({"ok": true})));
        assert_eq!(loaded[0].record.file.as_ref().unwrap().file_id, "f1");
        assert_eq!(loaded[0].files.len(), 1);
        assert_eq!(loaded[0].files[0].1.bytes, b"%PDF");

        // Re-saving replaces refs without touching blobs.
        store.save_job("job-1", &record(false)).unwrap();
        let loaded = store.load_all().unwrap();
        assert!(!loaded[0].record.complete);
        assert_eq!(loaded[0].files.len(), 1);
        assert_eq!(loaded[0].files[0].1.bytes, b"%PDF");

        store.delete_job("job-1").unwrap();
        assert!(store.load_all().unwrap().is_empty());
        // The blob went with the job.
        let conn = store.conn.lock().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM file_data", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn incomplete_jobs_load_as_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(&dir.path().join("jobs.db")).unwrap();
        store.save_job("job-2", &record(false)).unwrap();
        let loaded = store.load_all().unwrap();
        assert!(!loaded[0].record.complete);
        assert_eq!(loaded[0].record.completed_at, None);
    }
}
