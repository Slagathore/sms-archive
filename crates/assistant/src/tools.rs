use anyhow::Result;
use chrono::TimeZone;
use rusqlite::params;
use serde_json::{json, Value};
use sms_config::ResourceProfile;
use sms_db::Database;
use sms_search::{Fts5Backend, SearchBackend};
use std::path::Path;

pub fn get_assistant_tools() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "search_messages",
                "description": "Search for messages containing specific text",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The search query"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum results",
                            "default": 10
                        }
                    },
                    "required": ["query"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "get_thread",
                "description": "Get conversation history with a contact",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "contact": {
                            "type": "string",
                            "description": "Phone number or contact name"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum messages",
                            "default": 50
                        }
                    },
                    "required": ["contact"]
                }
            }
        }),
    ]
}

pub fn execute_tool(db_path: &str, tool_name: &str, params: &Value) -> Result<String> {
    match tool_name {
        "search_messages" => tool_search_messages(db_path, params),
        "get_thread" => tool_get_thread(db_path, params),
        _ => Err(anyhow::anyhow!("Unknown tool: {}", tool_name)),
    }
}

fn normalize_phone_like(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() == 11 && digits.starts_with('1') {
        digits = digits[1..].to_string();
    }
    if trimmed.starts_with('+') {
        format!("+{}", digits)
    } else {
        digits
    }
}

fn tool_search_messages(db_path: &str, params: &Value) -> Result<String> {
    if db_path.trim().is_empty() {
        return Ok("Open a database first.".to_string());
    }
    let query = params
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing query"))?;
    let limit = params
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(10)
        .max(1)
        .min(100) as usize;
    let backend = Fts5Backend::open(Path::new(db_path), ResourceProfile::detect())?;
    let mut results = backend.search(query, limit)?;
    if results.is_empty() {
        let conn = backend.connection();
        let like = format!("%{}%", query);
        let sql = "SELECT id, message_id, timestamp, address, body, body_searchable, message_type, message_direction, thread_id, contact_name \
               FROM messages WHERE body LIKE ?1 OR body_searchable LIKE ?1 \
               ORDER BY timestamp DESC LIMIT ?2";
        // #todo: add optional direction filter to assistant fallback searches.
        if let Ok(mut stmt) = conn.prepare(sql) {
            let rows = stmt.query_map(params![like, limit as i64], |row| {
                let message_type: i32 = row.get(6)?;
                let message_direction: i32 = row.get(7)?;
                Ok(sms_types::Message {
                    id: uuid::Uuid::parse_str(&row.get::<_, String>(0)?)
                        .unwrap_or_else(|_| uuid::Uuid::new_v4()),
                    message_id: row.get(1)?,
                    dedupe_hash: None,
                    timestamp: row.get(2)?,
                    address: row.get(3)?,
                    body: row.get(4)?,
                    body_searchable: row.get(5)?,
                    message_type: match message_type {
                        2 => sms_types::MessageType::Mms,
                        3 => sms_types::MessageType::Rcs,
                        _ => sms_types::MessageType::Sms,
                    },
                    direction: sms_types::MessageDirection::from_i32(message_direction),
                    thread_id: row.get(8)?,
                    attachments: Vec::new(),
                    contact_name: row.get(9)?,
                })
            })?;
            for row in rows.flatten() {
                results.push(row);
            }
        }
    }
    if results.is_empty() {
        return Ok(format!("No messages found for '{}'", query));
    }
    let mut out = String::new();
    out.push_str(&format!(
        "Found {} message(s) for '{}':\n",
        results.len(),
        query
    ));
    for msg in results {
        let ts = format_timestamp(msg.timestamp);
        let body = summarize_body(&msg.body, 160);
        out.push_str(&format!("- {} | {}: {}\n", ts, msg.address, body));
    }
    Ok(out)
}

fn tool_get_thread(db_path: &str, params: &Value) -> Result<String> {
    if db_path.trim().is_empty() {
        return Ok("Open a database first.".to_string());
    }
    let contact = params
        .get("contact")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing contact"))?;
    let limit = params
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(50)
        .max(1)
        .min(200) as usize;

    let db = Database::open(Path::new(db_path), ResourceProfile::detect())?;
    let conn = db.connection();

    let mut addresses = Vec::new();
    let like = format!("%{}%", contact);
    if let Ok(mut stmt) = conn.prepare(
        "SELECT contact_addresses.address \
         FROM contact_addresses \
         JOIN contacts ON contacts.id = contact_addresses.contact_id \
         WHERE contacts.display_name LIKE ?1",
    ) {
        if let Ok(rows) = stmt.query_map([like], |row| row.get::<_, String>(0)) {
            for row in rows.flatten() {
                addresses.push(row);
            }
        }
    }
    if addresses.is_empty() {
        addresses.push(contact.to_string());
    }
    let normalized = normalize_phone_like(contact);
    if !normalized.is_empty() && !addresses.iter().any(|a| a == &normalized) {
        addresses.push(normalized);
    }

    let placeholders = std::iter::repeat("?")
        .take(addresses.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT timestamp, address, body FROM messages WHERE address IN ({}) ORDER BY timestamp DESC LIMIT ?",
        placeholders
    );
    let mut params_vec: Vec<rusqlite::types::Value> =
        addresses.iter().map(|v| v.clone().into()).collect();
    params_vec.push((limit as i64).into());

    let mut out = String::new();
    if let Ok(mut stmt) = conn.prepare(&sql) {
        let rows = stmt.query_map(rusqlite::params_from_iter(params_vec), |row| {
            let timestamp: i64 = row.get(0)?;
            let address: String = row.get(1)?;
            let body: String = row.get(2)?;
            Ok((timestamp, address, body))
        })?;
        let mut count = 0;
        for row in rows.flatten() {
            count += 1;
            let ts = format_timestamp(row.0);
            let body = summarize_body(&row.2, 160);
            out.push_str(&format!("- {} | {}: {}\n", ts, row.1, body));
        }
        if count == 0 {
            return Ok(format!("No messages found for '{}'", contact));
        }
    }

    Ok(out)
}

fn summarize_body(body: &str, max_len: usize) -> String {
    let trimmed = body.trim();
    if trimmed.len() <= max_len {
        trimmed.to_string()
    } else {
        // Cut on a char boundary at or below max_len — a raw byte slice
        // panics when a multi-byte character straddles the cutoff.
        let cut = (0..=max_len)
            .rev()
            .find(|&i| trimmed.is_char_boundary(i))
            .unwrap_or(0);
        let mut s = trimmed[..cut].to_string();
        s.push('…');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::summarize_body;

    #[test]
    fn summarize_body_truncates_on_char_boundary() {
        // "ééééé" is 10 bytes / 5 chars; byte 5 falls mid-character.
        let body = "ééééé";
        let out = summarize_body(body, 5);
        assert_eq!(out, "éé…");
    }

    #[test]
    fn summarize_body_passes_short_bodies_through() {
        assert_eq!(summarize_body("  hi  ", 160), "hi");
    }
}

fn format_timestamp(timestamp_ms: i64) -> String {
    if let Some(dt) = chrono::Local.timestamp_millis_opt(timestamp_ms).single() {
        dt.format("%Y-%m-%d %H:%M").to_string()
    } else {
        timestamp_ms.to_string()
    }
}
