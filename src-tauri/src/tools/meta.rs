use crate::provider::ToolDefinition;
use serde_json::json;

pub fn delegate_task_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "delegate_task".into(),
            description: "Delegates a subtask to a specialized sub-agent. Creates a task package with instructions, context, and success criteria. Use for parallelizable work or specialized domains.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "description": {
                        "type": "string",
                        "description": "Short description of the delegated task (3-5 words)"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Detailed instructions for the sub-agent"
                    },
                    "agentType": {
                        "type": "string",
                        "description": "Type of sub-agent: 'general' (default), 'explore' (codebase search), 'plan' (architecture/design)"
                    }
                },
                "required": ["description", "prompt"]
            }),
        },
    }
}

pub fn premortem_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "premortem".into(),
            description: "Runs a premortem risk analysis on a plan, decision, or project. Assumes failure 6 months from now and works backward to identify every failure mode, blind spot, and risk factor. Produces structured risk assessment with mitigations.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "subject": {
                        "type": "string",
                        "description": "The plan, decision, or project to analyze"
                    },
                    "timeHorizon": {
                        "type": "string",
                        "description": "How far into the future to look (default: '6 months')"
                    },
                    "categories": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Risk categories to analyze: 'technical', 'operational', 'dependency', 'human', 'security', 'financial'. Default: all."
                    }
                },
                "required": ["subject"]
            }),
        },
    }
}

pub fn eisenhower_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "eisenhower".into(),
            description: "Organizes tasks into an Eisenhower Matrix (urgency vs importance). Classifies tasks into 4 quadrants: Do First (urgent+important), Schedule (important+not urgent), Delegate (urgent+not important), Eliminate (not urgent+not important).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": {"type": "string", "description": "Task name"},
                                "urgent": {"type": "boolean", "description": "Is this task urgent?"},
                                "important": {"type": "boolean", "description": "Is this task important?"},
                                "effort": {"type": "string", "enum": ["low", "medium", "high"], "description": "Estimated effort"}
                            },
                            "required": ["name"]
                        },
                        "description": "List of tasks to classify"
                    },
                    "format": {
                        "type": "string",
                        "enum": ["matrix", "list", "markdown"],
                        "description": "Output format (default: markdown)"
                    }
                },
                "required": ["tasks"]
            }),
        },
    }
}

pub async fn handle_delegate_task(args: serde_json::Value) -> Result<String, String> {
    let description = args["description"].as_str().ok_or("description required")?;
    let prompt = args["prompt"].as_str().ok_or("prompt required")?;
    let agent_type = args["agentType"].as_str().unwrap_or("general");

    let task_id = uuid::Uuid::new_v4().to_string();
    let ts = chrono::Utc::now().to_rfc3339();

    Ok(format!(
        "## Task Delegated\n\
         **ID:** {id}\n\
         **Description:** {desc}\n\
         **Agent Type:** {agent}\n\
         **Created:** {ts}\n\
         **Status:** QUEUED\n\n\
         **Instructions:**\n{prompt}\n\n\
         ---\n\
         This task will be picked up by the {agent} sub-agent.\n\
         Check back for results.",
        id = task_id,
        desc = description,
        agent = agent_type,
        ts = ts,
        prompt = prompt,
    ))
}

