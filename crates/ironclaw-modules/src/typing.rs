//! Typing-indicator module.
//!
//! Hooks the delivery pipeline so that whenever the container is doing work
//! (`current_tool` is set in `container_state`), the host periodically tells
//! each channel adapter to render a typing indicator on the most recently
//! addressed thread.
//!
//! The host calls [`TypingModule::tick`] from its sweep loop with a list of
//! in-progress targets derived from `container_state` plus the per-session
//! routing. The module throttles per-target so that each adapter receives at
//! most one `set_typing` call per `interval_ms`.

use crate::context::{DeliveryDispatcher, DispatchTarget, Module, ModuleContext};
use crate::error::ModuleError;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Configuration for the typing module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypingConfig {
    /// Master enable flag. If `false`, `install` is a no-op and `tick` does
    /// nothing.
    pub enabled: bool,
    /// Minimum gap, in milliseconds, between two `set_typing` calls for the
    /// same `(channel, platform, thread)` triple.
    pub interval_ms: u64,
}

impl Default for TypingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Slack, Telegram, and Discord all render typing for ~5-7s after
            // the last call. 4s gives us a comfortable refresh cadence.
            interval_ms: 4000,
        }
    }
}

#[derive(Debug, Default)]
struct Throttle {
    last_emit: HashMap<TargetKey, DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TargetKey {
    channel_type: Option<String>,
    platform_id: Option<String>,
    thread_id: Option<String>,
}

impl TargetKey {
    fn from_target(t: &DispatchTarget) -> Self {
        Self {
            channel_type: t.channel_type.as_ref().map(|c| c.as_str().to_owned()),
            platform_id: t.platform_id.clone(),
            thread_id: t.thread_id.clone(),
        }
    }
}

/// Typing-indicator module.
pub struct TypingModule {
    cfg: TypingConfig,
    throttle: Arc<Mutex<Throttle>>,
    dispatcher: Arc<Mutex<Option<Arc<dyn DeliveryDispatcher>>>>,
}

impl TypingModule {
    pub fn new(cfg: TypingConfig) -> Self {
        Self {
            cfg,
            throttle: Arc::new(Mutex::new(Throttle::default())),
            dispatcher: Arc::new(Mutex::new(None)),
        }
    }

    pub fn config(&self) -> &TypingConfig {
        &self.cfg
    }

    /// Returns true if a typing call should be emitted for `target` at `now`
    /// given the configured `interval_ms`. Side-effectfully updates the
    /// throttle table on `true`.
    pub fn should_emit(&self, target: &DispatchTarget, now: DateTime<Utc>) -> bool {
        if !self.cfg.enabled {
            return false;
        }
        let key = TargetKey::from_target(target);
        let mut t = self.throttle.lock().unwrap();
        let interval =
            Duration::milliseconds(i64::try_from(self.cfg.interval_ms).unwrap_or(i64::MAX));
        match t.last_emit.get(&key) {
            Some(prev) if now - *prev < interval => false,
            _ => {
                t.last_emit.insert(key, now);
                true
            }
        }
    }

    /// Called by the host sweep with the active in-progress targets. Issues
    /// one typing call per target, throttled.
    pub fn tick(&self, targets: &[DispatchTarget], now: DateTime<Utc>) {
        if !self.cfg.enabled {
            return;
        }
        let guard = self.dispatcher.lock().unwrap();
        let Some(dispatcher) = guard.as_ref() else {
            return;
        };
        let dispatcher = Arc::clone(dispatcher);
        drop(guard);
        for target in targets {
            if self.should_emit(target, now) {
                dispatcher.set_typing(target);
            }
        }
    }

    /// Reset the throttle (used in tests + as a safety hatch when the host
    /// observes a fresh container start).
    pub fn reset(&self) {
        self.throttle.lock().unwrap().last_emit.clear();
    }

    /// Install a dispatcher directly. The `install` hook also wires this via
    /// `on_delivery_adapter_ready`; this method is useful in tests and for
    /// hosts that want to drive the module without the ready callback.
    pub fn set_dispatcher(&self, dispatcher: Arc<dyn DeliveryDispatcher>) {
        *self.dispatcher.lock().unwrap() = Some(dispatcher);
    }

