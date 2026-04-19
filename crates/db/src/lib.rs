//! SQLite database abstraction

use rusqlite::{params, Connection, ErrorCode, OptionalExtension};
use sms_config::{detect_storage_type, ResourceProfile, StorageType};
use sms_errors::{AppError, Result};
use sms_types::Message;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct MediaTask {
    pub attachment_id: String,
    pub file_path: String,
    pub thumbnail_path: Option<String>,
    pub mime_type: String,
}

#[derive(Debug, Clone)]
pub struct MediaEmbeddingRow {
    pub attachment_id: String,
    pub frame_index: i64,
    pub frame_time_ms: Option<i64>,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct MediaNsfwRow {
    pub attachment_id: String,
    pub nsfw_label: String,
    pub nsfw_score: f32,
}

pub struct Database {
    conn: Connection,
}

#[derive(Debug, Clone, Copy)]
pub enum ConnectionMode {
    Interactive,
    Import,
}

impl Database {
    pub fn open(path: &std::path::Path, profile: ResourceProfile) -> Result<Self> {
        let conn = Connection::open(path)?;
        ensure_fts5_enabled(&conn)?;
        let storage = detect_storage_type(path);
        apply_pragmas(&conn, profile, storage, ConnectionMode::Interactive)?;
        run_migrations(&conn)?;
        Ok(Self { conn })
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

pub struct BatchWriter {
    conn: Connection,
    batch_size: usize,
}

impl BatchWriter {
    pub fn new(
        path: &std::path::Path,
        profile: ResourceProfile,
        batch_size: usize,
    ) -> Result<Self> {
        Self::new_with_mode(path, profile, batch_size, ConnectionMode::Import)
    }

    pub fn new_with_mode(
        path: &std::path::Path,
        profile: ResourceProfile,
        batch_size: usize,
        mode: ConnectionMode,
    ) -> Result<Self> {
        let conn = Connection::open(path)?;
        ensure_fts5_enabled(&conn)?;
        let storage = detect_storage_type(path);
        apply_pragmas(&conn, profile, storage, mode)?;
        run_migrations(&conn)?;
        Ok(Self { conn, batch_size })
    }

    pub fn insert_batch(&mut self, messages: &[Message]) -> Result<usize> {
        const MAX_RETRIES: usize = 5;
        let mut attempt = 0usize;

        loop {
            match self.try_insert_batch(messages) {
                Ok(inserted) => return Ok(inserted),
                Err(err) if is_busy_error(&err) && attempt < MAX_RETRIES => {
                    let backoff = 25u64.saturating_mul(1u64 << attempt);
                    std::thread::sleep(Duration::from_millis(backoff.min(500)));
                    attempt += 1;
                    continue;
                }
                Err(err) => return Err(AppError::Database(err)),
            }
        }
    }

    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    fn try_insert_batch(
        &mut self,
        messages: &[Message],
    ) -> std::result::Result<usize, rusqlite::Error> {
        if messages.is_empty() {
            return Ok(0);
        }

        let tx = self.conn.transaction()?;
        let mut stmt = tx.prepare_cached(
            "INSERT INTO messages (id, message_id, dedupe_hash, timestamp, address, body, body_searchable, message_type, message_direction, thread_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT DO NOTHING",
        )?;

        let mut attach_stmt = tx.prepare_cached(
            "INSERT INTO attachments (id, message_id, mime_type, file_path, file_hash, thumbnail_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT DO NOTHING",
        )?;

        let mut find_by_hash_stmt = tx.prepare_cached(
            "SELECT id FROM messages WHERE dedupe_hash = ?1 LIMIT 1",
        )?;

        let mut find_by_message_id_stmt = tx.prepare_cached(
            "SELECT id FROM messages WHERE message_id = ?1 LIMIT 1",
        )?;

        let mut find_by_timestamp_address_stmt = tx.prepare_cached(
            "SELECT id FROM messages WHERE timestamp = ?1 AND address = ?2 LIMIT 1",
        )?;

        let mut inserted = 0usize;
        let mut orphaned_attachments = 0usize;
        for msg in messages {
            let dedupe = msg.dedupe_hash.as_ref().map(|h| &h[..]);
            let changed = stmt.execute(params![
                msg.id.to_string(),
                msg.message_id,
                dedupe,
                msg.timestamp,
                msg.address,
                msg.body,
                msg.body_searchable,
                msg.message_type as i32,
                msg.direction.as_i32(),
                msg.thread_id,
            ])?;
            if changed > 0 {
                inserted += 1;
            }
            // Always insert attachments — link to existing message if this was a duplicate.
            // We must resolve the real parent_id that exists in the messages table,
            // otherwise the FOREIGN KEY constraint on attachments.message_id will fail.
            if !msg.attachments.is_empty() {
                let parent_id = if changed > 0 {
                    // Message was freshly inserted, use its id directly
                    Some(msg.id.to_string())
                } else {
                    // Message was a duplicate (ON CONFLICT DO NOTHING).
                    // Try dedupe_hash first, then message_id, to find the existing row.
                    let found_by_hash = dedupe.and_then(|hash| {
                        find_by_hash_stmt
                            .query_row(params![hash], |row| row.get::<_, String>(0))
                            .ok()
                    });
                    found_by_hash
                        .or_else(|| {
                            msg.message_id.as_ref().and_then(|mid| {
                                find_by_message_id_stmt
                                    .query_row(params![mid], |row| row.get::<_, String>(0))
                                    .ok()
                            })
                        })
                        .or_else(|| {
                            find_by_timestamp_address_stmt
                                .query_row(
                                    params![msg.timestamp, msg.address],
                                    |row| row.get::<_, String>(0),
                                )
                                .ok()
                        })
                };
                if let Some(parent) = parent_id {
                    for att in &msg.attachments {
                        attach_stmt.execute(params![
                            att.id.to_string(),
                            parent,
                            att.mime_type,
                            att.file_path,
                            &att.file_hash[..],
                            att.thumbnail_path,
                        ])?;
                    }
                } else {
                    // Could not resolve existing parent — skip these attachments to avoid FK violation.
                    // They can be recovered later via media backfill.
                    orphaned_attachments += msg.attachments.len();
                }
            }
        }

        if orphaned_attachments > 0 {
            tracing::warn!(
                orphaned_attachments,
                "Skipped attachments: could not resolve parent message for duplicate entries"
            );
        }

        drop(find_by_timestamp_address_stmt);
        drop(find_by_message_id_stmt);
        drop(find_by_hash_stmt);
        drop(attach_stmt);
        drop(stmt);
        tx.commit()?;
        Ok(inserted)
    }
}

pub fn checkpoint_wal(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    Ok(())
}

pub fn rebuild_fts(conn: &Connection) -> Result<()> {
    conn.execute("DROP TABLE IF EXISTS messages_fts", [])?;
    conn.execute(
        "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
            body_searchable,
            address,
            content=messages,
            tokenize='unicode61 remove_diacritics 0'
        )",
        [],
    )?;
    conn.execute(
        "INSERT INTO messages_fts(rowid, body_searchable, address)
         SELECT rowid, body_searchable, address FROM messages",
        [],
    )?;
    conn.execute(
        "INSERT INTO messages_fts(messages_fts) VALUES('optimize')",
        [],
    )?;
    Ok(())
}

pub fn upsert_ml_model(
    conn: &Connection,
    name: &str,
    version: &str,
    sha256: Option<&str>,
) -> Result<String> {
    upsert_ml_model_with_meta(conn, name, version, sha256, &ModelMeta::default())
}

#[derive(Debug, Default, Clone)]
pub struct ModelMeta {
    pub dims: Option<i64>,
    pub max_length: Option<i64>,
    pub normalize: Option<bool>,
    pub tokenizer_path: Option<String>,
    pub input_ids_name: Option<String>,
    pub attention_mask_name: Option<String>,
    pub token_type_ids_name: Option<String>,
    pub output_name: Option<String>,
}

pub fn upsert_ml_model_with_meta(
    conn: &Connection,
    name: &str,
    version: &str,
    sha256: Option<&str>,
    meta: &ModelMeta,
) -> Result<String> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM ml_models WHERE name = ?1 AND version = ?2 AND (sha256 IS ?3 OR sha256 = ?3)",
            params![name, version, sha256],
            |row| row.get(0),
        )
        .optional()
        .map_err(AppError::Database)?;

