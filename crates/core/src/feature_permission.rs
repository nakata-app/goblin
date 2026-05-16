//! Feature-aware permission wrapper.
//!
//! Wraps an existing permission implementation and adds feature flag checks
//! for advanced capabilities.

use crate::permission::{Permission, PermissionDecision};
use serde_json::Value;
use std::sync::Arc;

/// Feature configuration for permission checks.
#[derive(Debug, Clone)]
pub struct FeatureConfig {
    /// Enable multi-model evaluation.
    pub multi_model_evaluation: bool,
    /// Enable prompt perturbation.
    pub prompt_perturbation: bool,
    /// Enable parallel models.
    pub parallel_models: bool,
    /// Maximum parallel models.
    pub max_parallel_models: u32,
    /// Require confirmation for advanced features.
    pub require_confirmation: bool,
    /// Maximum budget for multi-model calls.
    pub multi_model_budget_usd: f64,
}

impl Default for FeatureConfig {
    fn default() -> Self {
        Self {
            multi_model_evaluation: false,
            prompt_perturbation: false,
            parallel_models: false,
            max_parallel_models: 3,
            require_confirmation: true,
            multi_model_budget_usd: 5.0,
        }
    }
}

/// Feature-aware permission wrapper.
pub struct FeaturePermission {
    inner: Arc<dyn Permission>,
    config: FeatureConfig,
    usage_tracker: Arc<std::sync::Mutex<UsageTracker>>,
}

impl FeaturePermission {
    pub fn new(inner: Arc<dyn Permission>, config: FeatureConfig) -> Self {
        Self {
            inner,
            config,
            usage_tracker: Arc::new(std::sync::Mutex::new(UsageTracker::new())),
        }
    }

    /// Check if a feature-specific tool is allowed.
    fn check_feature_tool(&self, tool: &str, args: &Value) -> Option<PermissionDecision> {
        match tool {
            "multi_model_evaluate" => {
                if !self.config.multi_model_evaluation {
                    return Some(PermissionDecision::HardDeny(
                        "multi_model_evaluation feature is disabled".to_string(),
                    ));
                }

                // Check budget
                let estimated_cost = self.estimate_multi_model_cost(args);
                let tracker = self.usage_tracker.lock().unwrap();
                if tracker.total_cost + estimated_cost > self.config.multi_model_budget_usd {
                    return Some(PermissionDecision::HardDeny(format!(
                        "budget exceeded: ${:.2} used + ${:.2} estimated > ${:.2} limit",
                        tracker.total_cost, estimated_cost, self.config.multi_model_budget_usd
                    )));
                }

                // Check confirmation
                if self.config.require_confirmation {
                    // This will be handled by the interactive prompt
                    return None;
                }
            }
            "perturb_prompt" => {
                if !self.config.prompt_perturbation {
                    return Some(PermissionDecision::HardDeny(
                        "prompt_perturbation feature is disabled".to_string(),
                    ));
                }

                if self.config.require_confirmation {
                    return None; // Let interactive prompt handle it
                }
            }
            "parallel_models" => {
                if !self.config.parallel_models {
                    return Some(PermissionDecision::HardDeny(
                        "parallel_models feature is disabled".to_string(),
                    ));
                }

                // Check max parallel models
                if let Some(num_models) = args.get("models").and_then(|v| v.as_array()) {
                    if num_models.len() > self.config.max_parallel_models as usize {
                        return Some(PermissionDecision::HardDeny(format!(
                            "exceeds max_parallel_models limit: {} > {}",
                            num_models.len(),
                            self.config.max_parallel_models
                        )));
                    }
                }

                // Check budget
                let estimated_cost = self.estimate_parallel_cost(args);
                let tracker = self.usage_tracker.lock().unwrap();
                if tracker.total_cost + estimated_cost > self.config.multi_model_budget_usd {
                    return Some(PermissionDecision::HardDeny(format!(
                        "budget exceeded: ${:.2} used + ${:.2} estimated > ${:.2} limit",
                        tracker.total_cost, estimated_cost, self.config.multi_model_budget_usd
                    )));
                }
            }
            _ => {}
        }

        None
    }

    fn estimate_multi_model_cost(&self, args: &Value) -> f64 {
        // Simple estimation: $0.01 per model
        args.get("models")
            .and_then(|v| v.as_array())
            .map(|arr| arr.len() as f64 * 0.01)
            .unwrap_or(0.01)
    }

