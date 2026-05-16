//! Autonomous mode security layer with kill switches and guardrails.
//!
//! This module provides security controls for Aegis's autonomous operation.
//! It wraps the existing permission system with additional safety checks,
//! resource limits, and emergency kill switches.

use crate::permission::{Permission, PermissionDecision};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Autonomous security configuration.
#[derive(Debug, Clone)]
pub struct AutonomousSecurityConfig {
    /// Maximum number of tool calls in autonomous mode
    pub max_tool_calls: u32,
    /// Maximum token usage (estimated)
    pub max_token_usage: u64,
    /// Maximum API cost in USD
    pub max_cost_usd: f64,
    /// Timeout for autonomous operation
    pub timeout: Duration,
    /// Require human approval for these tool types
    pub require_approval_for: Vec<String>,
    /// Paths that cannot be modified autonomously
    pub protected_paths: Vec<PathBuf>,
    /// Maximum number of files that can be deleted
    pub max_deletions: u32,
    /// Maximum number of git commits
    pub max_commits: u32,
    /// Enable kill switch monitoring
    pub enable_kill_switch: bool,
}

impl Default for AutonomousSecurityConfig {
    fn default() -> Self {
        Self {
            max_tool_calls: 100,
            max_token_usage: 100_000,
            max_cost_usd: 10.0,
            timeout: Duration::from_secs(600), // 10 minutes
            require_approval_for: vec![
                "bash".to_string(),
                "edit_file".to_string(),
                "write_file".to_string(),
                "multi_edit".to_string(),
            ],
            protected_paths: vec![
                PathBuf::from(".git"),
                PathBuf::from("Cargo.toml"),
                PathBuf::from("package.json"),
                PathBuf::from("node_modules"),
            ],
            max_deletions: 3,
            max_commits: 5,
            enable_kill_switch: true,
        }
    }
}

/// Autonomous operation statistics.
#[derive(Debug)]
struct AutonomousStats {
    tool_call_count: u32,
    estimated_tokens: u64,
    estimated_cost_usd: f64,
    deletions: u32,
    commits: u32,
    start_time: Instant,
    #[allow(dead_code)]
    last_approval_time: Option<Instant>,
}

impl Default for AutonomousStats {
    fn default() -> Self {
        Self {
            tool_call_count: 0,
            estimated_tokens: 0,
            estimated_cost_usd: 0.0,
            deletions: 0,
            commits: 0,
            start_time: Instant::now(),
            last_approval_time: None,
        }
    }
}

/// Snapshot of live security-layer counters. Returned by
/// `AutonomousSecurityLayer::stats_snapshot()` so the UI can display
/// real numbers instead of hardcoded literals.
#[derive(Debug, Clone)]
pub struct SecurityStatsSnapshot {
    pub tool_call_count: u32,
    pub estimated_tokens: u64,
    pub estimated_cost_usd: f64,
    pub deletions: u32,
    pub commits: u32,
    pub elapsed: Duration,
}

/// Kill switch state.
#[derive(Debug, Clone)]
pub enum KillSwitchState {
    /// Autonomous operation is allowed
    Enabled,
    /// Kill switch has been triggered - stop all autonomous actions
    Triggered(String), // Reason
    /// Paused - waiting for human approval to continue
    Paused(String), // Reason
}

/// Autonomous security layer.
pub struct AutonomousSecurityLayer {
    inner: Arc<dyn Permission>,
    config: AutonomousSecurityConfig,
    stats: Mutex<AutonomousStats>,
    kill_switch: Arc<Mutex<KillSwitchState>>,
    approval_history: Mutex<HashMap<String, Instant>>,
}

impl std::fmt::Debug for AutonomousSecurityLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutonomousSecurityLayer")
            .field("config", &self.config)
            .field("stats", &self.stats)
            .field("kill_switch", &self.kill_switch)
            .finish_non_exhaustive()
    }
}

