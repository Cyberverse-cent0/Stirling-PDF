use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

pub const MAX_ENTRY_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 500 * 1024 * 1024;
pub const MAX_ENTRIES: u64 = 10_000;

const BUFFER_SIZE: usize = 8192;

#[derive(Debug)]
pub enum ZipBombError {
    Io(io::Error),
    EntryTooLarge { max_bytes: u64 },
    TotalTooLarge { max_bytes: u64 },
    TooManyEntries { max_entries: u64 },
}

impl fmt::Display for ZipBombError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::EntryTooLarge { max_bytes } => write!(
                formatter,
                "Archive entry exceeds the maximum allowed size of {max_bytes} bytes"
            ),
            Self::TotalTooLarge { max_bytes } => write!(
                formatter,
                "Archive decompresses to more than the maximum allowed {max_bytes} bytes"
            ),
            Self::TooManyEntries { max_entries } => write!(
                formatter,
                "Archive contains more than the maximum allowed {max_entries} entries"
            ),
        }
    }
}

impl std::error::Error for ZipBombError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::EntryTooLarge { .. }
            | Self::TotalTooLarge { .. }
            | Self::TooManyEntries { .. } => None,
        }
    }
}

impl From<io::Error> for ZipBombError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub fn read_entry<R: Read>(reader: R) -> Result<Vec<u8>, ZipBombError> {
    read_entry_with_limit(reader, MAX_ENTRY_BYTES)
}

pub fn read_entry_with_limit<R: Read>(
    mut reader: R,
    max_bytes: u64,
) -> Result<Vec<u8>, ZipBombError> {
    let mut entry = Vec::new();
    let mut buffer = [0; BUFFER_SIZE];

    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            return Ok(entry);
        }

        if entry.len() as u64 + count as u64 > max_bytes {
            return Err(ZipBombError::EntryTooLarge { max_bytes });
        }
        entry.extend_from_slice(&buffer[..count]);
    }
}

#[derive(Debug)]
pub struct Budget {
    max_entry_bytes: u64,
    max_total_bytes: u64,
    max_entries: u64,
    total_bytes: u64,
    entry_count: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self::new(MAX_ENTRY_BYTES, MAX_TOTAL_BYTES, MAX_ENTRIES)
    }
}

impl Budget {
    pub const fn new(max_entry_bytes: u64, max_total_bytes: u64, max_entries: u64) -> Self {
        Self {
            max_entry_bytes,
            max_total_bytes,
            max_entries,
            total_bytes: 0,
            entry_count: 0,
        }
    }

    pub const fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub const fn entry_count(&self) -> u64 {
        self.entry_count
    }

    pub fn read_entry<R: Read>(&mut self, reader: R) -> Result<Vec<u8>, ZipBombError> {
        self.count_entry()?;
        let bytes = read_entry_with_limit(reader, self.max_entry_bytes)?;
        self.add_to_total(bytes.len() as u64)?;
        Ok(bytes)
    }

    pub fn copy_entry<R: Read>(
        &mut self,
        mut reader: R,
        target: impl AsRef<Path>,
    ) -> Result<(), ZipBombError> {
        self.count_entry()?;
        let mut output = File::create(target)?;
        let mut written = 0_u64;
        let mut buffer = [0; BUFFER_SIZE];

        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }

            written += count as u64;
            if written > self.max_entry_bytes {
                return Err(ZipBombError::EntryTooLarge {
                    max_bytes: self.max_entry_bytes,
                });
            }
            output.write_all(&buffer[..count])?;
        }

        self.add_to_total(written)
    }

    fn count_entry(&mut self) -> Result<(), ZipBombError> {
        self.entry_count += 1;
        if self.entry_count > self.max_entries {
            return Err(ZipBombError::TooManyEntries {
                max_entries: self.max_entries,
            });
        }
        Ok(())
    }

    fn add_to_total(&mut self, bytes: u64) -> Result<(), ZipBombError> {
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        if self.total_bytes > self.max_total_bytes {
            return Err(ZipBombError::TotalTooLarge {
                max_bytes: self.max_total_bytes,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{read_entry_with_limit, Budget, ZipBombError, MAX_ENTRY_BYTES, MAX_TOTAL_BYTES};

    static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

    fn temporary_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "stirling-zip-bomb-{}-{}.tmp",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn reads_entries_within_limit_and_rejects_excess() {
        let data = [42; 4];
        assert_eq!(read_entry_with_limit(Cursor::new(data), 4).unwrap(), data);
        assert!(matches!(
            read_entry_with_limit(Cursor::new(data), 3),
            Err(ZipBombError::EntryTooLarge { max_bytes: 3 })
        ));
    }

    #[test]
    fn budget_enforces_entry_size_total_size_and_entry_count() {
        let mut per_entry = Budget::new(3, 10, 4);
        assert!(matches!(
            per_entry.read_entry(Cursor::new([0; 4])),
            Err(ZipBombError::EntryTooLarge { max_bytes: 3 })
        ));

        let mut total = Budget::new(4, 5, 4);
        total.read_entry(Cursor::new([0; 3])).unwrap();
        assert!(matches!(
            total.read_entry(Cursor::new([0; 3])),
            Err(ZipBombError::TotalTooLarge { max_bytes: 5 })
        ));

        let mut count = Budget::new(4, 10, 1);
        count.read_entry(Cursor::new([0; 1])).unwrap();
        assert!(matches!(
            count.read_entry(Cursor::new([0; 1])),
            Err(ZipBombError::TooManyEntries { max_entries: 1 })
        ));
    }

    #[test]
    fn copies_within_limit_and_rejects_entry_overflow() {
        let path = temporary_path();
        let mut budget = Budget::new(4, 8, 2);

        budget.copy_entry(Cursor::new(b"abcd"), &path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"abcd");
        fs::remove_file(&path).unwrap();

        assert!(matches!(
            budget.copy_entry(Cursor::new(b"abcde"), &path),
            Err(ZipBombError::EntryTooLarge { max_bytes: 4 })
        ));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn defaults_match_java_limits() {
        let budget = Budget::default();
        assert_eq!(MAX_ENTRY_BYTES, 100 * 1024 * 1024);
        assert_eq!(MAX_TOTAL_BYTES, 500 * 1024 * 1024);
        assert_eq!(budget.entry_count(), 0);
        assert_eq!(budget.total_bytes(), 0);
    }
}