    if let Some(id) = existing {
        conn.execute(
            "UPDATE ml_models SET dims = ?1, max_length = ?2, normalize = ?3, \
                    tokenizer_path = ?4, input_ids_name = ?5, attention_mask_name = ?6, \
                    token_type_ids_name = ?7, output_name = ?8 \
             WHERE id = ?9",
            params![
                meta.dims,
                meta.max_length,
                meta.normalize.map(|v| if v { 1 } else { 0 }),
                meta.tokenizer_path,
                meta.input_ids_name,
                meta.attention_mask_name,
                meta.token_type_ids_name,
                meta.output_name,
                id
            ],
        )
        .map_err(AppError::Database)?;
        return Ok(id);
    }

    let id = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO ml_models (id, name, version, sha256, dims, max_length, normalize, \
                tokenizer_path, input_ids_name, attention_mask_name, token_type_ids_name, output_name) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            id,
            name,
            version,
            sha256,
            meta.dims,
            meta.max_length,
            meta.normalize.map(|v| if v { 1 } else { 0 }),
            meta.tokenizer_path,
            meta.input_ids_name,
            meta.attention_mask_name,
            meta.token_type_ids_name,
            meta.output_name
        ],
    )
    .map_err(AppError::Database)?;
    Ok(id)
}

pub fn insert_embedding(
    conn: &Connection,
    message_id: &str,
    model_id: &str,
    vector: &[f32],
) -> Result<()> {
    let bytes = encode_embedding_vector(vector);
    conn.execute(
        "INSERT OR REPLACE INTO embeddings (message_id, model_id, dims, vector)
         VALUES (?1, ?2, ?3, ?4)",
        params![message_id, model_id, vector.len() as i64, bytes],
    )
    .map_err(AppError::Database)?;
    Ok(())
}

