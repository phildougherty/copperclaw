//! Registry of channel factories keyed by `ChannelType`.

use crate::adapter::ChannelFactory;
use crate::error::AdapterError;
use copperclaw_types::ChannelType;
use std::collections::HashMap;
use std::sync::Arc;

/// In-process registry of available `ChannelFactory` implementations.
///
/// Channel crates call `register(&mut reg)` at host startup; the host
/// then looks up factories by `ChannelType` when wiring channels listed
/// in the central DB.
///
/// Duplicate registrations are rejected: the second call to `register`
/// for the same `ChannelType` returns `AdapterError::BadRequest`. This
/// prevents silent shadowing between channel crates.
#[derive(Default)]
pub struct ChannelRegistry {
    factories: HashMap<ChannelType, Arc<dyn ChannelFactory>>,
}

impl ChannelRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a factory. Errors if a factory for the same `ChannelType`
    /// is already present.
    pub fn register(&mut self, factory: Arc<dyn ChannelFactory>) -> Result<(), AdapterError> {
        let ct = factory.channel_type();
        if self.factories.contains_key(&ct) {
            return Err(AdapterError::BadRequest(format!(
                "channel factory already registered for {ct}"
            )));
        }
        self.factories.insert(ct, factory);
        Ok(())
    }

    /// Look up a factory by channel type.
    pub fn get(&self, ct: &ChannelType) -> Option<Arc<dyn ChannelFactory>> {
        self.factories.get(ct).cloned()
    }

    /// List the channel types this registry knows about.
    pub fn channel_types(&self) -> Vec<ChannelType> {
        self.factories.keys().cloned().collect()
    }

    /// Number of registered factories.
    pub fn len(&self) -> usize {
        self.factories.len()
    }

    /// True when no factories are registered.
    pub fn is_empty(&self) -> bool {
        self.factories.is_empty()
    }
}

impl std::fmt::Debug for ChannelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelRegistry")
            .field("channel_types", &self.channel_types())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MockFactory;

    #[test]
    fn new_is_empty() {
        let r = ChannelRegistry::new();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert!(r.channel_types().is_empty());
    }

    #[test]
    fn default_equivalent_to_new() {
        let r = ChannelRegistry::default();
        assert!(r.is_empty());
    }

    #[test]
    fn register_then_get_roundtrips() {
        let mut r = ChannelRegistry::new();
        r.register(Arc::new(MockFactory::new("a"))).unwrap();
        let got = r.get(&ChannelType::new("a")).expect("present");
        assert_eq!(got.channel_type().as_str(), "a");
    }

    #[test]
    fn get_missing_returns_none() {
        let r = ChannelRegistry::new();
        assert!(r.get(&ChannelType::new("absent")).is_none());
    }

    #[test]
    fn duplicate_register_errors() {
        let mut r = ChannelRegistry::new();
        r.register(Arc::new(MockFactory::new("x"))).unwrap();
        let err = r
            .register(Arc::new(MockFactory::new("x")))
            .expect_err("duplicate must error");
        match err {
            AdapterError::BadRequest(msg) => assert!(msg.contains('x')),
            other => panic!("expected BadRequest, got {other:?}"),
        }
        // Original factory is still present.
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn channel_types_lists_all_registered() {
        let mut r = ChannelRegistry::new();
        r.register(Arc::new(MockFactory::new("a"))).unwrap();
        r.register(Arc::new(MockFactory::new("b"))).unwrap();
        let mut types: Vec<_> = r.channel_types().into_iter().map(|c| c.0).collect();
        types.sort();
        assert_eq!(types, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn debug_format_lists_channel_types() {
        let mut r = ChannelRegistry::new();
        r.register(Arc::new(MockFactory::new("d"))).unwrap();
        let s = format!("{r:?}");
        assert!(s.contains("ChannelRegistry"));
        assert!(s.contains('d'));
    }
}
