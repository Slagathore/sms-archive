use anyhow::Result;
use serde_json::{json, Value};

use crate::ChatMessage;

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
    let response = ureq::post(&url).send_json(&body)?;
    let json: Value = response.into_json()?;
    Ok(json)
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
