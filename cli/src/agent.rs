use crate::config::ProviderConfig;
use crate::tools;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use std::io::Write;

pub struct Agent {
    client: Client,
    provider: ProviderConfig,
    pub conversation: Vec<Value>,
    max_rounds: u32,
    cwd: String,
    _last_reasoning: Option<String>,
}

impl Agent {
    pub fn new(provider: ProviderConfig, max_rounds: u32, cwd: String, system: Option<String>) -> Self {
        let mut conversation = Vec::new();
        if let Some(sys) = system {
            if !provider.is_anthropic {
                conversation.push(json!({ "role": "system", "content": sys }));
            }
        }
        Self {
            client: Client::new(),
            provider,
            conversation,
            max_rounds,
            cwd,
            _last_reasoning: None,
        }
    }

    pub async fn send(&mut self, user_message: &str) -> Result<String, String> {
        self.conversation.push(json!({ "role": "user", "content": user_message }));

        let tool_defs = tools::tool_definitions();
        let mut round = 0u32;
        let mut final_text = String::new();

        loop {
            round += 1;
            if round > self.max_rounds {
                break;
            }

            let (text, tool_calls) = if self.provider.is_anthropic {
                self.call_anthropic(&tool_defs).await?
            } else {
                self.call_openai(&tool_defs).await?
            };

            if !text.is_empty() {
                final_text = text.clone();
            }

            if tool_calls.is_empty() {
                // No more tool calls — done
                if !text.is_empty() {
                    let mut msg = json!({ "role": "assistant", "content": text });
                    if let Some(r) = self._last_reasoning.take() {
                        msg["reasoning_content"] = json!(r);
                    }
                    self.conversation.push(msg);
                }
                break;
            }

            // Record assistant message with tool_calls
            let mut msg = json!({
                "role": "assistant",
                "content": if text.is_empty() { Value::Null } else { Value::String(text.clone()) },
                "tool_calls": tool_calls
            });
            if let Some(r) = self._last_reasoning.take() {
                msg["reasoning_content"] = json!(r);
            }
            self.conversation.push(msg);

            // Execute each tool and collect results
            for tc in &tool_calls {
                let tool_id = tc["id"].as_str().unwrap_or("call_0").to_string();
                let tool_name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
                let args: Value = serde_json::from_str(args_str).unwrap_or(json!({}));

                eprint!("\x1b[2m[{}] {}", tool_name, args_str.chars().take(80).collect::<String>());
                if args_str.len() > 80 { eprint!("…"); }
                eprintln!("\x1b[0m");

                let (success, result) = tools::call_tool(&tool_name, &args, &self.cwd).await;

                if !success {
                    eprintln!("\x1b[33m  ! error: {}\x1b[0m", result.chars().take(120).collect::<String>());
                }

                self.conversation.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_id,
                    "content": result
                }));
            }
        }

        Ok(final_text)
    }

    async fn call_openai(&mut self, tools: &[Value]) -> Result<(String, Vec<Value>), String> {
        let url = format!("{}/chat/completions", self.provider.base_url.trim_end_matches('/'));

        let body = json!({
            "model": self.provider.model,
            "messages": self.conversation,
            "tools": tools,
            "tool_choice": "auto",
            "stream": true,
        });

        let resp = self.client
            .post(&url)
            .bearer_auth(&self.provider.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("API {} — {}", status, body));
        }

        // Stream SSE
        let mut stream = resp.bytes_stream();
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tool_calls_map: std::collections::HashMap<u64, (String, String, String)> =
            std::collections::HashMap::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("stream error: {}", e))?;
            let raw = String::from_utf8_lossy(&chunk);

            for line in raw.lines() {
                if !line.starts_with("data: ") { continue; }
                let data = &line[6..];
                if data == "[DONE]" { break; }

                let Ok(val): Result<Value, _> = serde_json::from_str(data) else { continue };
                let choices = val["choices"].as_array();
                let Some(choices) = choices else { continue };
                let Some(choice) = choices.first() else { continue };
                let delta = &choice["delta"];

                // DeepSeek thinking models return reasoning_content — capture it
                if let Some(r) = delta["reasoning_content"].as_str() {
                    reasoning.push_str(r);
                }

                // Text content
                if let Some(c) = delta["content"].as_str() {
                    text.push_str(c);
                    print!("{}", c);
                    let _ = std::io::stdout().flush();
                }

                // Tool call deltas
                if let Some(tc_arr) = delta["tool_calls"].as_array() {
                    for tc_delta in tc_arr {
                        let idx = tc_delta["index"].as_u64().unwrap_or(0);
                        let entry = tool_calls_map.entry(idx).or_insert_with(|| {
                            let id = tc_delta["id"].as_str().unwrap_or("").to_string();
                            let name = tc_delta["function"]["name"].as_str().unwrap_or("").to_string();
                            (id, name, String::new())
                        });
                        if entry.0.is_empty() {
                            if let Some(id) = tc_delta["id"].as_str() { entry.0 = id.to_string(); }
                        }
                        if entry.1.is_empty() {
                            if let Some(name) = tc_delta["function"]["name"].as_str() {
                                entry.1 = name.to_string();
                            }
                        }
                        if let Some(args_chunk) = tc_delta["function"]["arguments"].as_str() {
                            entry.2.push_str(args_chunk);
                        }
                    }
                }
            }
        }

        if !text.is_empty() { println!(); }

        // Store reasoning so it's sent back on the next turn (DeepSeek requirement)
        self._last_reasoning = if reasoning.is_empty() { None } else { Some(reasoning) };

        let mut tool_calls = Vec::new();
        let mut indices: Vec<u64> = tool_calls_map.keys().cloned().collect();
        indices.sort();
        for idx in indices {
            let (id, name, args) = tool_calls_map.remove(&idx).unwrap();
            tool_calls.push(json!({
                "id": if id.is_empty() { format!("call_{}", idx) } else { id },
                "type": "function",
                "function": { "name": name, "arguments": args }
            }));
        }

        Ok((text, tool_calls))
    }

    async fn call_anthropic(&self, tools: &[Value]) -> Result<(String, Vec<Value>), String> {
        let url = "https://api.anthropic.com/v1/messages";

        // Separate system from conversation
        let system_msg = self.conversation.iter()
            .find(|m| m["role"] == "system")
            .and_then(|m| m["content"].as_str())
            .unwrap_or("")
            .to_string();

        let messages: Vec<&Value> = self.conversation.iter()
            .filter(|m| m["role"] != "system")
            .collect();

        // Convert tools to Anthropic format
        let ant_tools: Vec<Value> = tools.iter().map(|t| {
            let f = &t["function"];
            json!({
                "name": f["name"],
                "description": f["description"],
                "input_schema": f["parameters"]
            })
        }).collect();

        let mut body = json!({
            "model": self.provider.model,
            "max_tokens": 8192,
            "messages": messages,
            "tools": ant_tools,
            "stream": true,
        });
        if !system_msg.is_empty() {
            body["system"] = json!(system_msg);
        }

        let resp = self.client
            .post(url)
            .header("x-api-key", &self.provider.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("anthropic request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Anthropic API {} — {}", status, body));
        }

        let mut stream = resp.bytes_stream();
        let mut text = String::new();
        let mut tool_calls: Vec<Value> = Vec::new();
        let mut current_tool: Option<(String, String, String)> = None; // id, name, args

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("stream error: {}", e))?;
            let raw = String::from_utf8_lossy(&chunk);

            for line in raw.lines() {
                if !line.starts_with("data: ") { continue; }
                let data = &line[6..];
                let Ok(val): Result<Value, _> = serde_json::from_str(data) else { continue };

                match val["type"].as_str() {
                    Some("content_block_start") => {
                        let block = &val["content_block"];
                        if block["type"] == "tool_use" {
                            current_tool = Some((
                                block["id"].as_str().unwrap_or("").to_string(),
                                block["name"].as_str().unwrap_or("").to_string(),
                                String::new(),
                            ));
                        }
                    }
                    Some("content_block_delta") => {
                        let delta = &val["delta"];
                        if delta["type"] == "text_delta" {
                            if let Some(c) = delta["text"].as_str() {
                                text.push_str(c);
                                print!("{}", c);
                                let _ = std::io::stdout().flush();
                            }
                        } else if delta["type"] == "input_json_delta" {
                            if let Some(ref mut tc) = current_tool {
                                if let Some(chunk) = delta["partial_json"].as_str() {
                                    tc.2.push_str(chunk);
                                }
                            }
                        }
                    }
                    Some("content_block_stop") => {
                        if let Some((id, name, args)) = current_tool.take() {
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": { "name": name, "arguments": args }
                            }));
                        }
                    }
                    _ => {}
                }
            }
        }

        if !text.is_empty() { println!(); }
        Ok((text, tool_calls))
    }
}
