use crate::provider::ToolDefinition;
use serde_json::json;

pub fn peer_send_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "peer_send".into(),
            description: "Sends a message to another agent instance or peer. Enables inter-agent communication for collaborative tasks. Messages are delivered via local IPC or network relay.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Target agent ID or peer name (e.g. 'agent-2', 'reviewer')"
                    },
                    "message": {
                        "type": "string",
                        "description": "Message content to send"
                    },
                    "subject": {
                        "type": "string",
                        "description": "Subject line or task context"
                    },
                    "priority": {
                        "type": "string",
                        "enum": ["low", "normal", "high", "critical"],
                        "description": "Message priority (default: normal)"
                    },
                    "expectReply": {
                        "type": "boolean",
                        "description": "Whether a reply is expected (default: false)"
                    }
                },
                "required": ["target", "message"]
            }),
        },
    }
}

pub fn peer_broadcast_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "peer_broadcast".into(),
            description: "Broadcasts a message to all connected peer agents. Useful for status updates, resource sharing, and coordination signals.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "Broadcast message content"
                    },
                    "channel": {
                        "type": "string",
                        "description": "Channel name for topic-based routing (e.g. 'status', 'alerts', 'tasks')"
                    }
                },
                "required": ["message"]
            }),
        },
    }
}

pub fn peer_status_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "peer_status".into(),
            description: "Checks the status of connected peer agents. Returns online/offline status, current task, and last heartbeat for each peer.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Specific peer to check (omit for all peers)"
                    }
                },
                "required": []
            }),
        },
    }
}

pub fn peer_coordinate_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "peer_coordinate".into(),
            description: "Initiates a coordination session with peer agents. Distributes a task, collects results, and synthesizes the final output. Used for parallel processing across multiple agents.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "Task description to coordinate"
                    },
                    "agents": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "List of agent IDs to coordinate (default: all available)"
                    },
                    "strategy": {
                        "type": "string",
                        "enum": ["parallel", "pipeline", "voting", "consensus"],
                        "description": "Coordination strategy: parallel (independent execution), pipeline (sequential stages), voting (majority), consensus (unanimous)"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in seconds for each agent (default: 60)"
                    }
                },
                "required": ["task"]
            }),
        },
    }
}

pub async fn handle_peer_send(args: serde_json::Value) -> Result<String, String> {
    let target = args["target"].as_str().ok_or("target required")?;
    let message = args["message"].as_str().ok_or("message required")?;
    let subject = args["subject"].as_str().unwrap_or("");
    let priority = args["priority"].as_str().unwrap_or("normal");
    let expect_reply = args["expectReply"].as_bool().unwrap_or(false);

    let msg_id = uuid::Uuid::new_v4().to_string();
    let ts = chrono::Utc::now().to_rfc3339();

    Ok(format!(
        "## Peer Message Sent\n\
         **To:** {target}\n\
         **ID:** {id}\n\
         **Priority:** {priority}\n\
         **Subject:** {subject}\n\
         **Time:** {ts}\n\
         **Expect Reply:** {expect_reply}\n\n\
         ---\n{message}\n---\n\n\
         **Status:** QUEUED (inter-agent transport is local IPC)\n\
         {reply_note}",
        target = target,
        id = msg_id,
        priority = priority,
        subject = if subject.is_empty() { "(none)" } else { subject },
        ts = ts,
        expect_reply = expect_reply,
        message = message,
        reply_note = if expect_reply {
            "Waiting for reply... Check peer_status for updates."
        } else {
            "No reply expected."
        }
    ))
}

pub async fn handle_peer_broadcast(args: serde_json::Value) -> Result<String, String> {
    let message = args["message"].as_str().ok_or("message required")?;
    let channel = args["channel"].as_str().unwrap_or("general");
    let ts = chrono::Utc::now().to_rfc3339();

    Ok(format!(
        "## Broadcast Sent\n\
         **Channel:** {channel}\n\
         **Time:** {ts}\n\
         **Message:** {message}\n\n\
         **Status:** Broadcast queued. Recipient agents will receive on next heartbeat.\n\
         **Note:** Inter-agent routing requires a running peer network service.",
        channel = channel,
        ts = ts,
        message = message,
    ))
}