pub fn insert_media_embedding(
    conn: &Connection,
    attachment_id: &str,
    model_id: &str,
    frame_index: i64,
    frame_time_ms: Option<i64>,
    caption: Option<&str>,
    vector: &[f32],
) -> Result<()> {
    let bytes = encode_embedding_vector(vector);
    conn.execute(
        "INSERT OR REPLACE INTO media_embeddings (attachment_id, model_id, frame_index, frame_time_ms, caption, dims, vector)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            attachment_id,
            model_id,
            frame_index,
            frame_time_ms,
            caption,
            vector.len() as i64,
            bytes
        ],
    )
    .map_err(AppError::Database)?;
    Ok(())
}

pub fn get_unprocessed_media(
    conn: &Connection,
    limit: Option<usize>,
    reprocess: bool,
) -> Result<Vec<MediaTask>> {
    let mut results = Vec::new();
    let mut sql = String::from(
        "SELECT a.id, a.file_path, a.thumbnail_path, a.mime_type \
         FROM attachments a \
         LEFT JOIN media_embeddings me ON me.attachment_id = a.id \
         WHERE (a.mime_type LIKE 'image/%' OR a.mime_type LIKE 'video/%')",
    );
    if !reprocess {
        sql.push_str(" AND (me.attachment_id IS NULL OR a.nsfw_label IS NULL)");
    }
    sql.push_str(" GROUP BY a.id ORDER BY a.created_at DESC");
    if limit.is_some() {
        sql.push_str(" LIMIT ?1");
    }

    let mut stmt = conn.prepare(&sql).map_err(AppError::Database)?;
    let mut map_row = |row: &rusqlite::Row| {
        Ok(MediaTask {
            attachment_id: row.get(0)?,
            file_path: row.get(1)?,
            thumbnail_path: row.get(2)?,
            mime_type: row.get(3)?,
        })
    };
    let rows = if let Some(limit) = limit {
        stmt.query_map(params![limit as i64], &mut map_row)
            .map_err(AppError::Database)?
    } else {
        stmt.query_map([], &mut map_row)
            .map_err(AppError::Database)?
    };
    for row in rows.flatten() {
        results.push(row);
    }
    Ok(results)
}

