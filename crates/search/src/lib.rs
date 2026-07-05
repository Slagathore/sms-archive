//! FTS5 search backend

use rusqlite::params;
use sms_config::ResourceProfile;
use sms_db::Database;
use sms_errors::Result;
use sms_types::{Message, MessageDirection, MessageType};
use std::cmp::Ordering;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

pub trait SearchBackend {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Message>>;
}

/// Convert free-form user text into a safe FTS5 query: each whitespace token
/// becomes a quoted phrase term (internal `"` doubled), joined implicitly as
/// AND. Without this, FTS5's query mini-language turns innocent input into
/// syntax errors (`don't`, `C++`, `3:30`) or column filters (`-secret`).
pub fn sanitize_fts5_query(raw: &str) -> String {
    raw.split_whitespace()
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

pub struct Fts5Backend {
    db: Database,
}

impl Fts5Backend {
    pub fn open(path: &std::path::Path, profile: ResourceProfile) -> Result<Self> {
        let db = Database::open(path, profile)?;
        Ok(Self { db })
    }

    pub fn connection(&self) -> &rusqlite::Connection {
        self.db.connection()
    }
}

impl SearchBackend for Fts5Backend {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Message>> {
        let conn = self.db.connection();
        // ORDER BY bm25: without it, LIMIT truncates an arbitrary subset and
        // the most relevant matches silently never appear.
        let mut stmt = conn.prepare(
            "SELECT messages.id, messages.message_id, messages.timestamp, messages.address, \
                messages.body, messages.body_searchable, messages.message_type, messages.message_direction, messages.thread_id, messages.contact_name \
             FROM messages_fts \
             JOIN messages ON messages.rowid = messages_fts.rowid \
             WHERE messages_fts MATCH ?1 \
             ORDER BY bm25(messages_fts) \
             LIMIT ?2",
        )?;

        let sanitized = sanitize_fts5_query(query);
        let rows = stmt.query_map(params![sanitized, limit as i64], |row| {
            let message_type: i32 = row.get(6)?;
            let message_direction: i32 = row.get(7)?;
            let msg = Message {
                id: uuid::Uuid::parse_str(&row.get::<_, String>(0)?)
                    .unwrap_or_else(|_| uuid::Uuid::new_v4()),
                message_id: row.get(1)?,
                dedupe_hash: None,
                timestamp: row.get(2)?,
                address: row.get(3)?,
                body: row.get(4)?,
                body_searchable: row.get(5)?,
                message_type: match message_type {
                    2 => MessageType::Mms,
                    3 => MessageType::Rcs,
                    _ => MessageType::Sms,
                },
                direction: MessageDirection::from_i32(message_direction),
                thread_id: row.get(8)?,
                attachments: Vec::new(),
                contact_name: row.get(9)?,
            };
            Ok(msg)
        })?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

pub struct TantivyBackend {
    #[cfg(feature = "tantivy")]
    index: tantivy::Index,
    #[cfg(feature = "tantivy")]
    schema: tantivy::schema::Schema,
    #[cfg(feature = "tantivy")]
    fields: TantivyFields,
}

#[derive(Debug, Clone)]
pub struct SemanticHit {
    pub message: Message,
    pub score: f32,
}

#[derive(Debug)]
struct ScoreEntry {
    score: f32,
    message: Message,
}

impl PartialEq for ScoreEntry {
    fn eq(&self, other: &Self) -> bool {
        self.score.to_bits() == other.score.to_bits()
    }
}

impl Eq for ScoreEntry {}

impl PartialOrd for ScoreEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoreEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score.total_cmp(&other.score)
    }
}

pub fn semantic_search(
    db_path: &std::path::Path,
    model_id: &str,
    query_embedding: &[f32],
    top_k: usize,
) -> Result<Vec<SemanticHit>> {
    if query_embedding.is_empty() || top_k == 0 {
        return Ok(Vec::new());
    }
    let db = Database::open(db_path, sms_config::ResourceProfile::detect())?;
    let conn = db.connection();
    let mut stmt = conn.prepare(
        "SELECT embeddings.message_id, embeddings.vector, embeddings.dims, \
                messages.timestamp, messages.address, messages.body, messages.body_searchable, \
                messages.message_type, messages.message_direction, messages.thread_id, messages.contact_name \
         FROM embeddings \
         JOIN messages ON messages.id = embeddings.message_id \
         WHERE embeddings.model_id = ?1",
    )?;
    let mut rows = stmt.query([model_id])?;

    let query_norm = l2_norm(query_embedding);
    if query_norm == 0.0 {
        return Ok(Vec::new());
    }

    let mut heap: BinaryHeap<Reverse<ScoreEntry>> = BinaryHeap::with_capacity(top_k + 1);
    while let Some(row) = rows.next()? {
        let message_id: String = row.get(0)?;
        let bytes: Vec<u8> = row.get(1)?;
        let dims: i64 = row.get(2)?;
        if dims as usize != query_embedding.len() {
            continue;
        }
        let embedding = match decode_f32_vec(&bytes, dims as usize) {
            Some(v) => v,
            None => continue,
        };
        let score = cosine_similarity(query_embedding, query_norm, &embedding);
        let message_type: i32 = row.get(7)?;
        let message_direction: i32 = row.get(8)?;
        let msg = Message {
            id: uuid::Uuid::parse_str(&message_id).unwrap_or_else(|_| uuid::Uuid::new_v4()),
            message_id: None,
            dedupe_hash: None,
            timestamp: row.get(3)?,
            address: row.get(4)?,
            body: row.get(5)?,
            body_searchable: row.get(6)?,
            message_type: match message_type {
                2 => MessageType::Mms,
                3 => MessageType::Rcs,
                _ => MessageType::Sms,
            },
            direction: MessageDirection::from_i32(message_direction),
            thread_id: row.get(9)?,
            attachments: Vec::new(),
            contact_name: row.get(10)?,
        };
        if heap.len() < top_k {
            heap.push(Reverse(ScoreEntry {
                score,
                message: msg,
            }));
        } else if let Some(Reverse(min)) = heap.peek() {
            if score > min.score {
                heap.pop();
                heap.push(Reverse(ScoreEntry {
                    score,
                    message: msg,
                }));
            }
        }
    }

    let mut hits: Vec<SemanticHit> = heap
        .into_sorted_vec()
        .into_iter()
        .map(|Reverse(entry)| SemanticHit {
            message: entry.message,
            score: entry.score,
        })
        .collect();
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    Ok(hits)
}

fn decode_f32_vec(bytes: &[u8], dims: usize) -> Option<Vec<f32>> {
    let expected = dims.saturating_mul(4);
    if bytes.len() < expected {
        return None;
    }
    let mut out = Vec::with_capacity(dims);
    for chunk in bytes[..expected].chunks_exact(4) {
        let v = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        out.push(v);
    }
    Some(out)
}

fn l2_norm(vec: &[f32]) -> f32 {
    let mut sum = 0.0f32;
    for v in vec {
        sum += v * v;
    }
    sum.sqrt()
}

fn cosine_similarity(query: &[f32], query_norm: f32, candidate: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut cand_norm = 0.0f32;
    for (a, b) in query.iter().zip(candidate.iter()) {
        dot += a * b;
        cand_norm += b * b;
    }
    if cand_norm == 0.0 {
        0.0
    } else {
        dot / (query_norm * cand_norm.sqrt())
    }
}

#[cfg(feature = "tantivy")]
#[derive(Clone, Copy)]
struct TantivyFields {
    id: tantivy::schema::Field,
    message_id: tantivy::schema::Field,
    address: tantivy::schema::Field,
    body: tantivy::schema::Field,
    body_search: tantivy::schema::Field,
    thread_id: tantivy::schema::Field,
    timestamp: tantivy::schema::Field,
    message_type: tantivy::schema::Field,
}

#[derive(Default, Clone)]
pub struct TantivyFilter {
    pub address: Option<String>,
    pub thread_id: Option<String>,
    pub message_type: Option<i64>,
    pub since: Option<i64>,
    pub until: Option<i64>,
}

impl TantivyBackend {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        #[cfg(feature = "tantivy")]
        {
            let index = tantivy::Index::open_in_dir(path)?;
            let schema = index.schema();
            let fields = TantivyFields {
                id: schema.get_field("id").ok_or_else(|| {
                    sms_errors::AppError::SearchUnsupported("missing field id".into())
                })?,
                message_id: schema.get_field("message_id").ok_or_else(|| {
                    sms_errors::AppError::SearchUnsupported("missing field message_id".into())
                })?,
                address: schema.get_field("address").ok_or_else(|| {
                    sms_errors::AppError::SearchUnsupported("missing field address".into())
                })?,
                body: schema.get_field("body").ok_or_else(|| {
                    sms_errors::AppError::SearchUnsupported("missing field body".into())
                })?,
                body_search: schema.get_field("body_search").ok_or_else(|| {
                    sms_errors::AppError::SearchUnsupported("missing field body_search".into())
                })?,
                thread_id: schema.get_field("thread_id").ok_or_else(|| {
                    sms_errors::AppError::SearchUnsupported("missing field thread_id".into())
                })?,
                timestamp: schema.get_field("timestamp").ok_or_else(|| {
                    sms_errors::AppError::SearchUnsupported("missing field timestamp".into())
                })?,
                message_type: schema.get_field("message_type").ok_or_else(|| {
                    sms_errors::AppError::SearchUnsupported("missing field message_type".into())
                })?,
            };
            Ok(Self {
                index,
                schema,
                fields,
            })
        }
        #[cfg(not(feature = "tantivy"))]
        {
            let _ = path;
            Err(sms_errors::AppError::SearchUnsupported(
                "tantivy feature not enabled".to_string(),
            ))
        }
    }

    pub fn build_index(
        db_path: &std::path::Path,
        index_dir: &std::path::Path,
        rebuild: bool,
    ) -> Result<()> {
        #[cfg(feature = "tantivy")]
        {
            if rebuild && index_dir.exists() {
                std::fs::remove_dir_all(index_dir)?;
            }
            std::fs::create_dir_all(index_dir)?;

            let mut schema_builder = tantivy::schema::Schema::builder();
            let id = schema_builder
                .add_text_field("id", tantivy::schema::STRING | tantivy::schema::STORED);
            let message_id = schema_builder.add_text_field(
                "message_id",
                tantivy::schema::STRING | tantivy::schema::STORED,
            );
            let address = schema_builder
                .add_text_field("address", tantivy::schema::TEXT | tantivy::schema::STORED);
            let body = schema_builder.add_text_field("body", tantivy::schema::STORED);
            let body_search = schema_builder.add_text_field("body_search", tantivy::schema::TEXT);
            let thread_id = schema_builder.add_text_field(
                "thread_id",
                tantivy::schema::STRING | tantivy::schema::STORED,
            );
            let timestamp = schema_builder
                .add_i64_field("timestamp", tantivy::schema::STORED | tantivy::schema::FAST);
            let message_type = schema_builder.add_i64_field(
                "message_type",
                tantivy::schema::STORED | tantivy::schema::FAST,
            );
            let schema = schema_builder.build();

            let index = tantivy::Index::create_in_dir(index_dir, schema.clone())?;
            let mut writer = index.writer(64_000_000)?;

            let db = Database::open(db_path, sms_config::ResourceProfile::detect())?;
            let conn = db.connection();
            let mut stmt = conn.prepare(
                "SELECT id, message_id, timestamp, address, body, body_searchable, message_type, thread_id \
                 FROM messages",
            )?;
            let iter = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i32>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            })?;

            for row in iter {
                let (
                    id_v,
                    message_id_v,
                    timestamp_v,
                    address_v,
                    body_v,
                    body_search_v,
                    message_type_v,
                    thread_id_v,
                ) = row?;
                let mut doc = tantivy::Document::default();
                doc.add_text(id, &id_v);
                if let Some(mid) = message_id_v {
                    doc.add_text(message_id, &mid);
                }
                doc.add_i64(timestamp, timestamp_v);
                doc.add_text(address, &address_v);
                doc.add_text(body, &body_v);
                doc.add_text(body_search, &body_search_v);
                if let Some(tid) = thread_id_v {
                    doc.add_text(thread_id, &tid);
                }
                doc.add_i64(message_type, message_type_v as i64);
                writer.add_document(doc)?;
            }

            writer.commit()?;
            Ok(())
        }
        #[cfg(not(feature = "tantivy"))]
        {
            let _ = (db_path, index_dir, rebuild);
            Err(sms_errors::AppError::SearchUnsupported(
                "tantivy feature not enabled".to_string(),
            ))
        }
    }

    pub fn update_index(db_path: &std::path::Path, index_dir: &std::path::Path) -> Result<()> {
        #[cfg(feature = "tantivy")]
        {
            let index = tantivy::Index::open_in_dir(index_dir)?;
            let schema = index.schema();
            let fields = TantivyFields {
                id: schema.get_field("id").ok_or_else(|| {
                    sms_errors::AppError::SearchUnsupported("missing field id".into())
                })?,
                message_id: schema.get_field("message_id").ok_or_else(|| {
                    sms_errors::AppError::SearchUnsupported("missing field message_id".into())
                })?,
                address: schema.get_field("address").ok_or_else(|| {
                    sms_errors::AppError::SearchUnsupported("missing field address".into())
                })?,
                body: schema.get_field("body").ok_or_else(|| {
                    sms_errors::AppError::SearchUnsupported("missing field body".into())
                })?,
                body_search: schema.get_field("body_search").ok_or_else(|| {
                    sms_errors::AppError::SearchUnsupported("missing field body_search".into())
                })?,
                thread_id: schema.get_field("thread_id").ok_or_else(|| {
                    sms_errors::AppError::SearchUnsupported("missing field thread_id".into())
                })?,
                timestamp: schema.get_field("timestamp").ok_or_else(|| {
                    sms_errors::AppError::SearchUnsupported("missing field timestamp".into())
                })?,
                message_type: schema.get_field("message_type").ok_or_else(|| {
                    sms_errors::AppError::SearchUnsupported("missing field message_type".into())
                })?,
            };

            let db = Database::open(db_path, sms_config::ResourceProfile::detect())?;
            let conn = db.connection();
            let existing =
                conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))?;

            let reader = index.reader()?;
            let searcher = reader.searcher();
            let count = searcher
                .segment_readers()
                .iter()
                .map(|s| s.num_docs() as i64)
                .sum::<i64>();
            if count >= existing {
                return Ok(());
            }

            let mut writer = index.writer(64_000_000)?;
            let mut stmt = conn.prepare(
                "SELECT id, message_id, timestamp, address, body, body_searchable, message_type, thread_id \
                 FROM messages \
                 WHERE rowid > ?1 \
                 ORDER BY rowid ASC",
            )?;
            let iter = stmt.query_map([count], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i32>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            })?;
            for row in iter {
                let (
                    id_v,
                    message_id_v,
                    timestamp_v,
                    address_v,
                    body_v,
                    body_search_v,
                    message_type_v,
                    thread_id_v,
                ) = row?;
                let mut doc = tantivy::Document::default();
                doc.add_text(fields.id, &id_v);
                if let Some(mid) = message_id_v {
                    doc.add_text(fields.message_id, &mid);
                }
                doc.add_i64(fields.timestamp, timestamp_v);
                doc.add_text(fields.address, &address_v);
                doc.add_text(fields.body, &body_v);
                doc.add_text(fields.body_search, &body_search_v);
                if let Some(tid) = thread_id_v {
                    doc.add_text(fields.thread_id, &tid);
                }
                doc.add_i64(fields.message_type, message_type_v as i64);
                writer.add_document(doc)?;
            }
            writer.commit()?;
            Ok(())
        }
        #[cfg(not(feature = "tantivy"))]
        {
            let _ = (db_path, index_dir);
            Err(sms_errors::AppError::SearchUnsupported(
                "tantivy feature not enabled".to_string(),
            ))
        }
    }

    pub fn search_filtered(
        &self,
        query: &str,
        limit: usize,
        filter: &TantivyFilter,
    ) -> Result<Vec<Message>> {
        #[cfg(feature = "tantivy")]
        {
            use tantivy::collector::TopDocs;
            use tantivy::query::QueryParser;

            let reader = self.index.reader()?;
            let searcher = reader.searcher();
            let query_parser = QueryParser::for_index(
                &self.index,
                vec![
                    self.fields.body_search,
                    self.fields.address,
                    self.fields.message_id,
                ],
            );
            let query = query_parser.parse_query(query)?;

            let mut multiplier = 5usize;
            let mut out = Vec::new();
            while out.len() < limit {
                let top_docs = searcher.search(&query, &TopDocs::with_limit(limit * multiplier))?;
                out.clear();
                for (_score, doc_address) in top_docs {
                    let doc = searcher.doc(doc_address)?;
                    let msg = materialize_message(&doc, &self.fields);
                    if !filter_match(&msg, filter) {
                        continue;
                    }
                    out.push(msg);
                    if out.len() >= limit {
                        break;
                    }
                }
                if out.len() >= limit || multiplier >= 20 {
                    break;
                }
                multiplier *= 2;
            }
            Ok(out)
        }
        #[cfg(not(feature = "tantivy"))]
        {
            let _ = (self, query, limit, filter);
            Err(sms_errors::AppError::SearchUnsupported(
                "tantivy feature not enabled".to_string(),
            ))
        }
    }
}