impl AutonomousSecurityLayer {
    pub fn new(inner: Arc<dyn Permission>, config: AutonomousSecurityConfig) -> Self {
        Self::with_kill_switch(
            inner,
            config,
            Arc::new(Mutex::new(KillSwitchState::Enabled)),
        )
    }

    fn with_kill_switch(
        inner: Arc<dyn Permission>,
        config: AutonomousSecurityConfig,
        kill_switch: Arc<Mutex<KillSwitchState>>,
    ) -> Self {
        Self {
            inner,
            config,
            stats: Mutex::new(AutonomousStats {
                start_time: Instant::now(),
                ..Default::default()
            }),
            kill_switch,
            approval_history: Mutex::new(HashMap::new()),
        }
    }

    /// Trigger the kill switch with a reason.
    pub fn trigger_kill_switch(&self, reason: String) {
        let mut state = self.kill_switch.lock().unwrap();
        *state = KillSwitchState::Triggered(reason);
    }

    /// Pause autonomous operation.
    pub fn pause(&self, reason: String) {
        let mut state = self.kill_switch.lock().unwrap();
        *state = KillSwitchState::Paused(reason);
    }

    /// Resume autonomous operation.
    pub fn resume(&self) {
        let mut state = self.kill_switch.lock().unwrap();
        *state = KillSwitchState::Enabled;
    }

    /// Check if kill switch is triggered.
    pub fn is_kill_switch_triggered(&self) -> bool {
        matches!(
            *self.kill_switch.lock().unwrap(),
            KillSwitchState::Triggered(_)
        )
    }

    /// Get kill switch state.
    pub fn kill_switch_state(&self) -> KillSwitchState {
        self.kill_switch.lock().unwrap().clone()
    }

    /// Snapshot of live stats for display by the UI. Cheap clone — no
    /// internal state held across the borrow.
    pub fn stats_snapshot(&self) -> SecurityStatsSnapshot {
        let stats = self.stats.lock().unwrap();
        SecurityStatsSnapshot {
            tool_call_count: stats.tool_call_count,
            estimated_tokens: stats.estimated_tokens,
            estimated_cost_usd: stats.estimated_cost_usd,
            deletions: stats.deletions,
            commits: stats.commits,
            elapsed: stats.start_time.elapsed(),
        }
    }

    /// Read-only view of the active config (so the UI can show the
    /// real configured limits, not hardcoded literals).
    pub fn config(&self) -> &AutonomousSecurityConfig {
        &self.config
    }

    /// Update statistics after a tool call.
    fn update_stats(&self, tool: &str, args: &Value) {
        let mut stats = self.stats.lock().unwrap();
        stats.tool_call_count += 1;

        // Estimate token usage based on tool type
        let token_estimate = match tool {
            "edit_file" | "write_file" => {
                let content = args.get("content").or_else(|| args.get("new_string"));
                content
                    .map(|v| v.as_str().map(|s| s.len() as u64 / 4).unwrap_or(0))
                    .unwrap_or(100)
            }
            "bash" => 500,        // Command execution
            "web_search" => 1000, // API call
            _ => 50,              // Other tools
        };
        stats.estimated_tokens += token_estimate;

        // Estimate cost (very rough)
        stats.estimated_cost_usd += (token_estimate as f64) * 0.000002; // $0.002 per 1K tokens

        // Track deletions
        if tool == "bash" {
            if let Some(cmd) = args.get("command").and_then(Value::as_str) {
                if cmd.contains("rm ") || cmd.contains("git rm") {
                    stats.deletions += 1;
                }
            }
        }

        // Track commits
        if tool == "bash" {
            if let Some(cmd) = args.get("command").and_then(Value::as_str) {
                if cmd.contains("git commit") {
                    stats.commits += 1;
                }
            }
        }
    }

