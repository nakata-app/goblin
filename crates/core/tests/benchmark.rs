//! Benchmark suite — measures agent loop quality metrics without
//! real API calls. Uses a scripted mock provider to simulate
//! multi-turn conversations and verify behavioral properties.
//!
//! These are not performance benchmarks (criterion); they are
//! *quality benchmarks* that measure how well the agent handles
//! reference tasks: tool selection, error recovery, output format,
//! and scope discipline.

use aegis_api::{
    ChatChoice, ChatMessage, ChatProvider, ChatRequest, ChatResponse, ToolCall, ToolCallFunction,
    Usage,
};
use aegis_core::{Agent, AgentConfig, AllowAll, ToolContext, ToolRegistry};
use std::sync::Arc;

/// A scripted provider that returns pre-defined responses in order.
struct ScriptedProvider {
    responses: std::sync::Mutex<Vec<ChatResponse>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
        }
    }

    fn text_response(text: &str) -> ChatResponse {
        ChatResponse {
            choices: vec![ChatChoice {
                message: ChatMessage::assistant_text(text),
                finish_reason: Some("stop".into()),
            }],
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }),
        }
    }

    fn tool_call_response(calls: Vec<(&str, &str)>) -> ChatResponse {
        let tool_calls: Vec<ToolCall> = calls
            .into_iter()
            .enumerate()
            .map(|(i, (name, args))| ToolCall {
                id: format!("call_{i}"),
                kind: "function".to_string(),
                function: ToolCallFunction {
                    name: name.to_string(),
                    arguments: args.to_string(),
                },
            })
            .collect();
        ChatResponse {
            choices: vec![ChatChoice {
                message: ChatMessage {
                    role: aegis_api::Role::Assistant,
                    content: None,
                    content_blocks: Vec::new(),
                    tool_calls,
                    tool_call_id: None,
                    name: None,
                    protected: false,
                    reasoning_content: None,
                },
                finish_reason: Some("tool_calls".into()),
            }],
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }),
        }
    }
}

#[async_trait::async_trait]
impl ChatProvider for ScriptedProvider {
    async fn chat(&self, _request: &ChatRequest) -> Result<ChatResponse, aegis_api::ApiError> {
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            Ok(Self::text_response("[no more scripted responses]"))
        } else {
            Ok(responses.remove(0))
        }
    }
}

fn tempdir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("metis-bench-{}-{}", std::process::id(), id));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_agent<'a>(
    provider: &'a ScriptedProvider,
    registry: &'a ToolRegistry,
    workspace: &std::path::Path,
    config: AgentConfig,
) -> Agent<'a> {
    let ctx = ToolContext::new(workspace.to_path_buf());
    Agent::new(provider, registry, ctx, config).with_permission(Arc::new(AllowAll))
}

// ─── Benchmark 1: Tool selection ───────────────────────────────────