impl SearchBackend for TantivyBackend {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Message>> {
        #[cfg(feature = "tantivy")]
        {
            use tantivy::collector::TopDocs;
            use tantivy::query::QueryParser;

            let reader = self.index.reader()?;
            let searcher = reader.searcher();
            let query_parser = QueryParser::for_index(
                &self.index,
                vec![
                    self.fields.body_search,
                    self.fields.address,
                    self.fields.message_id,
                ],
            );
            let query = query_parser.parse_query(query)?;
            let top_docs = searcher.search(&query, &TopDocs::with_limit(limit))?;

            let mut out = Vec::with_capacity(top_docs.len());
            for (_score, doc_address) in top_docs {
                let doc = searcher.doc(doc_address)?;
                out.push(materialize_message(&doc, &self.fields));
            }
            Ok(out)
        }
        #[cfg(not(feature = "tantivy"))]
        {
            let _ = (self, query, limit);
            Err(sms_errors::AppError::SearchUnsupported(
                "tantivy feature not enabled".to_string(),
            ))
        }
    }
}

#[cfg(feature = "tantivy")]
fn get_text(doc: &tantivy::Document, field: tantivy::schema::Field) -> Option<String> {
    doc.get_first(field)
        .and_then(|v| v.as_text())
        .map(|s| s.to_string())
}

