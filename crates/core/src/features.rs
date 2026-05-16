//! Safe feature system for Aegis.
//!
//! This module provides controlled access to advanced features with
//! safety guarantees and resource limits.

use std::sync::Arc;
use tokio::sync::RwLock;

/// Feature configuration from CLI/config.
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

/// Feature manager with safety controls.
pub struct FeatureManager {
    config: FeatureConfig,
    usage_tracker: UsageTracker,
}

impl FeatureManager {
    pub fn new(config: FeatureConfig) -> Self {
        Self {
            config,
            usage_tracker: UsageTracker::new(),
        }
    }

    /// Check if a feature is enabled and safe to use.
    pub async fn check_feature(
        &self,
        feature: &str,
        context: &FeatureContext,
    ) -> Result<(), String> {
        match feature {
            "multi_model_evaluation" => {
                if !self.config.multi_model_evaluation {
                    return Err("Multi-model evaluation is disabled".to_string());
                }
                self.check_budget(context.estimated_cost).await?;
            }
            "prompt_perturbation" => {
                if !self.config.prompt_perturbation {
                    return Err("Prompt perturbation is disabled".to_string());
                }
                if self.config.require_confirmation && !context.user_confirmed {
                    return Err("User confirmation required".to_string());
                }
            }
            "parallel_models" => {
                if !self.config.parallel_models {
                    return Err("Parallel models is disabled".to_string());
                }
                self.check_budget(context.estimated_cost).await?;
            }
            _ => return Err(format!("Unknown feature: {}", feature)),
        }

        Ok(())
    }

    /// Check budget limits.
    async fn check_budget(&self, estimated_cost: f64) -> Result<(), String> {
        let total_used = self.usage_tracker.get_total_cost().await;
        let projected_total = total_used + estimated_cost;

        if projected_total > self.config.multi_model_budget_usd {
            return Err(format!(
                "Budget exceeded: ${:.2} used + ${:.2} estimated > ${:.2} limit",
                total_used, estimated_cost, self.config.multi_model_budget_usd
            ));
        }

        Ok(())
    }

    /// Record feature usage.
    pub async fn record_usage(&self, feature: &str, cost: f64) {
        self.usage_tracker.record(feature, cost).await;
    }

    /// Get usage statistics.
    pub async fn get_usage(&self) -> UsageStats {
        self.usage_tracker.get_stats().await
    }
}

/// Context for feature usage.
pub struct FeatureContext {
    pub estimated_cost: f64,
    pub user_confirmed: bool,
    pub safety_level: SafetyLevel,
}

impl Default for FeatureContext {
    fn default() -> Self {
        Self {
            estimated_cost: 0.0,
            user_confirmed: false,
            safety_level: SafetyLevel::Medium,
        }
    }
}

/// Safety level for feature usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafetyLevel {
    /// Only safe, read-only operations.
    High,
    /// Limited write operations with checks.
    Medium,
    /// Advanced operations with strict monitoring.
    Low,
}

/// Usage tracker for features.
struct UsageTracker {
    data: Arc<RwLock<UsageData>>,
}

impl UsageTracker {
    fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(UsageData::default())),
        }
    }

    async fn record(&self, feature: &str, cost: f64) {
        let mut data = self.data.write().await;
        data.total_cost += cost;
        data.feature_usage
            .entry(feature.to_string())
            .and_modify(|c| *c += cost)
            .or_insert(cost);
    }

    async fn get_total_cost(&self) -> f64 {
        let data = self.data.read().await;
        data.total_cost
    }

    async fn get_stats(&self) -> UsageStats {
        let data = self.data.read().await;
        UsageStats {
            total_cost: data.total_cost,
            feature_usage: data.feature_usage.clone(),
        }
    }
}

#[derive(Debug, Default)]
struct UsageData {
    total_cost: f64,
    feature_usage: std::collections::HashMap<String, f64>,
}

/// Usage statistics.
#[derive(Debug, Clone)]
pub struct UsageStats {
    pub total_cost: f64,
    pub feature_usage: std::collections::HashMap<String, f64>,
}

/// Safe multi-model evaluation wrapper.
pub struct SafeMultiModelEvaluator {
    feature_manager: Arc<FeatureManager>,
    // Would contain actual evaluator when feature is enabled
}

impl SafeMultiModelEvaluator {
    pub fn new(feature_manager: Arc<FeatureManager>) -> Self {
        Self { feature_manager }
    }