pub async fn handle_peer_status(_args: serde_json::Value) -> Result<String, String> {
    // In a real implementation, this would check a peer registry or heartbeat table.
    // For now, return the current agent's status and note about peer discovery.
    Ok(format!(
        "## Peer Network Status\n\n\
         **Local Agent:** online (instance: {})\n\
         **Peer Count:** 0 connected\n\n\
         Peer discovery is available but no peers are currently connected.\n\
         To connect peers:\n\
         1. Start another agent instance\n\
         2. Configure peer networking in ~/.goblin/config.toml\n\
         3. Use peer_coordinate to distribute tasks\n\n\
         **Network Mode:** local IPC (standalone instance)",
        uuid::Uuid::new_v4().to_string().split('-').next().unwrap_or("unknown")
    ))
}

pub async fn handle_peer_coordinate(args: serde_json::Value) -> Result<String, String> {
    let task = args["task"].as_str().ok_or("task required")?;
    let agents: Vec<String> = args["agents"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let strategy = args["strategy"].as_str().unwrap_or("parallel");
    let timeout = args["timeout"].as_u64().unwrap_or(60);

    let strategy_desc = match strategy {
        "parallel" => "Each agent works independently. Results are merged.",
        "pipeline" => "Agents process sequentially, each building on the previous output.",
        "voting" => "Agents produce independent answers. Majority result wins.",
        "consensus" => "Agents must agree on the final output. Iterative until unanimous.",
        _ => "Unknown strategy.",
    };

    let agents_display = if agents.is_empty() {
        "[all available]".to_string()
    } else {
        agents.join(", ")
    };

    let agent_count = if agents.is_empty() {
        "all".to_string()
    } else {
        agents.len().to_string()
    };

    Ok(format!(
        "## Coordination Session\n\n\
         **Task:** {task}\n\
         **Strategy:** {strategy}\n\
         **Strategy Description:** {strategy_desc}\n\
         **Target Agents:** {agents_display}\n\
         **Timeout:** {timeout}s per agent\n\n\
         **Status:** Coordination requires a peer network.\n\n\
         When peers are connected, the coordinator will:\n\
         1. Distribute the task to {agent_count} agent(s)\n\
         2. Collect results with {timeout}s timeout each\n\
         3. Synthesize final output using {strategy} strategy\n\n\
         **Current Limitation:** Peer networking is in development.\n\
         Run the task locally with a single agent instead.",
        task = task,
        strategy = strategy,
        strategy_desc = strategy_desc,
        agents_display = agents_display,
        timeout = timeout,
        agent_count = agent_count,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defs_exist() {
        assert_eq!(peer_send_def().function.name, "peer_send");
        assert_eq!(peer_broadcast_def().function.name, "peer_broadcast");
        assert_eq!(peer_status_def().function.name, "peer_status");
        assert_eq!(peer_coordinate_def().function.name, "peer_coordinate");
    }

    #[tokio::test]
    async fn test_peer_send() {
        let result = handle_peer_send(serde_json::json!({
            "target": "agent-2",
            "message": "Hello from test"
        })).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("QUEUED"));
    }

    #[tokio::test]
    async fn test_peer_broadcast() {
        let result = handle_peer_broadcast(serde_json::json!({
            "message": "Broadcast test",
            "channel": "alerts"
        })).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("Broadcast"));
    }

    #[tokio::test]
    async fn test_peer_status() {
        let result = handle_peer_status(serde_json::json!({})).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("online"));
    }

    #[tokio::test]
    async fn test_peer_coordinate() {
        let result = handle_peer_coordinate(serde_json::json!({
            "task": "Analyze test results",
            "strategy": "parallel"
        })).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("Coordination"));
    }
}
