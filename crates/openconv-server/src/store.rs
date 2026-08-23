//! The durable record of conversations that have started.
//!
//! Rooms are ephemeral — LiveKit reaps one once it empties — but usage gating has to
//! answer for calls long after they end, so the association between a conversation and
//! the user who started it cannot live only on the room. This is where it outlives it.
//!
//! An append-only JSONL file rather than a database: one conversation is one line, a
//! line is written once and never revised, and `GET /v1/convai/conversations` reads
//! the file forward. That is the whole access pattern, and it needs no schema, no
//! migration, and no process to keep running beside this one.

use crate::record::ConversationRecord;
use std::fmt;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// Append-only conversation history.
pub struct ConversationLog {
    path: PathBuf,
    /// Serializes writers within this process. Each record is one short line opened
    /// in append mode, so the kernel keeps concurrent writes from interleaving, but
    /// holding the lock across the open-write-flush makes that guarantee ours rather
    /// than something inherited from a platform detail.
    writer: Mutex<()>,
}

impl ConversationLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), writer: Mutex::new(()) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Records a conversation. Returns an error rather than logging and continuing:
    /// a token handed out for a conversation nobody recorded is a call that can never
    /// be billed, which is worse than a caller seeing the mint fail and retrying.
    pub async fn append(&self, record: &ConversationRecord) -> Result<(), LogError> {
        let mut line = serde_json::to_string(record).map_err(LogError::Serialize)?;
        line.push('\n');

        let _guard = self.writer.lock().await;

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .map_err(|error| LogError::Io { path: self.path.clone(), error })?;

        file.write_all(line.as_bytes())
            .await
            .map_err(|error| LogError::Io { path: self.path.clone(), error })?;

        // Without this the record lives in the page cache, and a host that loses power
        // between minting a token and flushing has billed nobody for a call that
        // happened.
        file.flush().await.map_err(|error| LogError::Io { path: self.path.clone(), error })?;

        Ok(())
    }

    /// Reads the whole history back, oldest first.
    ///
    /// The counterpart to [`Self::append`], and the seam
    /// `GET /v1/convai/conversations` is built on.
    pub async fn read_all(&self) -> Result<Vec<ConversationRecord>, LogError> {
        let contents = match tokio::fs::read_to_string(&self.path).await {
            Ok(contents) => contents,
            // A log that does not exist yet is a service that has served no
            // conversations — a real and correct answer, distinct from a read that
            // failed, which still propagates.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(LogError::Io { path: self.path.clone(), error }),
        };

        contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line)
                    .map_err(|error| LogError::Corrupt { line: line.to_owned(), error })
            })
            .collect()
    }
}

#[derive(Debug)]
pub enum LogError {
    Io { path: PathBuf, error: std::io::Error },
    Serialize(serde_json::Error),
    Corrupt { line: String, error: serde_json::Error },
}

impl fmt::Display for LogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, error } => write!(f, "conversation log {}: {error}", path.display()),
            Self::Serialize(error) => write!(f, "could not serialize a conversation: {error}"),
            Self::Corrupt { line, error } => {
                write!(f, "conversation log holds an unreadable line {line:?}: {error}")
            }
        }
    }
}

impl std::error::Error for LogError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::ConversationId;
    use crate::record::{AgentId, HappyUserId};

    fn temp_log() -> (ConversationLog, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "openconv-test-{}-{}.jsonl",
            std::process::id(),
            ConversationId::generate()
        ));
        (ConversationLog::new(&path), path)
    }

    fn record(user: Option<&str>) -> ConversationRecord {
        ConversationRecord::start(
            ConversationId::generate(),
            AgentId::new("agent_happy"),
            user.map(HappyUserId::new),
            1_700_000_000,
        )
    }

    #[tokio::test]
    async fn an_absent_log_reads_as_no_conversations() {
        let (log, path) = temp_log();
        assert_eq!(log.read_all().await.unwrap(), Vec::new());
        assert!(!path.exists(), "reading should not create the log");
    }

    #[tokio::test]
    async fn records_come_back_in_the_order_they_were_written() {
        let (log, path) = temp_log();
        let written: Vec<_> = (0..5).map(|i| record(Some(&format!("u_{i}")))).collect();
        for entry in &written {
            log.append(entry).await.unwrap();
        }

        assert_eq!(log.read_all().await.unwrap(), written);
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_appends_all_survive() {
        let (log, path) = temp_log();
        let log = std::sync::Arc::new(log);

        let writes = (0..50).map(|i| {
            let log = log.clone();
            tokio::spawn(async move { log.append(&record(Some(&format!("u_{i}")))).await })
        });
        for write in writes {
            write.await.unwrap().unwrap();
        }

        assert_eq!(log.read_all().await.unwrap().len(), 50);
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn a_corrupt_line_is_reported_rather_than_skipped() {
        let (log, path) = temp_log();
        log.append(&record(Some("u_1"))).await.unwrap();
        tokio::fs::write(&path, "{\"not\":\"a record\"}\n").await.unwrap();

        assert!(matches!(log.read_all().await, Err(LogError::Corrupt { .. })));
        tokio::fs::remove_file(path).await.unwrap();
    }
}