#[cfg(feature = "tantivy")]
fn get_i64(doc: &tantivy::Document, field: tantivy::schema::Field) -> Option<i64> {
    doc.get_first(field).and_then(|v| v.as_i64())
}

#[cfg(feature = "tantivy")]
fn materialize_message(doc: &tantivy::Document, fields: &TantivyFields) -> Message {
    let id = get_text(doc, fields.id).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    // #todo: index message_direction in tantivy for direction-aware search filters.
    Message {
        id: uuid::Uuid::parse_str(&id).unwrap_or_else(|_| uuid::Uuid::new_v4()),
        message_id: get_text(doc, fields.message_id),
        dedupe_hash: None,
        timestamp: get_i64(doc, fields.timestamp).unwrap_or(0),
        address: get_text(doc, fields.address).unwrap_or_default(),
        body: get_text(doc, fields.body).unwrap_or_default(),
        body_searchable: get_text(doc, fields.body_search).unwrap_or_default(),
        message_type: match get_i64(doc, fields.message_type).unwrap_or(1) {
            2 => MessageType::Mms,
            3 => MessageType::Rcs,
            _ => MessageType::Sms,
        },
        direction: MessageDirection::Unknown,
        thread_id: get_text(doc, fields.thread_id),
        attachments: Vec::new(),
        // #todo: index contact_name in tantivy and read it back here once added.
        contact_name: None,
    }
}

#[cfg(feature = "tantivy")]
fn filter_match(msg: &Message, filter: &TantivyFilter) -> bool {
    if let Some(since) = filter.since {
        if msg.timestamp < since {
            return false;
        }
    }
    if let Some(until) = filter.until {
        if msg.timestamp > until {
            return false;
        }
    }
    if let Some(mt) = filter.message_type {
        if msg.message_type as i64 != mt {
            return false;
        }
    }
    if let Some(addr) = &filter.address {
        if &msg.address != addr {
            return false;
        }
    }
    if let Some(thread) = &filter.thread_id {
        if msg.thread_id.as_deref() != Some(thread.as_str()) {
            return false;
        }
    }
    true
}
