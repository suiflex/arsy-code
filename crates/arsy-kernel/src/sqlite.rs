use crate::{
    domain::{Principal, SessionId},
    event::{EventEnvelope, EventStore, StoreError, StreamVersion},
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    stream_id TEXT PRIMARY KEY,
    version INTEGER NOT NULL CHECK (version >= 0),
    durability TEXT NOT NULL CHECK (durability IN ('memory', 'normal', 'strict'))
) STRICT;

CREATE TABLE IF NOT EXISTS events (
    stream_id TEXT NOT NULL REFERENCES sessions(stream_id),
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    event_id TEXT NOT NULL UNIQUE,
    actor_json TEXT NOT NULL,
    correlation_id TEXT NOT NULL,
    occurred_at_ms INTEGER NOT NULL,
    envelope_json TEXT NOT NULL,
    PRIMARY KEY (stream_id, sequence)
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS events_actor_time
    ON events(actor_json, occurred_at_ms);
CREATE INDEX IF NOT EXISTS events_correlation
    ON events(correlation_id, stream_id, sequence);

CREATE TABLE IF NOT EXISTS session_metadata (
    stream_id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
"#;

/// Shape of the schema this binary writes and reads.
///
/// Stamped into SQLite's own `user_version` rather than a bookkeeping table:
/// the pragma costs no table and moves inside the same transaction as the
/// statements it describes.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    Memory,
    Normal,
    Strict,
}

impl Durability {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Normal => "normal",
            Self::Strict => "strict",
        }
    }

    const fn synchronous(self) -> &'static str {
        match self {
            Self::Memory => "OFF",
            Self::Normal => "NORMAL",
            Self::Strict => "FULL",
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "memory" => Ok(Self::Memory),
            "normal" => Ok(Self::Normal),
            "strict" => Ok(Self::Strict),
            _ => Err(StoreError::Storage(format!("unknown durability {value}"))),
        }
    }
}

/// One recorded stream as `arsy session list` reports it. Timestamps are
/// `None` for a stream whose row exists but whose append did not commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSummary {
    pub session: SessionId,
    pub version: StreamVersion,
    pub durability: Durability,
    pub started_at_ms: Option<u64>,
    pub last_event_at_ms: Option<u64>,
    pub title: Option<String>,
}

pub struct SqliteEventStore {
    path: PathBuf,
    writer: Mutex<Connection>,
    durability: Durability,
}