    /// Check if tool requires approval.
    fn requires_approval(&self, tool: &str, args: &Value) -> bool {
        // Check if tool is in require_approval_for list
        if self.config.require_approval_for.contains(&tool.to_string()) {
            return true;
        }

        // Check for protected paths
        if let Some(path_str) = args.get("path").and_then(Value::as_str) {
            let path = PathBuf::from(path_str);
            for protected in &self.config.protected_paths {
                if path.starts_with(protected) {
                    return true;
                }
            }
        }

        // Check for dangerous bash commands
        if tool == "bash" {
            if let Some(cmd) = args.get("command").and_then(Value::as_str) {
                let dangerous_patterns = [
                    "rm -rf",
                    "rm -r",
                    "rm -f",
                    "dd if=",
                    "mkfs",
                    "format",
                    "chmod 777",
                    "chmod 000",
                    "> /dev/sda",
                    "dd of=",
                    ":(){ :|:& };:", // Fork bomb
                    "curl | bash",
                    "wget | bash",
                ];
                if dangerous_patterns.iter().any(|p| cmd.contains(p)) {
                    return true;
                }
            }
        }

        false
    }

    /// Check resource limits.
    fn check_limits(&self) -> Option<String> {
        let stats = self.stats.lock().unwrap();
        let elapsed = stats.start_time.elapsed();

        // Check timeout
        if elapsed > self.config.timeout {
            return Some(format!(
                "Autonomous timeout exceeded: {:?}",
                self.config.timeout
            ));
        }

        // Check tool call limit
        if stats.tool_call_count >= self.config.max_tool_calls {
            return Some(format!(
                "Max tool calls exceeded: {}",
                self.config.max_tool_calls
            ));
        }

        // Check token limit
        if stats.estimated_tokens >= self.config.max_token_usage {
            return Some(format!(
                "Max token usage exceeded: {}",
                self.config.max_token_usage
            ));
        }

        // Check cost limit
        if stats.estimated_cost_usd >= self.config.max_cost_usd {
            return Some(format!(
                "Max cost exceeded: ${:.2}",
                self.config.max_cost_usd
            ));
        }

        // Check deletion limit
        if stats.deletions >= self.config.max_deletions {
            return Some(format!(
                "Max deletions exceeded: {}",
                self.config.max_deletions
            ));
        }

        // Check commit limit
        if stats.commits >= self.config.max_commits {
            return Some(format!("Max commits exceeded: {}", self.config.max_commits));
        }

        None
    }
}

impl Permission for AutonomousSecurityLayer {
    fn check(&self, tool: &str, args: &Value) -> PermissionDecision {
        // Check kill switch first
        if self.config.enable_kill_switch {
            let state = self.kill_switch.lock().unwrap();
            match &*state {
                KillSwitchState::Triggered(reason) => {
                    return PermissionDecision::HardDeny(format!(
                        "Kill switch triggered: {}",
                        reason
                    ));
                }
                KillSwitchState::Paused(reason) => {
                    return PermissionDecision::Deny(format!(
                        "Autonomous operation paused: {}",
                        reason
                    ));
                }
                KillSwitchState::Enabled => {}
            }
        }

        // Check resource limits
        if let Some(reason) = self.check_limits() {
            return PermissionDecision::Deny(format!("Resource limit: {}", reason));
        }

        // Check if tool requires approval
        if self.requires_approval(tool, args) {
            // Check if we have recent approval for this tool
            let history = self.approval_history.lock().unwrap();
            if let Some(last_approval) = history.get(tool) {
                if last_approval.elapsed() < Duration::from_secs(300) { // 5 minutes
                     // Approval still valid, proceed
                } else {
                    // Approval expired, require new approval
                    return PermissionDecision::Deny(format!(
                        "Approval required for {} (previous approval expired)",
                        tool
                    ));
                }
            } else {
                // No approval yet
                return PermissionDecision::Deny(format!("Approval required for {}", tool));
            }
        }

        // Update statistics
        self.update_stats(tool, args);

        // Delegate to inner permission
        self.inner.check(tool, args)
    }
}