#[tokio::test]
async fn bench_read_before_edit() {
    // The agent should call read_file before edit_file.
    // Scripted: turn 1 = read_file, turn 2 = edit_file, turn 3 = done.
    let provider = ScriptedProvider::new(vec![
        ScriptedProvider::tool_call_response(vec![("read_file", r#"{"path":"src/main.rs"}"#)]),
        ScriptedProvider::tool_call_response(vec![(
            "edit_file",
            r#"{"path":"src/main.rs","old_string":"hello","new_string":"world"}"#,
        )]),
        ScriptedProvider::text_response("Done — replaced hello with world."),
    ]);

    let dir = tempdir();
    let dir = std::fs::canonicalize(&dir).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/main.rs"),
        "fn main() { println!(\"hello\"); }\n",
    )
    .unwrap();

    let registry = ToolRegistry::with_builtins();
    let config = AgentConfig {
        model: "test".into(),
        max_turns: 5,
        ..AgentConfig::default()
    };

    let mut agent = make_agent(&provider, &registry, &dir, config);
    let result = agent
        .run("Replace hello with world in src/main.rs")
        .await
        .unwrap();

    // Verify the agent completed successfully
    assert!(!result.final_text.is_empty());
    assert!(
        result.turns >= 2,
        "should take at least 2 turns (read + edit)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ─── Benchmark 2: Error recovery ──────────────────────────────────

#[tokio::test]
async fn bench_error_recovery_edit_not_found() {
    // Agent tries edit_file with wrong old_string, gets error with hint,
    // then reads file and retries with correct text.
    let dir = tempdir();
    let dir = std::fs::canonicalize(&dir).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        "fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();

    let provider = ScriptedProvider::new(vec![
        // Turn 1: wrong edit (old_string not found) — error with hint
        ScriptedProvider::tool_call_response(vec![(
            "edit_file",
            r#"{"path":"src/lib.rs","old_string":"fn sum","new_string":"fn add_numbers"}"#,
        )]),
        // Turn 2: reads file to see correct content
        ScriptedProvider::tool_call_response(vec![("read_file", r#"{"path":"src/lib.rs"}"#)]),
        // Turn 3: correct edit with exact text
        ScriptedProvider::tool_call_response(vec![(
            "edit_file",
            r#"{"path":"src/lib.rs","old_string":"fn add(a: i32, b: i32) -> i32 { a + b }","new_string":"fn add_numbers(a: i32, b: i32) -> i32 { a + b }"}"#,
        )]),
        ScriptedProvider::text_response("Fixed — renamed fn add to fn add_numbers."),
    ]);

    let registry = ToolRegistry::with_builtins();
    let config = AgentConfig {
        model: "test".into(),
        max_turns: 6,
        ..AgentConfig::default()
    };

    let mut agent = make_agent(&provider, &registry, &dir, config);
    let result = agent
        .run("Rename fn add to fn add_numbers in src/lib.rs")
        .await
        .unwrap();

    assert!(!result.final_text.is_empty());
    // Verify the file was actually changed
    let content = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
    assert!(content.contains("fn add_numbers"), "file should be edited");

    let _ = std::fs::remove_dir_all(&dir);
}

// ─── Benchmark 3: Parallel tool calls ─────────────────────────────

#[tokio::test]
async fn bench_parallel_tool_calls() {
    // Agent issues multiple tool calls in one turn — they should
    // all execute and return results.
    let dir = tempdir();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(dir.join("src/b.rs"), "fn b() {}\n").unwrap();

    let provider = ScriptedProvider::new(vec![
        // Turn 1: read two files in parallel
        ScriptedProvider::tool_call_response(vec![
            ("read_file", r#"{"path":"src/a.rs"}"#),
            ("read_file", r#"{"path":"src/b.rs"}"#),
        ]),
        ScriptedProvider::text_response("Both files contain simple function definitions."),
    ]);

    let registry = ToolRegistry::with_builtins();
    let config = AgentConfig {
        model: "test".into(),
        max_turns: 3,
        ..AgentConfig::default()
    };

    let mut agent = make_agent(&provider, &registry, &dir, config);
    let result = agent.run("Read both src/a.rs and src/b.rs").await.unwrap();

    assert!(!result.final_text.is_empty());
    // Both tool results should be in the transcript
    let tool_results: Vec<_> = result
        .transcript
        .iter()
        .filter(|m| m.role == aegis_api::Role::Tool)
        .collect();
    assert_eq!(
        tool_results.len(),
        2,
        "should have 2 tool results from parallel calls"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ─── Benchmark 4: Cost efficiency ─────────────────────────────────

#[tokio::test]
async fn bench_cost_tracking() {
    let provider = ScriptedProvider::new(vec![ScriptedProvider::text_response(
        "Hello! How can I help?",
    )]);

    let dir = tempdir();
    let registry = ToolRegistry::with_builtins();
    let config = AgentConfig {
        model: "test".into(),
        max_turns: 3,
        ..AgentConfig::default()
    };

    let mut agent = make_agent(&provider, &registry, &dir, config);
    let result = agent.run("hi").await.unwrap();

    assert_eq!(result.usage.input_tokens, 100);
    assert_eq!(result.usage.output_tokens, 50);
    assert_eq!(result.turns, 1);

    let _ = std::fs::remove_dir_all(&dir);
}

// ─── Benchmark 5: Scope discipline (max_turns respected) ──────────

#[tokio::test]
async fn bench_max_turns_enforced() {
    // If the model keeps calling tools forever, max_turns should stop it.
    let provider = ScriptedProvider::new(vec![
        ScriptedProvider::tool_call_response(vec![("bash", r#"{"command":"echo 1"}"#)]),
        ScriptedProvider::tool_call_response(vec![("bash", r#"{"command":"echo 2"}"#)]),
        ScriptedProvider::tool_call_response(vec![("bash", r#"{"command":"echo 3"}"#)]),
        ScriptedProvider::tool_call_response(vec![("bash", r#"{"command":"echo 4"}"#)]),
    ]);

    let dir = tempdir();
    let registry = ToolRegistry::with_builtins();
    let config = AgentConfig {
        model: "test".into(),
        max_turns: 2,
        ..AgentConfig::default()
    };

    let mut agent = make_agent(&provider, &registry, &dir, config);
    let result = agent.run("keep running commands").await;

    assert!(result.is_err(), "should hit MaxTurns error");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Madde 6 — DeepSeek live tool-use bench
//
// Sends 5 representative prompts to DeepSeek with all 40 built-in tool
// schemas and asserts the model selects the correct tool.
//
// Run with:
//   DEEPSEEK_API_KEY=<key> cargo test -p metis-core --test benchmark deepseek_tool_bench -- --ignored --nocapture
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Madde 7 — Multi-provider tool-use comparison
//
// Same 5 prompts, 6 providers: DeepSeek, Gemini Flash, GLM, NIM,
// Kimi-K2 (via OpenRouter), Qwen 2.5 (via OpenRouter).
// Prints a comparison table and fails if fewer than 2 providers reach 4/5.
//
// Run with:
//   cargo test -p metis-core --test benchmark multi_provider_tool_bench -- --ignored --nocapture
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "live multi-provider API — run explicitly with --ignored"]
async fn multi_provider_tool_bench() {
    use aegis_api::{ChatMessage, ChatRequest, OpenAICompatClient};
    use aegis_core::ToolRegistry;

    struct ProviderCfg {
        label: &'static str,
        base_url: &'static str,
        env_var: &'static str,
        model: &'static str,
        timeout_secs: u64,
    }

    let providers: &[ProviderCfg] = &[
        ProviderCfg {
            label: "DeepSeek",
            base_url: "https://api.deepseek.com",
            env_var: "DEEPSEEK_API_KEY",
            model: "deepseek-chat",
            timeout_secs: 30,
        },
        ProviderCfg {
            label: "Gemini-Flash",
            base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
            env_var: "GEMINI_API_KEY",
            model: "gemini-2.5-flash",  // 2.0-flash had API errors
            timeout_secs: 30,
        },
        ProviderCfg {
            label: "GLM-5.1",
            base_url: "https://api.z.ai/api/paas/v4",
            env_var: "ZAI_API_KEY",
            model: "glm-5.1",
            timeout_secs: 30,
        },
        ProviderCfg {
            label: "NIM-DeepSeek",
            base_url: "https://integrate.api.nvidia.com",
            env_var: "NVIDIA_API_KEY",
            model: "deepseek-ai/deepseek-v4-flash",
            timeout_secs: 90,  // NIM cold-start often 30-60s
        },
        ProviderCfg {
            label: "Kimi-K2",
            base_url: "https://openrouter.ai/api",
            env_var: "OPENROUTER_API_KEY",
            model: "moonshotai/kimi-k2-0711-preview",  // preview model ID
            timeout_secs: 60,
        },
        ProviderCfg {
            label: "Qwen-2.5-72B",
            base_url: "https://openrouter.ai/api",
            env_var: "OPENROUTER_API_KEY",
            model: "qwen/qwen-2.5-72b-instruct",
            timeout_secs: 60,
        },
    ];

    let cases: &[BenchCase] = &[
        BenchCase {
            prompt: "Read the file /tmp/test.rs and show me its content",
            expected_tool: "read_file",
            acceptable_tools: &["read_file"],
        },
        BenchCase {
            prompt: "Search for all Rust files that contain the text 'ToolContext' in the crates directory",
            expected_tool: "grep",
            acceptable_tools: &["grep", "bash"],
        },
        BenchCase {
            prompt: "Run the shell command 'echo hello world' and show me the output",
            expected_tool: "bash",
            acceptable_tools: &["bash"],
        },
        BenchCase {
            prompt: "Find all .rs files in the src directory",
            expected_tool: "glob",
            acceptable_tools: &["glob", "bash"],
        },
        BenchCase {
            prompt: "Create a task called 'implement login' with description 'Build the auth flow'",
            expected_tool: "create_task",
            acceptable_tools: &["create_task"],
        },
    ];

    let registry = ToolRegistry::with_builtins();
    let tool_specs = registry.specs();

    let mut table: Vec<(String, usize, Vec<String>)> = Vec::new();
    let mut providers_at_4 = 0usize;

    for cfg in providers {
        let key = match std::env::var(cfg.env_var) {
            Ok(k) => k,
            Err(_) => {
                eprintln!("SKIP {}: {} not set", cfg.label, cfg.env_var);
                table.push((cfg.label.to_string(), 0, vec!["SKIP (no key)".into()]));
                continue;
            }
        };

        let client = match OpenAICompatClient::new(cfg.base_url, &key) {
            Ok(c) => c,
            Err(e) => {
                table.push((cfg.label.to_string(), 0, vec![format!("SKIP (client err: {e})")]));
                continue;
            }
        };

        let mut passed = 0usize;
        let mut row_results: Vec<String> = Vec::new();

        for (i, case) in cases.iter().enumerate() {
            let request = ChatRequest {
                model: cfg.model.to_string(),
                messages: vec![
                    ChatMessage::system(
                        "You are a helpful coding assistant. When given a task, call the appropriate tool."
                    ),
                    ChatMessage::user(case.prompt),
                ],
                tools: Some(tool_specs.clone()),
                temperature: Some(0.0),
                max_tokens: Some(512),
                thinking: false,
                thinking_budget: 0,
            };

            let call = tokio::time::timeout(
                std::time::Duration::from_secs(cfg.timeout_secs),
                client.chat(&request),
            );
            let cell = match call.await {
                Err(_) => format!("[{i}] TIMEOUT"),
                Ok(Err(e)) => format!("[{i}] ERR:{}", &e.to_string()[..e.to_string().len().min(30)]),
                Ok(Ok(resp)) => {
                    let choice = match resp.choices.first() {
                        Some(c) => c,
                        None => { row_results.push(format!("[{i}] EMPTY")); continue; }
                    };
                    let called: Vec<&str> = choice.message.tool_calls.iter()
                        .map(|tc| tc.function.name.as_str()).collect();
                    let ok = called.iter().any(|t| case.acceptable_tools.contains(t));
                    if ok {
                        passed += 1;
                        format!("[{i}]OK:{}", called.first().copied().unwrap_or("?"))
                    } else if called.is_empty() {
                        format!("[{i}]NO_TOOL")
                    } else {
                        format!("[{i}]WRONG:{}", called.join(","))
                    }
                }
            };
            row_results.push(cell);
        }

        if passed >= 4 {
            providers_at_4 += 1;
        }
        table.push((cfg.label.to_string(), passed, row_results));
    }

    // Build table string (also write to /tmp/metis-bench.txt for review)
    let mut out = String::new();
    out.push_str(&format!("\nMulti-Provider Tool-Use Bench ({} prompts x {} providers)\n", cases.len(), providers.len()));
    out.push_str(&format!("{:<14} | Score | Detail\n", "Provider"));
    out.push_str(&"-".repeat(70));
    out.push('\n');
    for (label, score, detail) in &table {
        let bar = "#".repeat(*score) + &".".repeat(cases.len().saturating_sub(*score));
        let detail_str = detail.join("  ");
        out.push_str(&format!("{:<14} | {}/{} [{}] | {}\n",
            label, score, cases.len(), bar, &detail_str[..detail_str.len().min(50)]));
    }
    out.push_str(&format!("\nProviders hitting 4+/5: {providers_at_4}\n"));

    eprintln!("{out}");
    let _ = std::fs::write("/tmp/metis-bench.txt", &out);

    assert!(
        providers_at_4 >= 2,
        "Only {providers_at_4} provider(s) scored 4+/5 — \
         check API keys and model availability"
    );
}

/// One bench case: a user prompt and the tool name we expect.
struct BenchCase {
    prompt: &'static str,
    expected_tool: &'static str,
    /// Loose check: if the model calls ANY tool from this list it passes.
    /// Use when the task is ambiguous between close alternatives.
    acceptable_tools: &'static [&'static str],
}

#[tokio::test]
#[ignore = "live DeepSeek API — run explicitly with --ignored"]
async fn deepseek_tool_bench() {
    use aegis_api::{ChatMessage, ChatRequest, OpenAICompatClient};
    use aegis_core::ToolRegistry;

    let api_key = match std::env::var("DEEPSEEK_API_KEY") {
        Ok(k) => k,
        Err(_) => {
            eprintln!("SKIP: DEEPSEEK_API_KEY not set");
            return;
        }
    };

    let cases: &[BenchCase] = &[
        BenchCase {
            prompt: "Read the file /Users/macmini/Projects/metis/Cargo.toml and show me its content",
            expected_tool: "read_file",
            acceptable_tools: &["read_file"],
        },
        BenchCase {
            prompt: "Search for all Rust files that contain the text 'ToolContext' in the crates directory",
            expected_tool: "grep",
            acceptable_tools: &["grep", "bash"],
        },
        BenchCase {
            prompt: "Run the shell command 'echo hello world' and show me the output",
            expected_tool: "bash",
            acceptable_tools: &["bash"],
        },
        BenchCase {
            prompt: "Find all .rs files in the crates/core/src/tools directory",
            expected_tool: "glob",
            acceptable_tools: &["glob", "bash"],
        },
        BenchCase {
            prompt: "Create a task called 'implement feature X' with description 'Build the new authentication flow'",
            expected_tool: "create_task",
            acceptable_tools: &["create_task"],
        },
    ];

    // Build all tool specs from the registry.
    let registry = ToolRegistry::with_builtins();
    let tool_specs = registry.specs();

    let client = OpenAICompatClient::new("https://api.deepseek.com", &api_key).unwrap();

    let mut passed = 0usize;
    let mut results: Vec<String> = Vec::new();

    for (i, case) in cases.iter().enumerate() {
        let request = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![
                ChatMessage::system(
                    "You are a helpful coding assistant. When given a task, call the appropriate tool."
                ),
                ChatMessage::user(case.prompt),
            ],
            tools: Some(tool_specs.clone()),
            temperature: Some(0.0),
            max_tokens: Some(512),
            thinking: false,
            thinking_budget: 0,
        };

        let result_str = match client.chat(&request).await {
            Err(e) => {
                let s = format!("  [{i}] FAIL (api error: {e}) — {}", case.prompt);
                results.push(s);
                continue;
            }
            Ok(resp) => {
                let choice = match resp.choices.first() {
                    Some(c) => c,
                    None => {
                        let s = format!("  [{i}] FAIL (empty response) — {}", case.prompt);
                        results.push(s);
                        continue;
                    }
                };

                let called_tools: Vec<&str> = choice
                    .message
                    .tool_calls
                    .iter()
                    .map(|tc| tc.function.name.as_str())
                    .collect();

                let ok = called_tools
                    .iter()
                    .any(|t| case.acceptable_tools.contains(t));

                if ok {
                    passed += 1;
                    format!(
                        "  [{i}] PASS called={} — {}",
                        called_tools.join(","),
                        case.expected_tool
                    )
                } else if called_tools.is_empty() {
                    let text = choice.message.content.as_deref().unwrap_or("(no text)");
                    format!(
                        "  [{i}] FAIL no tool call (text: {}) — expected {}",
                        &text[..text.len().min(80)],
                        case.expected_tool
                    )
                } else {
                    format!(
                        "  [{i}] FAIL called={} — expected {}",
                        called_tools.join(","),
                        case.expected_tool
                    )
                }
            }
        };
        results.push(result_str);
    }

    println!("\n=== DeepSeek tool-use bench: {passed}/{} passed ===", cases.len());
    for r in &results {
        println!("{r}");
    }
    println!();

    // Soft assertion: at least 4/5 must pass for the model to be viable.
    // If this fails, Madde 3 (Claude subprocess) is mandatory.
    assert!(
        passed >= 4,
        "DeepSeek tool-use bench: only {passed}/{} passed — \
         consider routing complex tool calls to Claude subprocess (Madde 3)",
        cases.len()
    );
}