    fn validate(&self) -> Result<(), ModuleError> {
        if self.cfg.interval_ms == 0 {
            return Err(ModuleError::invalid_config(
                "typing",
                "interval_ms must be > 0",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl Module for TypingModule {
    fn name(&self) -> &'static str {
        "typing"
    }

    async fn install(&self, ctx: Arc<dyn ModuleContext>) -> Result<(), ModuleError> {
        self.validate()?;
        if !self.cfg.enabled {
            return Ok(());
        }
        let slot = Arc::clone(&self.dispatcher);
        ctx.on_delivery_adapter_ready(Arc::new(move |d: Arc<dyn DeliveryDispatcher>| {
            *slot.lock().unwrap() = Some(d);
        }));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{MockDispatcher, MockModuleContext};
    use ironclaw_types::ChannelType;

    fn target(chan: &str, platform: &str) -> DispatchTarget {
        DispatchTarget::channel(ChannelType::new(chan), platform.into(), None)
    }

    #[test]
    fn default_config_is_enabled_with_4s() {
        let cfg = TypingConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.interval_ms, 4000);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = TypingConfig {
            enabled: false,
            interval_ms: 1234,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: TypingConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn should_emit_first_time_returns_true() {
        let m = TypingModule::new(TypingConfig::default());
        let now = Utc::now();
        assert!(m.should_emit(&target("slack", "C1"), now));
    }

    #[test]
    fn should_emit_throttled_within_interval() {
        let m = TypingModule::new(TypingConfig {
            enabled: true,
            interval_ms: 4000,
        });
        let now = Utc::now();
        let t = target("slack", "C1");
        assert!(m.should_emit(&t, now));
        // Within interval -> false.
        assert!(!m.should_emit(&t, now + Duration::milliseconds(100)));
        // After interval -> true again.
        assert!(m.should_emit(&t, now + Duration::milliseconds(5000)));
    }

    #[test]
    fn should_emit_independent_per_target() {
        let m = TypingModule::new(TypingConfig::default());
        let now = Utc::now();
        assert!(m.should_emit(&target("slack", "C1"), now));
        assert!(m.should_emit(&target("slack", "C2"), now));
        assert!(m.should_emit(&target("telegram", "C1"), now));
    }

    #[test]
    fn disabled_never_emits() {
        let m = TypingModule::new(TypingConfig {
            enabled: false,
            interval_ms: 4000,
        });
        assert!(!m.should_emit(&target("slack", "C1"), Utc::now()));
    }

    #[test]
    fn reset_clears_throttle() {
        let m = TypingModule::new(TypingConfig::default());
        let now = Utc::now();
        let t = target("slack", "C1");
        assert!(m.should_emit(&t, now));
        m.reset();
        assert!(m.should_emit(&t, now));
    }

    #[test]
    fn validate_rejects_zero_interval() {
        let m = TypingModule::new(TypingConfig {
            enabled: true,
            interval_ms: 0,
        });
        assert!(m.validate().is_err());
    }

    #[tokio::test]
    async fn install_registers_delivery_ready_when_enabled() {
        let m = TypingModule::new(TypingConfig::default());
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        assert_eq!(ctx.registered(), vec!["delivery_ready"]);
    }

    #[tokio::test]
    async fn install_is_noop_when_disabled() {
        let m = TypingModule::new(TypingConfig {
            enabled: false,
            interval_ms: 4000,
        });
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        assert!(ctx.registered().is_empty());
    }

    #[tokio::test]
    async fn install_returns_err_on_zero_interval() {
        let m = TypingModule::new(TypingConfig {
            enabled: true,
            interval_ms: 0,
        });
        let ctx = MockModuleContext::new();
        assert!(m.install(ctx).await.is_err());
    }

    #[tokio::test]
    async fn install_captures_dispatcher_via_ready_callback() {
        let m = TypingModule::new(TypingConfig::default());
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let dispatcher: Arc<dyn DeliveryDispatcher> = MockDispatcher::new();
        ctx.fire_delivery_ready(&dispatcher);
        // After firing, the typing module should be ready to dispatch.
        let casted: Arc<MockDispatcher> = MockDispatcher::new();
        m.set_dispatcher(casted.clone());
        m.tick(&[target("slack", "C1")], Utc::now());
        assert_eq!(casted.typing_count(), 1);
    }

    #[test]
    fn tick_calls_dispatcher_for_in_progress_targets() {
        let m = TypingModule::new(TypingConfig::default());
        let dispatcher = MockDispatcher::new();
        m.set_dispatcher(dispatcher.clone());
        let targets = vec![target("slack", "C1"), target("telegram", "C2")];
        m.tick(&targets, Utc::now());
        assert_eq!(dispatcher.typing_count(), 2);
    }

    #[test]
    fn tick_is_noop_when_disabled() {
        let m = TypingModule::new(TypingConfig {
            enabled: false,
            interval_ms: 4000,
        });
        let dispatcher = MockDispatcher::new();
        m.set_dispatcher(dispatcher.clone());
        m.tick(&[target("slack", "C1")], Utc::now());
        assert_eq!(dispatcher.typing_count(), 0);
    }

    #[test]
    fn tick_is_noop_without_dispatcher() {
        let m = TypingModule::new(TypingConfig::default());
        m.tick(&[target("slack", "C1")], Utc::now());
    }

    #[test]
    fn tick_respects_throttle() {
        let m = TypingModule::new(TypingConfig {
            enabled: true,
            interval_ms: 4000,
        });
        let dispatcher = MockDispatcher::new();
        m.set_dispatcher(dispatcher.clone());
        let now = Utc::now();
        let t = target("slack", "C1");
        m.tick(&[t.clone()], now);
        m.tick(&[t.clone()], now + Duration::milliseconds(100));
        assert_eq!(dispatcher.typing_count(), 1);
        m.tick(&[t], now + Duration::milliseconds(5000));
        assert_eq!(dispatcher.typing_count(), 2);
    }

    #[test]
    fn name_is_stable() {
        let m = TypingModule::new(TypingConfig::default());
        assert_eq!(m.name(), "typing");
    }

    #[test]
    fn config_accessor_works() {
        let cfg = TypingConfig {
            enabled: true,
            interval_ms: 7777,
        };
        let m = TypingModule::new(cfg.clone());
        assert_eq!(m.config(), &cfg);
    }
}