/// Emergency kill switch that can be triggered externally.
pub struct GlobalKillSwitch {
    state: Arc<Mutex<KillSwitchState>>,
}

impl Default for GlobalKillSwitch {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalKillSwitch {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(KillSwitchState::Enabled)),
        }
    }

    pub fn trigger(&self, reason: String) {
        let mut state = self.state.lock().unwrap();
        *state = KillSwitchState::Triggered(reason);
    }

    pub fn pause(&self, reason: String) {
        let mut state = self.state.lock().unwrap();
        *state = KillSwitchState::Paused(reason);
    }

    pub fn resume(&self) {
        let mut state = self.state.lock().unwrap();
        *state = KillSwitchState::Enabled;
    }

    pub fn state(&self) -> KillSwitchState {
        self.state.lock().unwrap().clone()
    }

    pub fn create_layer(&self, inner: Arc<dyn Permission>) -> AutonomousSecurityLayer {
        AutonomousSecurityLayer::with_kill_switch(
            inner,
            AutonomousSecurityConfig::default(),
            Arc::clone(&self.state),
        )
    }
}

/// Autonomous operation monitor for real-time monitoring.
pub struct AutonomousMonitor {
    security_layer: Arc<AutonomousSecurityLayer>,
    alert_handlers: Vec<Box<dyn Fn(String) + Send + Sync>>,
}

impl AutonomousMonitor {
    pub fn new(security_layer: Arc<AutonomousSecurityLayer>) -> Self {
        Self {
            security_layer,
            alert_handlers: Vec::new(),
        }
    }

    pub fn add_alert_handler<F: Fn(String) + Send + Sync + 'static>(&mut self, handler: F) {
        self.alert_handlers.push(Box::new(handler));
    }

    pub fn check_and_alert(&self) {
        // Check kill switch
        if self.security_layer.is_kill_switch_triggered() {
            let state = self.security_layer.kill_switch_state();
            if let KillSwitchState::Triggered(reason) = state {
                self.send_alert(format!("Kill switch triggered: {}", reason));
            }
        }

        // Check resource limits
        // (Implementation would check stats and send alerts)
    }

    fn send_alert(&self, message: String) {
        for handler in &self.alert_handlers {
            handler(message.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::AllowAll;

    #[test]
    fn test_kill_switch() {
        let kill_switch = GlobalKillSwitch::new();
        let security_layer = kill_switch.create_layer(Arc::new(AllowAll));

        // Initially enabled
        assert!(!security_layer.is_kill_switch_triggered());

        // Trigger kill switch
        kill_switch.trigger("Test emergency".to_string());

        // Check that tool calls are denied
        let decision = security_layer.check("edit_file", &Value::Null);
        match decision {
            PermissionDecision::HardDeny(reason) => {
                assert!(reason.contains("Kill switch triggered"));
            }
            _ => panic!("Expected HardDeny"),
        }
    }

    #[test]
    fn test_resource_limits() {
        let config = AutonomousSecurityConfig {
            max_tool_calls: 2,
            ..Default::default()
        };

        let security_layer = AutonomousSecurityLayer::new(Arc::new(AllowAll), config);

        // First call should succeed
        let decision1 = security_layer.check("read_file", &Value::Null);
        assert!(matches!(decision1, PermissionDecision::Allow));

        // Second call should succeed
        let decision2 = security_layer.check("read_file", &Value::Null);
        assert!(matches!(decision2, PermissionDecision::Allow));

        // Third call should fail due to limit
        let decision3 = security_layer.check("read_file", &Value::Null);
        match decision3 {
            PermissionDecision::Deny(reason) => {
                assert!(reason.contains("Max tool calls exceeded"));
            }
            _ => panic!("Expected Deny"),
        }
    }
}
