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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_message() -> Message {
        Message {
            id: uuid::Uuid::new_v4(),
            message_id: None,
            dedupe_hash: None,
            timestamp: 0,
            address: String::new(),
            body: String::new(),
            body_searchable: String::new(),
            message_type: MessageType::Sms,
            direction: MessageDirection::Unknown,
            thread_id: None,
            attachments: Vec::new(),
            contact_name: None,
        }
    }

    #[test]
    fn sanitize_fts5_query_quotes_plain_words() {
        assert_eq!(sanitize_fts5_query("hello world"), "\"hello\" \"world\"");
    }

    #[test]
    fn sanitize_fts5_query_doubles_embedded_quotes() {
        // A literal `"` in a token must be doubled per FTS5 quoting rules,
        // then the whole token wrapped in its own quotes.
        assert_eq!(sanitize_fts5_query("say \"hi\""), "\"say\" \"\"\"hi\"\"\"");
    }

    #[test]
    fn sanitize_fts5_query_neutralizes_fts5_operators() {
        // Each of these is meaningful to FTS5's query mini-language unquoted:
        // `don't`/`C++`/`3:30` are syntax errors, `-secret` is a column
        // exclusion, `NOT` is a boolean operator. Quoting must defang all of
        // them into inert phrase terms.
        let sanitized = sanitize_fts5_query("don't C++ 3:30 -secret NOT");
        assert_eq!(sanitized, "\"don't\" \"C++\" \"3:30\" \"-secret\" \"NOT\"");
        assert!(sanitized
            .split(' ')
            .all(|tok| tok.starts_with('"') && tok.ends_with('"')));
    }

    #[test]
    fn sanitize_fts5_query_empty_or_whitespace_is_empty() {
        assert_eq!(sanitize_fts5_query(""), "");
        assert_eq!(sanitize_fts5_query("   \t  "), "");
    }

    #[test]
    fn cosine_similarity_zero_candidate_vector_is_zero_not_nan() {
        let query = [1.0f32, 2.0, 3.0];
        let query_norm = l2_norm(&query);
        let candidate = [0.0f32, 0.0, 0.0];
        let score = cosine_similarity(&query, query_norm, &candidate);
        assert_eq!(score, 0.0);
        assert!(!score.is_nan());
    }

    #[test]
    fn cosine_similarity_identical_vectors_is_one() {
        let query = [1.0f32, 0.0, 0.0];
        let query_norm = l2_norm(&query);
        let score = cosine_similarity(&query, query_norm, &query);
        assert!((score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn score_entry_ordering_matches_score() {
        let low = ScoreEntry {
            score: 0.1,
            message: sample_message(),
        };
        let high = ScoreEntry {
            score: 0.9,
            message: sample_message(),
        };
        assert!(high > low);
        assert_eq!(low.cmp(&low), Ordering::Equal);
    }

    #[test]
    fn top_k_heap_keeps_only_highest_scores() {
        // Mirrors the bounded top-k "keep if better than current min" logic
        // in `semantic_search`: once the heap is full, a new entry is only
        // kept if it beats the current worst kept score.
        let top_k = 2usize;
        let scores = [0.2f32, 0.9, 0.5, 0.1, 0.7];
        let mut heap: BinaryHeap<Reverse<ScoreEntry>> = BinaryHeap::with_capacity(top_k + 1);
        for &score in &scores {
            let entry = ScoreEntry {
                score,
                message: sample_message(),
            };
            if heap.len() < top_k {
                heap.push(Reverse(entry));
            } else if let Some(Reverse(min)) = heap.peek() {
                if score > min.score {
                    heap.pop();
                    heap.push(Reverse(entry));
                }
            }
        }
        assert_eq!(heap.len(), top_k);
        let mut kept: Vec<f32> = heap
            .into_sorted_vec()
            .into_iter()
            .map(|Reverse(e)| e.score)
            .collect();
        kept.sort_by(|a, b| a.total_cmp(b));
        assert_eq!(kept, vec![0.7, 0.9]);
    }
}
