//! Outbound notification fan-out. The agent loop emits events
//! (decision / tool / error) into a global feed; the feed forwards
//! them to whichever channels are enabled in the live config. Every
//! delivery is fire-and-forget — a Telegram outage must never block
//! the agent from answering or stall a tool round.

pub mod telegram;
pub mod webhook;

use crate::config::ChannelsConfig;
use std::sync::OnceLock;
use std::sync::RwLock;

/// One published event. `kind` lets a channel filter ("decision",
/// "tool", "error"); `text` is the body rendered for human reading.
/// Currently inlined at call sites; kept here for the future bus
/// queue / batching layer.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ChannelEvent {
    pub kind: String,
    pub text: String,
}

struct FeedInner {
    config: RwLock<ChannelsConfig>,
}

impl FeedInner {
    fn new(cfg: ChannelsConfig) -> Self {
        Self { config: RwLock::new(cfg) }
    }

    fn snapshot(&self) -> ChannelsConfig {
        self.config.read().map(|g| g.clone()).unwrap_or_default()
    }
}

static FEED: OnceLock<FeedInner> = OnceLock::new();

/// Wire the feed at app startup. Idempotent: a second call only swaps
/// the live config (used by `save_config` after the user edits the
/// settings panel).
pub fn init(cfg: ChannelsConfig) {
    if let Some(existing) = FEED.get() {
        if let Ok(mut g) = existing.config.write() {
            *g = cfg;
        }
    } else {
        let _ = FEED.set(FeedInner::new(cfg));
    }
}

/// Publish an event. Spawns a tokio task per channel so the caller
/// never waits on network I/O. Safe to call before `init`: an
/// un-initialised feed silently drops events.
pub fn publish(kind: &str, text: &str) {
    let Some(inner) = FEED.get() else { return };
    let cfg = inner.snapshot();

    if cfg.telegram.enabled && cfg.telegram.events.iter().any(|e| e == kind) {
        let bot_token = cfg.telegram.bot_token.clone();
        let chat_id = cfg.telegram.chat_id.clone();
        let formatted = format!("[{}] {}", kind, text);
        tokio::spawn(async move {
            if let Err(e) = telegram::send(&bot_token, &chat_id, &formatted).await {
                eprintln!("[channel:telegram] send failed: {}", e);
            }
        });
    }

    if cfg.webhook.enabled && cfg.webhook.events.iter().any(|e| e == kind) {
        let url = cfg.webhook.url.clone();
        let token = cfg.webhook.bearer_token.clone();
        let kind_s = kind.to_string();
        let text_s = text.to_string();
        tokio::spawn(async move {
            if let Err(e) = webhook::send(&url, &token, &kind_s, &text_s).await {
                eprintln!("[channel:webhook] send failed: {}", e);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_without_init_is_noop() {
        // Nothing has called init(); this should not panic.
        publish("decision", "hello");
    }

    #[test]
    fn snapshot_returns_live_config() {
        let cfg = ChannelsConfig::default();
        let inner = FeedInner::new(cfg);
        let snap = inner.snapshot();
        assert!(!snap.telegram.enabled);
    }
}
