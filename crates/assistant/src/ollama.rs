use anyhow::Result;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use crate::ChatMessage;

/// Shared HTTP agent with explicit timeouts. The default `ureq` agent has no
/// overall timeout, so a hung Ollama server would block the worker thread
/// forever with no way for the UI to recover. The overall cap is generous
/// because large local models can legitimately take minutes to respond.
fn http_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout(Duration::from_secs(600))
            .build()
    })
}

pub fn send_ollama_chat(messages: &[ChatMessage], ollama_url: &str, model: &str) -> Result<String> {
    let response = send_ollama_chat_raw(messages, ollama_url, model, None)?;
    extract_message_content(&response)
}

pub fn send_ollama_chat_with_tools(
    messages: &[ChatMessage],
    ollama_url: &str,
    model: &str,
    tools: &[Value],
) -> Result<Value> {
    send_ollama_chat_raw(messages, ollama_url, model, Some(tools))
}

fn send_ollama_chat_raw(
    messages: &[ChatMessage],
    ollama_url: &str,
    model: &str,
    tools: Option<&[Value]>,
) -> Result<Value> {
    let url = format!("{}/api/chat", ollama_url.trim_end_matches('/'));
    let mut body = json!({
        "model": model,
        "messages": messages
            .iter()
            .map(|m| {
                json!({
                    "role": m.role,
                    "content": m.content,
                })
            })
            .collect::<Vec<_>>(),
        "stream": false,
    });
    if let Some(tools) = tools {
        body["tools"] = json!(tools);
    }
    let response = http_agent().post(&url).send_json(&body)?;
    let json: Value = response.into_json()?;
    Ok(json)
}

/// Result of a streamed chat turn: the full accumulated content plus any
/// tool calls the model requested (tool calls arrive in the final chunk).
pub struct StreamOutcome {
    pub content: String,
    pub tool_calls: Vec<Value>,
}

/// Stream a chat completion, invoking `on_delta` for each content chunk as it
/// arrives (so the UI can render tokens live) and checking `cancel` between
/// chunks so a "Stop" button aborts promptly. Returns the accumulated content
/// and any tool calls seen.
pub fn stream_ollama_chat(
    messages: &[ChatMessage],
    ollama_url: &str,
    model: &str,
    tools: Option<&[Value]>,
    cancel: &AtomicBool,
    mut on_delta: impl FnMut(&str),
) -> Result<StreamOutcome> {
    use std::io::BufRead;
    let url = format!("{}/api/chat", ollama_url.trim_end_matches('/'));
    let mut body = json!({
        "model": model,
        "messages": messages
            .iter()
            .map(|m| json!({ "role": m.role, "content": m.content }))
            .collect::<Vec<_>>(),
        "stream": true,
    });
    if let Some(tools) = tools {
        body["tools"] = json!(tools);
    }
    let response = http_agent().post(&url).send_json(&body)?;
    let reader = std::io::BufReader::new(response.into_reader());
    let mut content = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for line in reader.lines() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(msg) = value.get("message") {
            if let Some(delta) = msg.get("content").and_then(|c| c.as_str()) {
                if !delta.is_empty() {
                    content.push_str(delta);
                    on_delta(delta);
                }
            }
            if let Some(calls) = msg.get("tool_calls").and_then(|c| c.as_array()) {
                tool_calls.extend(calls.iter().cloned());
            }
        }
        if value.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
            break;
        }
    }
    Ok(StreamOutcome {
        content,
        tool_calls,
    })
}

pub fn extract_message_content(response: &Value) -> Result<String> {
    response
        .get("message")
        .and_then(|msg| msg.get("content"))
        .and_then(|content| content.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("No content in Ollama response"))
}

pub fn extract_tool_calls(response: &Value) -> Option<&Vec<Value>> {
    response
        .get("message")
        .and_then(|msg| msg.get("tool_calls"))
        .and_then(|calls| calls.as_array())
        .or_else(|| {
            response
                .get("tool_calls")
                .and_then(|calls| calls.as_array())
        })
}
