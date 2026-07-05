use anyhow::Result;
use serde_json::Value;

mod chat;
mod ollama;
mod tools;

pub use chat::ChatMessage;

#[derive(Debug, Clone)]
pub struct Assistant {
    pub messages: Vec<ChatMessage>,
    pub ollama_url: String,
    pub model: String,
}

impl Assistant {
    pub fn new(ollama_url: String, model: String) -> Self {
        Self {
            messages: Vec::new(),
            ollama_url,
            model,
        }
    }

    pub fn complete_chat(
        &self,
        mut messages: Vec<ChatMessage>,
        db_path: &str,
    ) -> Result<Vec<ChatMessage>> {
        // Bounded tool loop: the model may chain tool calls (e.g. search,
        // then pull the matching thread) before answering. A single round —
        // the old behavior — made chained lookups impossible.
        const MAX_TOOL_ROUNDS: usize = 5;

        for _ in 0..MAX_TOOL_ROUNDS {
            let response = ollama::send_ollama_chat_with_tools(
                &messages,
                &self.ollama_url,
                &self.model,
                &tools::get_assistant_tools(),
            )?;

            let tool_calls = match ollama::extract_tool_calls(&response) {
                Some(calls) if !calls.is_empty() => calls,
                _ => {
                    let content = ollama::extract_message_content(&response)?;
                    messages.push(ChatMessage::new("assistant", content));
                    return Ok(messages);
                }
            };
            for call in tool_calls {
                let (tool_name, args) = parse_tool_call(call)?;
                let tool_result = match tools::execute_tool(db_path, &tool_name, &args) {
                    Ok(result) => result,
                    Err(err) => format!("Tool error: {}", err),
                };
                messages.push(ChatMessage::new(
                    "tool",
                    format!("{}:\n{}", tool_name, tool_result.trim()),
                ));
            }
        }

        // Round cap hit — request a final answer without tools so the user
        // always gets text back instead of an endless tool chain.
        let final_response = ollama::send_ollama_chat(&messages, &self.ollama_url, &self.model)?;
        messages.push(ChatMessage::new("assistant", final_response));
        Ok(messages)
    }

    pub fn get_messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    pub fn clear_history(&mut self) {
        self.messages.clear();
    }
}

fn parse_tool_call(call: &Value) -> Result<(String, Value)> {
    let func = call
        .get("function")
        .ok_or_else(|| anyhow::anyhow!("Tool call missing function"))?;
    let name = func
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Tool call missing name"))?;
    let args_value = func
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    let args = if let Some(arg_str) = args_value.as_str() {
        serde_json::from_str(arg_str).unwrap_or_else(|_| Value::Object(Default::default()))
    } else {
        args_value
    };
    Ok((name.to_string(), args))
}