    fn estimate_parallel_cost(&self, args: &Value) -> f64 {
        // Simple estimation: $0.005 per model
        args.get("models")
            .and_then(|v| v.as_array())
            .map(|arr| arr.len() as f64 * 0.005)
            .unwrap_or(0.005)
    }

    /// Record feature usage after successful execution.
    pub fn record_usage(&self, tool: &str, cost: f64) {
        let mut tracker = self.usage_tracker.lock().unwrap();
        tracker.record(tool, cost);
    }

    /// Get usage statistics.
    pub fn get_usage(&self) -> UsageStats {
        let tracker = self.usage_tracker.lock().unwrap();
        tracker.get_stats()
    }
}

impl Permission for FeaturePermission {
    fn check(&self, tool: &str, args: &Value) -> PermissionDecision {
        // First check feature-specific restrictions
        if let Some(decision) = self.check_feature_tool(tool, args) {
            return decision;
        }

        // Then delegate to inner permission
        self.inner.check(tool, args)
    }
}

/// Usage tracker for feature costs.
struct UsageTracker {
    total_cost: f64,
    feature_usage: std::collections::HashMap<String, f64>,
}

impl UsageTracker {
    fn new() -> Self {
        Self {
            total_cost: 0.0,
            feature_usage: std::collections::HashMap::new(),
        }
    }

    fn record(&mut self, feature: &str, cost: f64) {
        self.total_cost += cost;
        *self.feature_usage.entry(feature.to_string()).or_insert(0.0) += cost;
    }

    fn get_stats(&self) -> UsageStats {
        UsageStats {
            total_cost: self.total_cost,
            feature_usage: self.feature_usage.clone(),
        }
    }
}

/// Usage statistics.
#[derive(Debug, Clone)]
pub struct UsageStats {
    pub total_cost: f64,
    pub feature_usage: std::collections::HashMap<String, f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::AllowAll;
    use std::sync::Arc;

    #[test]
    fn test_feature_disabled() {
        let config = FeatureConfig::default();
        let inner = Arc::new(AllowAll);
        let permission = FeaturePermission::new(inner, config);

        let args = serde_json::json!({
            "models": ["model1", "model2"]
        });

        let decision = permission.check("multi_model_evaluate", &args);
        assert!(matches!(decision, PermissionDecision::HardDeny(_)));
    }

    #[test]
    fn test_feature_enabled_with_budget() {
        let config = FeatureConfig {
            multi_model_evaluation: true,
            multi_model_budget_usd: 0.01, // Very small budget
            ..FeatureConfig::default()
        };

        let inner = Arc::new(AllowAll);
        let permission = FeaturePermission::new(inner, config);

        let args = serde_json::json!({
            "models": ["model1", "model2"] // Estimated cost: $0.02
        });

        let decision = permission.check("multi_model_evaluate", &args);
        assert!(matches!(decision, PermissionDecision::HardDeny(_)));
        if let PermissionDecision::HardDeny(msg) = decision {
            assert!(msg.contains("budget exceeded"));
        }
    }

    #[test]
    fn test_parallel_models_limit() {
        let config = FeatureConfig {
            parallel_models: true,
            max_parallel_models: 2,
            ..FeatureConfig::default()
        };

        let inner = Arc::new(AllowAll);
        let permission = FeaturePermission::new(inner, config);

        let args = serde_json::json!({
            "models": ["model1", "model2", "model3"] // 3 > 2
        });

        let decision = permission.check("parallel_models", &args);
        assert!(matches!(decision, PermissionDecision::HardDeny(_)));
        if let PermissionDecision::HardDeny(msg) = decision {
            assert!(msg.contains("exceeds max_parallel_models"));
        }
    }

    #[test]
    fn test_usage_tracking() {
        let config = FeatureConfig::default();
        let inner = Arc::new(AllowAll);
        let permission = FeaturePermission::new(inner, config);

        permission.record_usage("multi_model_evaluate", 0.05);
        permission.record_usage("multi_model_evaluate", 0.03);

        let stats = permission.get_usage();
        assert_eq!(stats.total_cost, 0.08);
        assert_eq!(stats.feature_usage["multi_model_evaluate"], 0.08);
    }
}