impl SqliteEventStore {
    pub fn open(path: impl AsRef<Path>, durability: Durability) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let connection = Connection::open(&path).map_err(storage)?;
        configure(&connection, durability)?;
        match read_schema_version(&connection)? {
            // Either a new file or one written before stamping began; both
            // already have the v1 shape, and `SCHEMA` is idempotent.
            0 => {
                connection.execute_batch(SCHEMA).map_err(storage)?;
                write_schema_version(&connection, SCHEMA_VERSION)?;
            }
            SCHEMA_VERSION => {}
            current if current < SCHEMA_VERSION => {
                return Err(StoreError::MigrationRequired {
                    current,
                    expected: SCHEMA_VERSION,
                })
            }
            current => {
                return Err(StoreError::SchemaTooNew {
                    current,
                    supported: SCHEMA_VERSION,
                })
            }
        }
        Ok(Self {
            path,
            writer: Mutex::new(connection),
            durability,
        })
    }

    pub fn session_durability(&self, stream: SessionId) -> Result<Option<Durability>, StoreError> {
        let connection = self.reader()?;
        connection
            .query_row(
                "SELECT durability FROM sessions WHERE stream_id = ?1",
                [stream.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(storage)?
            .map(|value| Durability::parse(&value))
            .transpose()
    }

    /// Every recorded stream in this store, most recently active first.
    ///
    /// The event store trait is stream-scoped by design, so enumeration lives
    /// here rather than on `EventStore`: only a store that owns a catalogue of
    /// streams can answer it, and an in-memory one has no durable catalogue to
    /// list. `arsy session list` is the caller.
    pub fn sessions(&self, limit: usize) -> Result<Vec<SessionSummary>, StoreError> {
        let connection = self.reader()?;
        let mut statement = connection
            .prepare(
                "SELECT s.stream_id, s.version, s.durability,
                        MIN(e.occurred_at_ms), MAX(e.occurred_at_ms),
                        m.title
                 FROM sessions s
                 LEFT JOIN events e ON e.stream_id = s.stream_id
                 LEFT JOIN session_metadata m ON m.stream_id = s.stream_id
                 GROUP BY s.stream_id, s.version, s.durability, m.title
                 ORDER BY MAX(e.occurred_at_ms) DESC, s.stream_id
                 LIMIT ?1",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map([limit_i64(limit)], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })
            .map_err(storage)?;
        let mut sessions = Vec::new();
        for row in rows {
            let (id, version, durability, first, last, title) = row.map_err(storage)?;
            sessions.push(SessionSummary {
                session: id
                    .parse()
                    .map_err(|_| StoreError::Storage(format!("unreadable stream id {id}")))?,
                version: StreamVersion(
                    version
                        .try_into()
                        .map_err(|_| StoreError::Storage("negative stream version".into()))?,
                ),
                durability: Durability::parse(&durability)?,
                started_at_ms: first.map(unsigned).transpose()?,
                last_event_at_ms: last.map(unsigned).transpose()?,
                title,
            });
        }
        Ok(sessions)
    }

    pub fn set_session_title(&self, session: SessionId, title: &str) -> Result<(), StoreError> {
        let connection = self.writer.lock().unwrap();
        let stream_id = session.to_string();
        let now = crate::artifact::unix_time_ms();
        connection
            .execute(
                "INSERT INTO session_metadata (stream_id, title, updated_at_ms)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(stream_id) DO UPDATE SET title = ?2, updated_at_ms = ?3",
                params![stream_id, title, now as i64],
            )
            .map_err(storage)?;
        Ok(())
    }

    /// Replace the title only while it still reads `expected`, and answer
    /// whether it did. One statement under the writer lock, so a rename
    /// that lands between a caller's read and its write is never overwritten.
    pub fn replace_session_title(
        &self,
        session: SessionId,
        expected: &str,
        title: &str,
    ) -> Result<bool, StoreError> {
        let connection = self.writer.lock().unwrap();
        let now = crate::artifact::unix_time_ms();
        let changed = connection
            .execute(
                "UPDATE session_metadata SET title = ?3, updated_at_ms = ?4
                 WHERE stream_id = ?1 AND title = ?2",
                params![session.to_string(), expected, title, now as i64],
            )
            .map_err(storage)?;
        Ok(changed == 1)
    }

    pub fn session_title(&self, session: SessionId) -> Result<Option<String>, StoreError> {
        let connection = self.reader()?;
        connection
            .query_row(
                "SELECT title FROM session_metadata WHERE stream_id = ?1",
                [session.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(storage)
    }

    pub fn delete_session(&self, session: SessionId) -> Result<bool, StoreError> {
        let connection = self.writer.lock().unwrap();
        let stream_id = session.to_string();
        let _ = connection.execute(
            "DELETE FROM session_metadata WHERE stream_id = ?1",
            params![stream_id],
        );
        let events = connection
            .execute(
                "DELETE FROM events WHERE stream_id = ?1",
                params![stream_id],
            )
            .map_err(storage)?;
        let sess = connection
            .execute(
                "DELETE FROM sessions WHERE stream_id = ?1",
                params![stream_id],
            )
            .map_err(storage)?;
        Ok(events > 0 || sess > 0)
    }

    pub fn read_by_actor(
        &self,
        actor: &Principal,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        let connection = self.reader()?;
        let actor = serde_json::to_string(actor).map_err(serialization)?;
        let mut statement = connection
            .prepare(
                "SELECT envelope_json FROM events
                 WHERE actor_json = ?1
                 ORDER BY occurred_at_ms, stream_id, sequence
                 LIMIT ?2",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map(params![actor, limit_i64(limit)], |row| {
                row.get::<_, String>(0)
            })
            .map_err(storage)?;
        decode_rows(rows)
    }

    fn reader(&self) -> Result<Connection, StoreError> {
        let connection = Connection::open(&self.path).map_err(storage)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(storage)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(storage)?;
        Ok(connection)
    }
}

impl EventStore for SqliteEventStore {
    fn current_version(&self, stream: SessionId) -> Result<StreamVersion, StoreError> {
        let connection = self.reader()?;
        let version = connection
            .query_row(
                "SELECT version FROM sessions WHERE stream_id = ?1",
                [stream.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(storage)?
            .unwrap_or_default();
        Ok(StreamVersion(version.try_into().map_err(|_| {
            StoreError::Storage("negative stream version".into())
        })?))
    }

    fn append(
        &self,
        stream: SessionId,
        expected: StreamVersion,
        events: Vec<EventEnvelope>,
    ) -> Result<StreamVersion, StoreError> {
        let mut encoded = Vec::with_capacity(events.len());
        for (offset, event) in events.iter().enumerate() {
            let sequence = expected
                .0
                .checked_add(offset as u64 + 1)
                .ok_or(StoreError::SequenceOverflow)?;
            event.validate_for_append(stream, sequence)?;
            encoded.push((
                event,
                serde_json::to_string(event).map_err(serialization)?,
                serde_json::to_string(&event.actor).map_err(serialization)?,
            ));
        }

        let mut connection = self
            .writer
            .lock()
            .map_err(|_| StoreError::Storage("SQLite writer lock poisoned".into()))?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let stored = transaction
            .query_row(
                "SELECT version, durability FROM sessions WHERE stream_id = ?1",
                [stream.to_string()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(storage)?;
        let actual = match stored {
            Some((version, stored_durability)) => {
                if stored_durability != self.durability.as_str() {
                    return Err(StoreError::Storage(format!(
                        "session durability is {stored_durability}, writer requested {}",
                        self.durability.as_str()
                    )));
                }
                StreamVersion(
                    version
                        .try_into()
                        .map_err(|_| StoreError::Storage("negative stream version".into()))?,
                )
            }
            None => {
                transaction
                    .execute(
                        "INSERT INTO sessions(stream_id, version, durability) VALUES (?1, 0, ?2)",
                        params![stream.to_string(), self.durability.as_str()],
                    )
                    .map_err(storage)?;
                StreamVersion(0)
            }
        };
        if actual != expected {
            return Err(StoreError::Conflict { expected, actual });
        }

        for (event, envelope_json, actor_json) in encoded {
            transaction
                .execute(
                    "INSERT INTO events(
                        stream_id, sequence, event_id, actor_json, correlation_id,
                        occurred_at_ms, envelope_json
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        stream.to_string(),
                        integer(event.sequence)?,
                        event.id.to_string(),
                        actor_json,
                        event.correlation.to_string(),
                        integer(event.occurred_at_ms)?,
                        envelope_json,
                    ],
                )
                .map_err(storage)?;
        }
        let new_version = expected
            .0
            .checked_add(events.len() as u64)
            .ok_or(StoreError::SequenceOverflow)?;
        transaction
            .execute(
                "UPDATE sessions SET version = ?1 WHERE stream_id = ?2 AND version = ?3",
                params![
                    integer(new_version)?,
                    stream.to_string(),
                    integer(expected.0)?
                ],
            )
            .map_err(storage)?;
        transaction.commit().map_err(storage)?;
        Ok(StreamVersion(new_version))
    }

    fn read(
        &self,
        stream: SessionId,
        from_sequence: u64,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        let connection = self.reader()?;
        let mut statement = connection
            .prepare(
                "SELECT envelope_json FROM events
                 WHERE stream_id = ?1 AND sequence >= ?2
                 ORDER BY sequence
                 LIMIT ?3",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map(
                params![
                    stream.to_string(),
                    integer(from_sequence)?,
                    limit_i64(limit)
                ],
                |row| row.get::<_, String>(0),
            )
            .map_err(storage)?;
        decode_rows(rows)
    }
}

pub(crate) fn read_schema_version(connection: &Connection) -> Result<u32, StoreError> {
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(storage)?;
    version
        .try_into()
        .map_err(|_| StoreError::Storage("negative schema version".into()))
}

pub(crate) fn write_schema_version(
    connection: &Connection,
    version: u32,
) -> Result<(), StoreError> {
    // Pragmas reject bound parameters, so the value is formatted in; it is a
    // `u32`, so there is nothing to inject.
    connection
        .execute_batch(&format!("PRAGMA user_version = {version};"))
        .map_err(storage)
}

/// Whether another connection is writing to the store at `path` right now.
///
/// Asked before the store's files are deleted, so a reset never pulls a
/// database out from under a turn that is mid-write. A missing file is not in
/// use.
// ponytail: detects a writer holding a transaction, not an idle process with
// the store open; that process recreates the store on its next write. A
// workspace lock file is the upgrade if that ever matters.
pub fn is_being_written(path: &Path) -> Result<bool, StoreError> {
    if !path.exists() {
        return Ok(false);
    }
    let connection = Connection::open(path).map_err(storage)?;
    connection.busy_timeout(Duration::ZERO).map_err(storage)?;
    match connection.execute_batch("BEGIN IMMEDIATE; ROLLBACK;") {
        Ok(()) => Ok(false),
        Err(rusqlite::Error::SqliteFailure(failure, _))
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            Ok(true)
        }
        Err(error) => Err(storage(error)),
    }
}

/// Fold the write-ahead log of the store at `path` into the database file,
/// so the file alone holds every committed transaction. `false` when a
/// reader or writer kept the checkpoint from completing.
///
/// Asked before the store's files are moved: a database moved without its
/// log loses whatever the log still held.
pub fn checkpoint(path: &Path) -> Result<bool, StoreError> {
    let connection = Connection::open(path).map_err(storage)?;
    connection.busy_timeout(Duration::ZERO).map_err(storage)?;
    let busy: i64 = connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
        .map_err(storage)?;
    Ok(busy == 0)
}

fn configure(connection: &Connection, durability: Durability) -> Result<(), StoreError> {
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(storage)?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(storage)?;
    connection
        .pragma_update(None, "synchronous", durability.synchronous())
        .map_err(storage)?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(storage)?;
    Ok(())
}

fn integer(value: u64) -> Result<i64, StoreError> {
    value
        .try_into()
        .map_err(|_| StoreError::Storage("value exceeds SQLite INTEGER".into()))
}

fn unsigned(value: i64) -> Result<u64, StoreError> {
    value
        .try_into()
        .map_err(|_| StoreError::Storage("negative timestamp".into()))
}

fn limit_i64(limit: usize) -> i64 {
    limit.try_into().unwrap_or(i64::MAX)
}

fn decode_rows(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<String>>,
) -> Result<Vec<EventEnvelope>, StoreError> {
    rows.map(|row| serde_json::from_str(&row.map_err(storage)?).map_err(serialization))
        .collect()
}

fn storage(error: rusqlite::Error) -> StoreError {
    StoreError::Storage(error.to_string())
}

fn serialization(error: serde_json::Error) -> StoreError {
    StoreError::Serialization(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{CorrelationId, EventId},
        event::{EventPayload, SchemaVersion},
    };
    use serde_json::Value;
    use std::{fs, sync::Arc, thread};
    use uuid::Uuid;

    fn event(stream: SessionId, sequence: u64) -> EventEnvelope {
        EventEnvelope::new(
            stream,
            sequence,
            Principal::System,
            None,
            CorrelationId::new(),
            SchemaVersion(1),
            "test.event",
            EventPayload::Inline { data: Value::Null },
        )
    }

    fn database_path() -> PathBuf {
        std::env::temp_dir().join(format!("arsy-sqlite-test-{}.db", Uuid::new_v4()))
    }

    fn remove_database(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    /// A conditional replace writes only over the title it expected, so a
    /// rename made in between survives.
    #[test]
    fn a_title_is_replaced_only_while_it_reads_as_expected() {
        let directory = tempfile::tempdir().unwrap();
        let store =
            SqliteEventStore::open(directory.path().join("s.db"), Durability::Normal).unwrap();
        let session = SessionId::new();

        assert!(!store
            .replace_session_title(session, "draft", "model")
            .unwrap());
        assert_eq!(
            store.session_title(session).unwrap(),
            None,
            "no row is created"
        );

        store.set_session_title(session, "draft").unwrap();
        assert!(store
            .replace_session_title(session, "draft", "model")
            .unwrap());
        assert_eq!(
            store.session_title(session).unwrap().as_deref(),
            Some("model")
        );

        store.set_session_title(session, "mine").unwrap();
        assert!(!store
            .replace_session_title(session, "model", "later")
            .unwrap());
        assert_eq!(
            store.session_title(session).unwrap().as_deref(),
            Some("mine")
        );
    }

    #[test]
    fn a_checkpoint_leaves_every_commit_in_the_database_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let writer = Connection::open(&path).unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        writer
            .execute_batch("CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (7);")
            .unwrap();
        let wal = directory.path().join("sessions.sqlite3-wal");
        assert!(
            std::fs::metadata(&wal).unwrap().len() > 0,
            "the commit is in the log"
        );

        assert!(checkpoint(&path).unwrap());
        assert_eq!(std::fs::metadata(&wal).map_or(0, |m| m.len()), 0);
        drop(writer);

        // The database file alone, without its sidecars, still has the row.
        let moved = directory.path().join("alone.sqlite3");
        std::fs::copy(&path, &moved).unwrap();
        let reader = Connection::open(&moved).unwrap();
        let value: i64 = reader
            .query_row("SELECT v FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, 7);
    }

    #[test]
    fn a_store_is_in_use_only_while_something_writes_to_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        assert!(
            !is_being_written(&path).unwrap(),
            "a missing store is not in use"
        );

        let _store = SqliteEventStore::open(&path, Durability::Normal).unwrap();
        assert!(!is_being_written(&path).unwrap(), "open but idle");

        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
        assert!(is_being_written(&path).unwrap(), "mid-transaction");
        writer.execute_batch("ROLLBACK;").unwrap();
        assert!(!is_being_written(&path).unwrap());
    }

    #[test]
    fn opening_stamps_the_schema_version() {
        let path = database_path();
        let store = SqliteEventStore::open(&path, Durability::Normal).unwrap();
        let connection = store.reader().unwrap();

        assert_eq!(read_schema_version(&connection).unwrap(), SCHEMA_VERSION);
        drop(connection);
        remove_database(&path);
    }

    #[test]
    fn a_store_from_a_newer_build_is_refused() {
        let path = database_path();
        drop(SqliteEventStore::open(&path, Durability::Normal).unwrap());
        let connection = Connection::open(&path).unwrap();
        write_schema_version(&connection, SCHEMA_VERSION + 1).unwrap();
        drop(connection);

        let error = SqliteEventStore::open(&path, Durability::Normal)
            .err()
            .expect("a newer schema must not open");
        assert_eq!(
            error,
            StoreError::SchemaTooNew {
                current: SCHEMA_VERSION + 1,
                supported: SCHEMA_VERSION,
            }
        );
        remove_database(&path);
    }

    #[test]
    fn wal_store_batches_and_allows_concurrent_readers() {
        let path = database_path();
        let store = Arc::new(SqliteEventStore::open(&path, Durability::Strict).unwrap());
        let stream = SessionId::new();
        let batch: Vec<_> = (1..=25).map(|sequence| event(stream, sequence)).collect();
        assert_eq!(
            store.append(stream, StreamVersion(0), batch).unwrap(),
            StreamVersion(25)
        );
        assert_eq!(
            store.session_durability(stream).unwrap(),
            Some(Durability::Strict)
        );

        let mut readers = Vec::new();
        for _ in 0..4 {
            let store = Arc::clone(&store);
            readers.push(thread::spawn(move || {
                for _ in 0..50 {
                    assert!(!store.read(stream, 1, 100).unwrap().is_empty());
                }
            }));
        }
        for sequence in 26..=100 {
            store
                .append(
                    stream,
                    StreamVersion(sequence - 1),
                    vec![event(stream, sequence)],
                )
                .unwrap();
        }
        for reader in readers {
            reader.join().unwrap();
        }
        assert_eq!(store.read(stream, 1, 200).unwrap().len(), 100);
        assert_eq!(
            store.read_by_actor(&Principal::System, 200).unwrap().len(),
            100
        );
        assert!(matches!(
            store.append(stream, StreamVersion(99), vec![event(stream, 100)]),
            Err(StoreError::Conflict {
                actual: StreamVersion(100),
                ..
            })
        ));

        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
        let indexes: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema
                 WHERE type = 'index' AND name IN ('events_actor_time', 'events_correlation')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexes, 2);
        drop(connection);
        drop(store);
        remove_database(&path);
    }

    #[test]
    fn duplicate_ids_are_rejected_by_the_unique_index() {
        let path = database_path();
        let store = SqliteEventStore::open(&path, Durability::Normal).unwrap();
        let stream = SessionId::new();
        let mut first = event(stream, 1);
        first.id = EventId::new();
        store
            .append(stream, StreamVersion(0), vec![first.clone()])
            .unwrap();
        let mut duplicate = event(stream, 2);
        duplicate.id = first.id;
        assert!(store
            .append(stream, StreamVersion(1), vec![duplicate])
            .is_err());
        assert_eq!(store.current_version(stream).unwrap(), StreamVersion(1));
        drop(store);
        remove_database(&path);
    }
}