pub fn insert_media_results_batch(
    conn: &Connection,
    embeddings: &[MediaEmbeddingRow],
    nsfw_rows: &[MediaNsfwRow],
    model_id: &str,
) -> Result<usize> {
    conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")
        .map_err(AppError::Database)?;
    let mut embed_stmt = conn
        .prepare_cached(
            "INSERT OR REPLACE INTO media_embeddings \
             (attachment_id, model_id, frame_index, frame_time_ms, caption, dims, vector) \
             VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)",
        )
        .map_err(AppError::Database)?;
    let mut nsfw_stmt = conn
        .prepare_cached(
            "UPDATE attachments \
             SET nsfw_label = ?1, nsfw_score = ?2, nsfw_model = ?3, nsfw_timestamp = ?4 \
             WHERE id = ?5",
        )
        .map_err(AppError::Database)?;
    let now = chrono::Utc::now().timestamp_millis();
    let mut inserted = 0usize;
    for row in embeddings {
        let bytes = encode_embedding_vector(&row.embedding);
        embed_stmt
            .execute(params![
                row.attachment_id,
                model_id,
                row.frame_index,
                row.frame_time_ms,
                row.embedding.len() as i64,
                bytes,
            ])
            .map_err(AppError::Database)?;
        inserted += 1;
    }
    for row in nsfw_rows {
        nsfw_stmt
            .execute(params![
                row.nsfw_label,
                row.nsfw_score as f64,
                "clip-laion-probe",
                now,
                row.attachment_id,
            ])
            .map_err(AppError::Database)?;
    }
    conn.execute_batch("COMMIT").map_err(AppError::Database)?;
    Ok(inserted)
}