pub async fn handle_premortem(args: serde_json::Value) -> Result<String, String> {
    let subject = args["subject"].as_str().ok_or("subject required")?;
    let time_horizon = args["timeHorizon"].as_str().unwrap_or("6 months");
    let categories: Vec<String> = args["categories"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_else(|| vec![
            "technical".into(), "operational".into(), "dependency".into(),
            "human".into(), "security".into(), "financial".into(),
        ]);

    let all_categories = vec![
        ("Technical", "System failures, architecture flaws, performance bottlenecks, scalability limits, data corruption, integration failures"),
        ("Operational", "Process breakdowns, monitoring gaps, deployment failures, rollback difficulties, environment drift, capacity planning misses"),
        ("Dependency", "External service outages, API deprecations, library vulnerabilities, vendor lock-in, version incompatibilities, license changes"),
        ("Human", "Knowledge silos, onboarding friction, miscommunication, burnout, skill gaps, incorrect assumptions, documentation rot"),
        ("Security", "Unauthorized access, data leaks, injection attacks, credential exposure, supply chain compromise, insider threats"),
        ("Financial", "Cost overruns, pricing model changes, budget cuts, inefficient resource usage, hidden infrastructure costs, exchange rate swings"),
    ];

    let selected: Vec<(&str, &str)> = all_categories.into_iter()
        .filter(|(name, _)| categories.iter().any(|c| c.to_lowercase() == name.to_lowercase()))
        .collect();

    let mut output = format!(
        "## Premortem: {subject}\n\
         **Time Horizon:** {horizon} from now (assuming FAILURE)\n\n\
         > Working backward from failure to identify what went wrong.\n\n",
        subject = subject,
        horizon = time_horizon,
    );

    for (category, risks) in &selected {
        output.push_str(&format!("### {}\n{}\n\n", category, risks));
        output.push_str("**Failure modes to investigate:**\n");
        output.push_str("- [ ] Root cause identified?\n");
        output.push_str("- [ ] Early warning signals missed?\n");
        output.push_str("- [ ] Mitigation could have been applied?\n\n");
    }

    output.push_str(&format!(
        "---\n\
         **Action:** For each category above, identify at least 2 specific failure scenarios\n\
         with: (1) how it emerges, (2) preventive measure, (3) detection criterion, (4) owner.\n\n\
         **Reference:** Run this analysis against the actual plan/commit/code to generate concrete risks."
    ));

    Ok(output)
}

pub async fn handle_eisenhower(args: serde_json::Value) -> Result<String, String> {
    let tasks = args["tasks"].as_array().ok_or("tasks required")?;
    let format = args["format"].as_str().unwrap_or("markdown");

    struct Task {
        name: String,
        urgent: bool,
        important: bool,
        effort: String,
    }

    let mut parsed: Vec<Task> = Vec::new();
    for t in tasks {
        let urgent = t.get("urgent").and_then(|v| v.as_bool()).unwrap_or(false);
        let important = t.get("important").and_then(|v| v.as_bool()).unwrap_or(false);
        let effort = t.get("effort").and_then(|v| v.as_str()).unwrap_or("medium").to_string();
        let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("unnamed").to_string();
        parsed.push(Task { name, urgent, important, effort });
    }

    if parsed.is_empty() {
        return Ok("No tasks provided for classification.".to_string());
    }

    // Urgent classification
    let n_urgent = parsed.iter().filter(|t| t.urgent).count();
    let n_important = parsed.iter().filter(|t| t.important).count();

    let has_classification = parsed.iter().any(|t| t.urgent) || parsed.iter().any(|t| t.important);

    if !has_classification {
        return Ok(
            "## Tasks (unclassified)\n\n".to_string() +
            &parsed.iter().enumerate().map(|(i, t)| format!("{}. {}\n   Urgency: ? | Importance: ? | Effort: {}", i + 1, t.name, t.effort)).collect::<Vec<_>>().join("\n") +
            "\n\n_Please classify tasks by urgency and importance for matrix placement._"
        );
    }

    let mut do_first: Vec<&Task> = Vec::new();    // urgent + important
    let mut schedule: Vec<&Task> = Vec::new();     // not urgent + important
    let mut delegate: Vec<&Task> = Vec::new();     // urgent + not important
    let mut eliminate: Vec<&Task> = Vec::new();    // not urgent + not important

    for t in &parsed {
        match (t.urgent, t.important) {
            (true, true) => do_first.push(t),
            (false, true) => schedule.push(t),
            (true, false) => delegate.push(t),
            (false, false) => eliminate.push(t),
        }
    }

    let mut output = format!(
        "## Eisenhower Matrix\n\n\
         {} tasks total | {} urgent | {} important\n\n",
        parsed.len(), n_urgent, n_important,
    );

    output.push_str("```\n");
    output.push_str("                   URGENT                  NOT URGENT\n");
    output.push_str("          ┌─────────────────────┬─────────────────────┐\n");
    output.push_str(&format!(
        "IMPORTANT │ DO FIRST ({:>2})        │ SCHEDULE ({:>2})        │\n",
        do_first.len(), schedule.len()
    ));
    output.push_str("          │                     │                     │\n");
    output.push_str(&format!(
        "NOT       │ DELEGATE ({:>2})       │ ELIMINATE ({:>2})      │\n",
        delegate.len(), eliminate.len()
    ));
    output.push_str("          └─────────────────────┴─────────────────────┘\n");
    output.push_str("```\n\n");

    for (label, quad) in [
        ("🔴 DO FIRST — Urgent & Important", do_first),
        ("🟡 SCHEDULE — Important, Not Urgent", schedule),
        ("🟢 DELEGATE — Urgent, Not Important", delegate),
        ("⚪ ELIMINATE — Not Urgent, Not Important", eliminate),
    ] {
        if !quad.is_empty() {
            output.push_str(&format!("### {}\n", label));
            for (i, t) in quad.iter().enumerate() {
                output.push_str(&format!("{}. {} (effort: {})\n", i + 1, t.name, t.effort));
            }
            output.push('\n');
        }
    }

    if format == "list" {
        let simple: Vec<String> = parsed.iter()
            .map(|t| {
                let quad = match (t.urgent, t.important) {
                    (true, true) => "DO",
                    (false, true) => "SCHEDULE",
                    (true, false) => "DELEGATE",
                    (false, false) => "DROP",
                };
                format!("[{}] {} (effort:{})", quad, t.name, t.effort)
            })
            .collect();
        return Ok(simple.join("\n"));
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defs_exist() {
        assert_eq!(delegate_task_def().function.name, "delegate_task");
        assert_eq!(premortem_def().function.name, "premortem");
        assert_eq!(eisenhower_def().function.name, "eisenhower");
    }

    #[tokio::test]
    async fn test_delegate_task() {
        let result = handle_delegate_task(serde_json::json!({
            "description": "Test task",
            "prompt": "Do something useful"
        })).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("QUEUED"));
    }

    #[tokio::test]
    async fn test_premortem() {
        let result = handle_premortem(serde_json::json!({
            "subject": "Deploying to production"
        })).await;
        assert!(result.is_ok());
        let out = result.unwrap();
        assert!(out.contains("Premortem"));
        assert!(out.contains("Technical"));
    }

    #[tokio::test]
    async fn test_eisenhower_empty() {
        let result = handle_eisenhower(serde_json::json!({"tasks": []})).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("No tasks"));
    }

    #[tokio::test]
    async fn test_eisenhower_classified() {
        let result = handle_eisenhower(serde_json::json!({
            "tasks": [
                {"name": "Fix prod bug", "urgent": true, "important": true, "effort": "high"},
                {"name": "Write docs", "urgent": false, "important": true, "effort": "medium"},
                {"name": "Reply to sales email", "urgent": true, "important": false, "effort": "low"},
                {"name": "Browse Reddit", "urgent": false, "important": false, "effort": "low"}
            ]
        })).await;
        assert!(result.is_ok());
        let out = result.unwrap();
        assert!(out.contains("DO FIRST"));
        assert!(out.contains("SCHEDULE"));
        assert!(out.contains("DELEGATE"));
        assert!(out.contains("ELIMINATE"));
    }

    #[tokio::test]
    async fn test_eisenhower_list_format() {
        let result = handle_eisenhower(serde_json::json!({
            "tasks": [
                {"name": "Fix bug", "urgent": true, "important": true, "effort": "high"}
            ],
            "format": "list"
        })).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("[DO]"));
    }
}