    /// Safely evaluate using multiple models.
    pub async fn evaluate(
        &self,
        prompt: &str,
        models: &[&str],
    ) -> Result<Vec<MultiModelResult>, String> {
        // Check if feature is enabled
        let context = FeatureContext {
            estimated_cost: self.estimate_cost(models.len()),
            user_confirmed: true, // Assume confirmed for now
            safety_level: SafetyLevel::Medium,
        };

        self.feature_manager
            .check_feature("multi_model_evaluation", &context)
            .await?;

        // Simulate evaluation (would use real evaluator)
        let results = self.simulate_evaluation(prompt, models).await;

        // Record usage
        let cost = self.estimate_cost(models.len());
        self.feature_manager
            .record_usage("multi_model_evaluation", cost)
            .await;

        Ok(results)
    }

    fn estimate_cost(&self, num_models: usize) -> f64 {
        // Rough estimate
        num_models as f64 * 0.01
    }

    async fn simulate_evaluation(&self, prompt: &str, models: &[&str]) -> Vec<MultiModelResult> {
        let mut results = Vec::new();
        for (i, &model) in models.iter().enumerate() {
            // Simulate API call
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            results.push(MultiModelResult {
                model: model.to_string(),
                response: format!("Response {} from {}: {}", i + 1, model, prompt),
                score: 0.7 + (i as f32 * 0.1),
                cost_usd: 0.01,
            });
        }
        results
    }
}

/// Result from multi-model evaluation.
#[derive(Debug, Clone)]
pub struct MultiModelResult {
    pub model: String,
    pub response: String,
    pub score: f32,
    pub cost_usd: f64,
}

/// Safe prompt perturbation wrapper.
pub struct SafePromptPerturbator {
    feature_manager: Arc<FeatureManager>,
}

impl SafePromptPerturbator {
    pub fn new(feature_manager: Arc<FeatureManager>) -> Self {
        Self { feature_manager }
    }

    /// Safely perturb a prompt.
    pub async fn perturb(&self, prompt: &str, user_confirmed: bool) -> Result<Vec<String>, String> {
        let context = FeatureContext {
            estimated_cost: 0.0,
            user_confirmed,
            safety_level: SafetyLevel::High,
        };

        self.feature_manager
            .check_feature("prompt_perturbation", &context)
            .await?;

        // Safe perturbations only
        let perturbations = vec![
            format!("Rephrased: {}", prompt),
            format!("Detailed: Please explain {}", prompt),
            format!("Simple: {}", prompt.to_lowercase()),
        ];

        self.feature_manager
            .record_usage("prompt_perturbation", 0.0)
            .await;

        Ok(perturbations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_feature_manager_disabled() {
        let config = FeatureConfig::default();
        let manager = FeatureManager::new(config);
        let context = FeatureContext::default();

        assert!(manager
            .check_feature("multi_model_evaluation", &context)
            .await
            .is_err());
        assert!(manager
            .check_feature("prompt_perturbation", &context)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_feature_manager_enabled() {
        let config = FeatureConfig {
            multi_model_evaluation: true,
            prompt_perturbation: true,
            ..FeatureConfig::default()
        };

        let manager = FeatureManager::new(config);
        let context = FeatureContext::default();

        // Should fail without confirmation
        assert!(manager
            .check_feature("prompt_perturbation", &context)
            .await
            .is_err());

        // Should work with confirmation
        let context_confirmed = FeatureContext {
            user_confirmed: true,
            ..Default::default()
        };
        assert!(manager
            .check_feature("prompt_perturbation", &context_confirmed)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_budget_check() {
        let config = FeatureConfig {
            multi_model_evaluation: true,
            multi_model_budget_usd: 1.0,
            ..FeatureConfig::default()
        };

        let manager = FeatureManager::new(config);

        // First check should pass
        let context1 = FeatureContext {
            estimated_cost: 0.5,
            user_confirmed: true,
            ..Default::default()
        };
        assert!(manager
            .check_feature("multi_model_evaluation", &context1)
            .await
            .is_ok());

        // Record usage
        manager.record_usage("multi_model_evaluation", 0.5).await;

        // Second check should fail (exceeds budget)
        let context2 = FeatureContext {
            estimated_cost: 0.6,
            user_confirmed: true,
            ..Default::default()
        };
        assert!(manager
            .check_feature("multi_model_evaluation", &context2)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_safe_evaluator() {
        let config = FeatureConfig {
            multi_model_evaluation: true,
            ..FeatureConfig::default()
        };

        let manager = Arc::new(FeatureManager::new(config));
        let evaluator = SafeMultiModelEvaluator::new(manager.clone());

        let models = vec!["model1", "model2"];
        let results = evaluator.evaluate("test", &models).await;

        assert!(results.is_ok());
        let results = results.unwrap();
        assert_eq!(results.len(), 2);
        // Scores should be different (simulated)
        assert_ne!(results[0].score, results[1].score);
    }
}