fn encode_embedding_vector(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for v in vector {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    bytes
}

fn apply_pragmas(
    conn: &Connection,
    profile: ResourceProfile,
    storage: StorageType,
    mode: ConnectionMode,
) -> Result<()> {
    let mut stmts = vec![
        "PRAGMA journal_mode=WAL".to_string(),
        "PRAGMA synchronous=NORMAL".to_string(),
        "PRAGMA foreign_keys=ON".to_string(),
        "PRAGMA busy_timeout=5000".to_string(),
    ];

    if matches!(mode, ConnectionMode::Import) {
        stmts.push("PRAGMA wal_autocheckpoint=0".to_string());
    } else {
        stmts.push("PRAGMA wal_autocheckpoint=4000".to_string());
    }

    match profile {
        ResourceProfile::Low => {
            stmts.push("PRAGMA cache_size=-256000".to_string());
            stmts.push("PRAGMA temp_store=FILE".to_string());
            stmts.push("PRAGMA mmap_size=1000000000".to_string());
            stmts.push("PRAGMA page_size=16384".to_string());
        }
        ResourceProfile::Medium => {
            stmts.push("PRAGMA cache_size=-512000".to_string());
            stmts.push("PRAGMA temp_store=FILE".to_string());
            stmts.push("PRAGMA mmap_size=5000000000".to_string());
            stmts.push("PRAGMA page_size=32768".to_string());
        }
        ResourceProfile::High => {
            stmts.push("PRAGMA cache_size=-768000".to_string());
            stmts.push("PRAGMA temp_store=MEMORY".to_string());
            stmts.push("PRAGMA mmap_size=10000000000".to_string());
            stmts.push("PRAGMA page_size=32768".to_string());
        }
    }

    match storage {
        StorageType::Hdd => {
            stmts.push("PRAGMA temp_store=FILE".to_string());
            stmts.push("PRAGMA mmap_size=500000000".to_string());
            stmts.push("PRAGMA page_size=16384".to_string());
        }
        StorageType::Ssd => {
            stmts.push("PRAGMA mmap_size=8000000000".to_string());
        }
        StorageType::Unknown => {}
    }

    conn.execute_batch(&stmts.join(";"))?;
    Ok(())
}

fn is_busy_error(err: &rusqlite::Error) -> bool {
    match err {
        rusqlite::Error::SqliteFailure(code, _) => {
            matches!(
                code.code,
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
            )
        }
        _ => false,
    }
}

fn ensure_fts5_enabled(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA compile_options")?;
    let mut rows = stmt.query([])?;
    let mut has_fts5 = false;
    while let Some(row) = rows.next()? {
        let opt: String = row.get(0)?;
        if opt.contains("FTS5") {
            has_fts5 = true;
            break;
        }
    }

    if !has_fts5 {
        return Err(AppError::Fts5Unavailable);
    }
    Ok(())
}

fn run_migrations(conn: &Connection) -> Result<()> {
    const MIGRATIONS: &[(i64, &str)] = &[
        (1, include_str!("../migrations/0001_initial.sql")),
        (2, include_str!("../migrations/0002_ml.sql")),
        (3, include_str!("../migrations/0003_ml_meta.sql")),
        (4, include_str!("../migrations/0004_contacts.sql")),
        (5, include_str!("../migrations/0005_settings.sql")),
        (6, include_str!("../migrations/0006_extended_contacts.sql")),
        (7, include_str!("../migrations/0007_ocr_data.sql")),
        (8, include_str!("../migrations/0008_vision_analysis.sql")),
        (9, include_str!("../migrations/0009_message_direction.sql")),
        (10, include_str!("../migrations/0010_media_nsfw.sql")),
        (11, include_str!("../migrations/0011_media_embeddings.sql")),
        (12, include_str!("../migrations/0012_attachment_gps_cache.sql")),
    ];
    // #todo: add a post-migration backfill that infers message_direction from legacy fields if available.

    let mut version = current_schema_version(conn)?;
    for (target, sql) in MIGRATIONS {
        if version >= *target {
            continue;
        }
        if *target == 3 && column_exists(conn, "ml_models", "dims")? {
            if table_exists(conn, "schema_version")? {
                conn.execute("UPDATE schema_version SET version = ?1", params![target])?;
            }
            version = *target;
            continue;
        }
        if *target == 4 && table_exists(conn, "contacts")? {
            if table_exists(conn, "schema_version")? {
                conn.execute("UPDATE schema_version SET version = ?1", params![target])?;
            }
            version = *target;
            continue;
        }
        if *target == 5 && table_exists(conn, "app_settings")? {
            if table_exists(conn, "schema_version")? {
                conn.execute("UPDATE schema_version SET version = ?1", params![target])?;
            }
            version = *target;
            continue;
        }
        if *target == 6 && column_exists(conn, "contacts", "phone_primary_type")? {
            if table_exists(conn, "schema_version")? {
                conn.execute("UPDATE schema_version SET version = ?1", params![target])?;
            }
            version = *target;
            continue;
        }
        if *target == 7 && column_exists(conn, "attachments", "ocr_text")? {
            if table_exists(conn, "schema_version")? {
                conn.execute("UPDATE schema_version SET version = ?1", params![target])?;
            }
            version = *target;
            continue;
        }
        if *target == 8 && column_exists(conn, "attachments", "vision_analysis")? {
            if table_exists(conn, "schema_version")? {
                conn.execute("UPDATE schema_version SET version = ?1", params![target])?;
            }
            version = *target;
            continue;
        }
        if *target == 12 && column_exists(conn, "attachments", "gps_lat")? {
            if table_exists(conn, "schema_version")? {
                conn.execute("UPDATE schema_version SET version = ?1", params![target])?;
            }
            version = *target;
            continue;
        }
        if *target == 12 {
            if !column_exists(conn, "attachments", "gps_lat")? {
                conn.execute("ALTER TABLE attachments ADD COLUMN gps_lat REAL", [])
                    .map_err(AppError::Database)?;
            }
            if !column_exists(conn, "attachments", "gps_lon")? {
                conn.execute("ALTER TABLE attachments ADD COLUMN gps_lon REAL", [])
                    .map_err(AppError::Database)?;
            }
            if !column_exists(conn, "attachments", "gps_checked")? {
                conn.execute(
                    "ALTER TABLE attachments ADD COLUMN gps_checked INTEGER DEFAULT 0",
                    [],
                )
                .map_err(AppError::Database)?;
            }
        }
        conn.execute_batch(sql)?;
        version = *target;
    }
    Ok(())
}

fn current_schema_version(conn: &Connection) -> Result<i64> {
    if !table_exists(conn, "schema_version")? {
        return Ok(0);
    }
    let version: Option<i64> = conn
        .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
            row.get(0)
        })
        .optional()
        .map_err(AppError::Database)?;
    Ok(version.unwrap_or(0))
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            params![table],
            |row| row.get(0),
        )
        .optional()
        .map_err(AppError::Database)?;
    Ok(exists.is_some())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    // Build SQL string directly (safe - table names come from migrations only)
    let sql = format!("PRAGMA table_info({})", table);
    let mut stmt = conn.prepare(&sql).map_err(AppError::Database)?;
    let mut rows = stmt.query([]).map_err(AppError::Database)?;
    while let Some(row) = rows.next().map_err(AppError::Database)? {
        let name: String = row.get(1).map_err(AppError::Database)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sms_types::{Message, MessageDirection, MessageType};

    #[test]
    fn migrations_create_tables() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::open(tmp.path(), ResourceProfile::Low).unwrap();
        let conn = db.connection();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='messages'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn batch_writer_inserts() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = BatchWriter::new(tmp.path(), ResourceProfile::Low, 10).unwrap();
        let msg = Message {
            id: uuid::Uuid::new_v4(),
            message_id: Some("m1".into()),
            dedupe_hash: None,
            timestamp: 123,
            address: "+1555".into(),
            body: "hello".into(),
            body_searchable: "hello".into(),
            message_type: MessageType::Sms,
            direction: MessageDirection::Incoming,
            thread_id: None,
            attachments: Vec::new(),
        };
        writer.insert_batch(&[msg]).unwrap();
        let count: i64 = writer
            .conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn batch_writer_inserts_attachments() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = BatchWriter::new(tmp.path(), ResourceProfile::Low, 10).unwrap();
        let msg = Message {
            id: uuid::Uuid::new_v4(),
            message_id: None,
            dedupe_hash: Some([2u8; 32]),
            timestamp: 123,
            address: "+1555".into(),
            body: "hello".into(),
            body_searchable: "hello".into(),
            message_type: MessageType::Mms,
            direction: MessageDirection::Outgoing,
            thread_id: None,
            attachments: vec![sms_types::AttachmentRef {
                id: uuid::Uuid::new_v4(),
                mime_type: "image/jpeg".into(),
                file_path: "image.jpg".into(),
                file_hash: [9u8; 32],
                thumbnail_path: None,
            }],
        };
        writer.insert_batch(&[msg]).unwrap();
        let count: i64 = writer
            .conn
            .query_row("SELECT COUNT(*) FROM attachments", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn upserts_ml_model() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::open(tmp.path(), ResourceProfile::Low).unwrap();
        let conn = db.connection();
        let id1 = upsert_ml_model(conn, "test", "1", Some("abc")).unwrap();
        let id2 = upsert_ml_model(conn, "test", "1", Some("abc")).unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn inserts_embedding() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::open(tmp.path(), ResourceProfile::Low).unwrap();
        let conn = db.connection();
        let model_id = upsert_ml_model(conn, "test", "1", None).unwrap();
        conn.execute(
            "INSERT INTO messages (id, timestamp, address, body, body_searchable, message_type, message_direction)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                "msg-1",
                1i64,
                "+1",
                "hi",
                "hi",
                MessageType::Sms as i32,
                MessageDirection::Incoming as i32
            ],
        )
        .unwrap();
        insert_embedding(conn, "msg-1", &model_id, &[0.1, 0.2, 0.3]).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn inserts_media_embedding() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::open(tmp.path(), ResourceProfile::Low).unwrap();
        let conn = db.connection();
        let model_id = upsert_ml_model(conn, "media", "1", None).unwrap();
        conn.execute(
            "INSERT INTO messages (id, timestamp, address, body, body_searchable, message_type, message_direction)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                "msg-2",
                2i64,
                "+1",
                "hi",
                "hi",
                MessageType::Sms as i32,
                MessageDirection::Incoming as i32
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO attachments (id, message_id, mime_type, file_path, file_hash)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params!["att-1", "msg-2", "image/jpeg", "img.jpg", vec![0u8; 32]],
        )
        .unwrap();
        insert_media_embedding(conn, "att-1", &model_id, 0, None, Some("caption"), &[0.2, 0.3])
            .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM media_embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
