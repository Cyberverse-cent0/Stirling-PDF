use std::path::Path;

use anyhow::Result;

use crate::cli::DbCmd;
use crate::output::json;
use crate::services::database;

pub fn run(root: &Path, cmd: &DbCmd, json_flag: bool) -> Result<()> {
    match cmd {
        DbCmd::Status => {
            let status = database::status(root);
            let text = format!(
                "embedded database: {}\nDATABASE_URL: {}\n{}",
                status.embedded,
                status.database_url.as_deref().unwrap_or("(unset)"),
                status.note
            );
            let value = serde_json::to_value(&status)?;
            json::emit(json_flag, &text, Some(&value));
            Ok(())
        }
        DbCmd::Migrate | DbCmd::Backup | DbCmd::Restore { .. } | DbCmd::Verify | DbCmd::Vacuum => {
            let status = database::status(root);
            anyhow::bail!(
                "db {} is not applicable: {}",
                match cmd {
                    DbCmd::Migrate => "migrate",
                    DbCmd::Backup => "backup",
                    DbCmd::Restore { .. } => "restore",
                    DbCmd::Verify => "verify",
                    DbCmd::Vacuum => "vacuum",
                    DbCmd::Status => unreachable!(),
                },
                status.note
            )
        }
    }
}
