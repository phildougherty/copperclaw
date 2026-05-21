//! [`WhatsAppAdapter`] — the [`ChannelAdapter`] implementation for the
//! native (Baileys-style) WhatsApp gateway.
//!
//! ### What the adapter does today
//!
//! - Parses `platform_id` into `user:<wa_id>` / `group:<jid>` shapes.
//! - Holds a [`crate::config::WhatsAppConfig`] and the
//!   [`crate::keystore::Keystore`].
//! - Holds a [`crate::crypto::CryptoBackend`] — defaulting to the
//!   real [`crate::crypto::DalekBackend`] (X25519 / HKDF-SHA256 /
//!   AES-256-GCM / Ed25519). The legacy [`crate::crypto::StubBackend`]
//!   is still exported for tests that want the "no crypto installed"
//!   behaviour.
//! - Returns [`AdapterError::Unsupported`] from outbound calls even
//!   with the real backend installed: the Signal Protocol session
//!   state machine (X3DH + Double Ratchet) that sits above the
//!   primitives is a separate piece of work and has not been written
//!   yet. The error message distinguishes "stub backend, no primitives"
//!   from "real backend, no session pipeline".
//!
//! ### What the adapter doesn't do (yet)
//!
//! - Open the WebSocket. Connection lifecycle lives in
//!   [`crate::gateway::lifecycle`]; the adapter does not start it
//!   automatically because there is nothing useful to do with the
//!   frames until a real crypto backend is wired up. The
//!   [`testing::run_gateway_for_test`] helper exists for end-to-end
//!   tests against `MockTransport`.
//!
//! [`testing::run_gateway_for_test`]: crate::testing::run_gateway_for_test

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_channels_core::{AdapterError, ChannelAdapter, DmHandle};
use ironclaw_types::{ChannelType, OutboundMessage};
use tokio::sync::Mutex;
use tokio::sync::mpsc::Sender;

use crate::config::WhatsAppConfig;
use crate::crypto::{CryptoBackend, DalekBackend};
use crate::factory::CHANNEL_TYPE_STR;
use crate::keystore::Keystore;

/// Parsed form of a `platform_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendTarget {
    /// A direct chat with a WhatsApp user identified by `wa_id`.
    User(String),
    /// A group chat identified by `jid`.
    Group(String),
}

impl SendTarget {
    /// The string form of the target, suitable for use in protocol
    /// addresses.
    pub fn address(&self) -> &str {
        match self {
            Self::User(s) | Self::Group(s) => s.as_str(),
        }
    }

    /// `true` if the target is a group.
    pub fn is_group(&self) -> bool {
        matches!(self, Self::Group(_))
    }
}

/// Parse a `platform_id` into a [`SendTarget`].
///
/// Accepts `user:<wa_id>` or `group:<jid>`. Anything else surfaces as
/// [`AdapterError::BadRequest`].
pub fn parse_platform_id(platform_id: &str) -> Result<SendTarget, AdapterError> {
    if let Some(rest) = platform_id.strip_prefix("user:") {
        if rest.is_empty() {
            return Err(AdapterError::BadRequest(
                "whatsapp: empty user wa_id in platform_id".into(),
            ));
        }
        return Ok(SendTarget::User(rest.to_owned()));
    }
    if let Some(rest) = platform_id.strip_prefix("group:") {
        if rest.is_empty() {
            return Err(AdapterError::BadRequest(
                "whatsapp: empty group jid in platform_id".into(),
            ));
        }
        return Ok(SendTarget::Group(rest.to_owned()));
    }
    Err(AdapterError::BadRequest(format!(
        "whatsapp: platform_id must be `user:<wa_id>` or `group:<jid>`, got `{platform_id}`"
    )))
}

/// The WhatsApp channel adapter.
pub struct WhatsAppAdapter {
    channel_type: ChannelType,
    config: WhatsAppConfig,
    keystore: Mutex<Keystore>,
    crypto: Arc<dyn CryptoBackend>,
    #[allow(dead_code)] // wired up for the future delivery / decryption code
    inbound_tx: Sender<ironclaw_types::InboundEvent>,
}

impl std::fmt::Debug for WhatsAppAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhatsAppAdapter")
            .field("channel_type", &self.channel_type)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl WhatsAppAdapter {
    /// Construct an adapter with the real [`DalekBackend`] wired in
    /// (Curve25519 / X25519 / HKDF-SHA256 / AES-256-GCM / Ed25519).
    /// The cryptographic primitives work; outbound `deliver` still
    /// returns [`AdapterError::Unsupported`] because the Signal
    /// Protocol session-state machinery that sits above the primitives
    /// has not been written yet — see the `deliver` body.
    pub fn new(
        config: WhatsAppConfig,
        keystore: Keystore,
        inbound_tx: Sender<ironclaw_types::InboundEvent>,
    ) -> Self {
        Self::with_crypto_backend(
            config,
            keystore,
            inbound_tx,
            Arc::new(DalekBackend::new()),
        )
    }

    /// Construct an adapter with an explicit [`CryptoBackend`]. A
    /// future contributor wiring real e2e crypto uses this constructor
    /// to install their backend.
    pub fn with_crypto_backend(
        config: WhatsAppConfig,
        keystore: Keystore,
        inbound_tx: Sender<ironclaw_types::InboundEvent>,
        crypto: Arc<dyn CryptoBackend>,
    ) -> Self {
        Self {
            channel_type: ChannelType::new(CHANNEL_TYPE_STR),
            config,
            keystore: Mutex::new(keystore),
            crypto,
            inbound_tx,
        }
    }

    /// Read-only access to the configuration.
    pub fn config(&self) -> &WhatsAppConfig {
        &self.config
    }

    /// Borrow the active crypto backend.
    pub fn crypto_backend(&self) -> &Arc<dyn CryptoBackend> {
        &self.crypto
    }

    /// Snapshot of the keystore.
    pub async fn keystore_snapshot(&self) -> Keystore {
        self.keystore.lock().await.clone()
    }

    /// Replace the in-memory keystore. The on-disk file is **not**
    /// updated; callers must persist explicitly via
    /// [`crate::keystore::save`].
    pub async fn replace_keystore(&self, ks: Keystore) {
        *self.keystore.lock().await = ks;
    }
}

#[async_trait]
impl ChannelAdapter for WhatsAppAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    fn supports_threads(&self) -> bool {
        false
    }

    async fn subscribe(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        // We validate the platform_id so a typo at config time surfaces as
        // an error rather than being silently accepted.
        let _ = parse_platform_id(platform_id)?;
        Ok(())
    }

    async fn set_typing(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        let _ = parse_platform_id(platform_id)?;
        Err(unsupported(
            "set_typing requires a real CryptoBackend (the stub is wired in)",
        ))
    }

    async fn deliver(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        let target = parse_platform_id(platform_id)?;
        // Run the message through a couple of validation steps so a
        // malformed call still surfaces a BadRequest even with the stub
        // backend, rather than always returning Unsupported regardless.
        validate_outbound(&target, message)?;
        // Exercise the encryption boundary so the codepath is genuinely
        // wired up: the stub backend errors here, and we translate that
        // into an Unsupported result with a pointer to the missing
        // backend.
        match self.crypto.generate_keypair() {
            Ok(_) => {
                // A non-stub backend can reach this branch; the actual
                // outbound encryption pipeline lives behind a future
                // implementation. Surface Unsupported for now so the
                // adapter does not silently drop messages.
                Err(unsupported(
                    "outbound delivery pipeline not yet implemented above the CryptoBackend",
                ))
            }
            Err(crate::crypto::CryptoError::NotImplemented(_)) => Err(unsupported(
                "outbound delivery requires a real CryptoBackend (the stub is wired in)",
            )),
            Err(err) => Err(AdapterError::Transport(format!("whatsapp crypto: {err}"))),
        }
    }

    async fn open_dm(&self, user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        // The platform allows direct messaging any known wa_id; we
        // synthesise a handle so the host can wire delivery once a
        // real crypto backend is plugged in.
        if user_id.is_empty() {
            return Err(AdapterError::BadRequest(
                "whatsapp: open_dm requires a non-empty user id".into(),
            ));
        }
        Ok(Some(DmHandle {
            user_id: user_id.to_owned(),
            platform_id: format!("user:{user_id}"),
            channel_type: ChannelType::new(CHANNEL_TYPE_STR),
        }))
    }
}

/// Validate fields the adapter requires before talking to the crypto
/// backend. Returns `Ok(())` when the outbound is shaped correctly.
fn validate_outbound(target: &SendTarget, message: &OutboundMessage) -> Result<(), AdapterError> {
    if message.content.is_null() {
        return Err(AdapterError::BadRequest(
            "whatsapp: outbound content must not be null".into(),
        ));
    }
    if !message.content.is_object() && !message.content.is_string() {
        return Err(AdapterError::BadRequest(
            "whatsapp: outbound content must be an object or a string".into(),
        ));
    }
    if target.address().is_empty() {
        return Err(AdapterError::BadRequest(
            "whatsapp: target address is empty".into(),
        ));
    }
    Ok(())
}

fn unsupported(msg: &str) -> AdapterError {
    AdapterError::Unsupported(format!("whatsapp: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_types::{InboundEvent, MessageKind};
    use serde_json::json;
    use tokio::sync::mpsc;

    fn build_adapter() -> (Arc<WhatsAppAdapter>, mpsc::Receiver<InboundEvent>) {
        let (tx, rx) = mpsc::channel(8);
        let cfg = WhatsAppConfig::default();
        let ks = Keystore::default();
        let a = WhatsAppAdapter::new(cfg, ks, tx);
        (Arc::new(a), rx)
    }

    /// Build an adapter explicitly using the no-op [`StubBackend`], so
    /// tests can pin behaviour to the "no crypto primitives available"
    /// codepath even after [`WhatsAppAdapter::new`] switched to the
    /// real [`DalekBackend`] default.
    fn build_adapter_with_stub() -> (Arc<WhatsAppAdapter>, mpsc::Receiver<InboundEvent>) {
        let (tx, rx) = mpsc::channel(8);
        let a = WhatsAppAdapter::with_crypto_backend(
            WhatsAppConfig::default(),
            Keystore::default(),
            tx,
            Arc::new(crate::crypto::StubBackend),
        );
        (Arc::new(a), rx)
    }

    fn chat_message(text: &str) -> OutboundMessage {
        OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": text}),
            files: vec![],
        }
    }

    // ---- parse_platform_id ----

    #[test]
    fn parse_user_platform_id() {
        match parse_platform_id("user:15551234").unwrap() {
            SendTarget::User(s) => assert_eq!(s, "15551234"),
            SendTarget::Group(_) => panic!("expected User"),
        }
    }

    #[test]
    fn parse_group_platform_id() {
        match parse_platform_id("group:abc-def@g.us").unwrap() {
            SendTarget::Group(s) => assert_eq!(s, "abc-def@g.us"),
            SendTarget::User(_) => panic!("expected Group"),
        }
    }

    #[test]
    fn parse_empty_user_is_bad_request() {
        let err = parse_platform_id("user:").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn parse_empty_group_is_bad_request() {
        let err = parse_platform_id("group:").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn parse_unknown_prefix_is_bad_request() {
        let err = parse_platform_id("telegram:1").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn parse_naked_id_is_bad_request() {
        let err = parse_platform_id("just-some-id").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn send_target_address_returns_inner_string() {
        assert_eq!(SendTarget::User("u".into()).address(), "u");
        assert_eq!(SendTarget::Group("g".into()).address(), "g");
    }

    #[test]
    fn send_target_is_group_predicate() {
        assert!(SendTarget::Group("g".into()).is_group());
        assert!(!SendTarget::User("u".into()).is_group());
    }

    #[test]
    fn send_target_clone_eq_debug() {
        let a = SendTarget::User("u".into());
        let b = a.clone();
        assert_eq!(a, b);
        assert!(format!("{a:?}").contains("User"));
    }

    // ---- adapter trait surface ----

    #[tokio::test]
    async fn channel_type_is_whatsapp() {
        let (a, _rx) = build_adapter();
        assert_eq!(a.channel_type().as_str(), "whatsapp");
    }

    #[tokio::test]
    async fn supports_threads_is_false() {
        let (a, _rx) = build_adapter();
        assert!(!a.supports_threads());
    }

    #[tokio::test]
    async fn subscribe_accepts_user_target() {
        let (a, _rx) = build_adapter();
        a.subscribe("user:15551234", None).await.unwrap();
        a.subscribe("group:abc@g.us", Some("t")).await.unwrap();
    }

    #[tokio::test]
    async fn subscribe_rejects_bad_platform_id() {
        let (a, _rx) = build_adapter();
        let err = a.subscribe("nope", None).await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn set_typing_returns_unsupported_with_stub_backend() {
        let (a, _rx) = build_adapter_with_stub();
        let err = a.set_typing("user:1", None).await.unwrap_err();
        match err {
            AdapterError::Unsupported(m) => assert!(m.contains("CryptoBackend")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_typing_returns_unsupported_with_real_backend_too() {
        // set_typing does not currently exercise the backend at all;
        // the unconditional Unsupported survives the backend swap.
        let (a, _rx) = build_adapter();
        let err = a.set_typing("user:1", None).await.unwrap_err();
        assert!(matches!(err, AdapterError::Unsupported(_)));
    }

    #[tokio::test]
    async fn set_typing_bad_platform_id_is_bad_request() {
        let (a, _rx) = build_adapter();
        let err = a.set_typing("nope", None).await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn deliver_with_stub_backend_returns_unsupported() {
        let (a, _rx) = build_adapter_with_stub();
        let err = a
            .deliver("user:1", None, &chat_message("hi"))
            .await
            .unwrap_err();
        match err {
            AdapterError::Unsupported(m) => {
                assert!(m.contains("CryptoBackend"));
                assert!(
                    m.contains("stub is wired in"),
                    "stub-specific message expected, got {m:?}"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_with_real_backend_returns_unsupported_pipeline_message() {
        // With the real DalekBackend the primitives succeed, so the
        // adapter falls through to the "pipeline not implemented above
        // the CryptoBackend" branch instead of the stub-only message.
        let (a, _rx) = build_adapter();
        let err = a
            .deliver("user:1", None, &chat_message("hi"))
            .await
            .unwrap_err();
        match err {
            AdapterError::Unsupported(m) => {
                assert!(
                    m.contains("pipeline not yet implemented"),
                    "real-backend pipeline message expected, got {m:?}"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_propagates_bad_platform_id() {
        let (a, _rx) = build_adapter();
        let err = a
            .deliver("nope", None, &chat_message("hi"))
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn deliver_rejects_null_content() {
        let (a, _rx) = build_adapter();
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: serde_json::Value::Null,
            files: vec![],
        };
        let err = a.deliver("user:1", None, &msg).await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn deliver_rejects_array_content() {
        let (a, _rx) = build_adapter();
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!([1, 2, 3]),
            files: vec![],
        };
        let err = a.deliver("user:1", None, &msg).await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn deliver_accepts_string_content_shape() {
        // The adapter still returns Unsupported, but only after the
        // content-shape check passes — we know we got past validation
        // because the error is Unsupported rather than BadRequest.
        let (a, _rx) = build_adapter();
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!("hi"),
            files: vec![],
        };
        let err = a.deliver("user:1", None, &msg).await.unwrap_err();
        assert!(matches!(err, AdapterError::Unsupported(_)));
    }

    #[tokio::test]
    async fn open_dm_returns_synthetic_handle() {
        let (a, _rx) = build_adapter();
        let h = a.open_dm("15551112222").await.unwrap().unwrap();
        assert_eq!(h.platform_id, "user:15551112222");
        assert_eq!(h.user_id, "15551112222");
        assert_eq!(h.channel_type.as_str(), "whatsapp");
    }

    #[tokio::test]
    async fn open_dm_rejects_empty_user_id() {
        let (a, _rx) = build_adapter();
        let err = a.open_dm("").await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    // ---- constructors / accessors ----

    #[tokio::test]
    async fn new_uses_real_backend() {
        let (a, _rx) = build_adapter();
        let backend = a.crypto_backend();
        // We don't compare instances; the test for real-ness is that
        // primitives on the wired-in backend actually succeed.
        let kp = backend
            .generate_keypair()
            .expect("real backend must generate a keypair");
        assert_eq!(kp.private.len(), 32);
        assert_eq!(kp.public.len(), 32);
        let other = backend.generate_keypair().unwrap();
        let s1 = backend.dh(&kp.private, &other.public).unwrap();
        let s2 = backend.dh(&other.private, &kp.public).unwrap();
        assert_eq!(s1, s2);
    }

    #[tokio::test]
    async fn explicit_stub_backend_still_returns_not_implemented() {
        let (a, _rx) = build_adapter_with_stub();
        let backend = a.crypto_backend();
        let err = backend.generate_keypair().unwrap_err();
        assert!(matches!(err, crate::crypto::CryptoError::NotImplemented(_)));
    }

    #[tokio::test]
    async fn config_accessor_returns_config() {
        let (a, _rx) = build_adapter();
        assert_eq!(a.config().endpoint, WhatsAppConfig::default().endpoint);
    }

    #[tokio::test]
    async fn keystore_snapshot_returns_default_initially() {
        let (a, _rx) = build_adapter();
        let snap = a.keystore_snapshot().await;
        assert!(snap.is_empty());
    }

    #[tokio::test]
    async fn replace_keystore_updates_state() {
        let (a, _rx) = build_adapter();
        let ks = Keystore {
            device_id: "dev-test".into(),
            noise_key: "AA==".into(),
            ..Keystore::default()
        };
        a.replace_keystore(ks.clone()).await;
        assert_eq!(a.keystore_snapshot().await, ks);
    }

    #[tokio::test]
    async fn debug_format_renders() {
        let (a, _rx) = build_adapter();
        let s = format!("{a:?}");
        assert!(s.contains("WhatsAppAdapter"));
    }

    #[tokio::test]
    async fn with_crypto_backend_uses_provided_backend() {
        // Build a fresh adapter with the no-op StubBackend explicitly,
        // exercising the with_crypto_backend escape hatch.
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let a = WhatsAppAdapter::with_crypto_backend(
            WhatsAppConfig::default(),
            Keystore::default(),
            tx,
            Arc::new(crate::crypto::StubBackend),
        );
        let err = a
            .deliver("user:1", None, &chat_message("hi"))
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Unsupported(_)));
    }

    #[test]
    fn unsupported_helper_prefixes_with_whatsapp() {
        let err = unsupported("x");
        match err {
            AdapterError::Unsupported(m) => {
                assert!(m.starts_with("whatsapp:"));
                assert!(m.contains('x'));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn validate_outbound_rejects_null_content() {
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: serde_json::Value::Null,
            files: vec![],
        };
        let err =
            validate_outbound(&SendTarget::User("u".into()), &msg).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn validate_outbound_rejects_number_content() {
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!(7),
            files: vec![],
        };
        let err =
            validate_outbound(&SendTarget::User("u".into()), &msg).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn validate_outbound_rejects_empty_target() {
        // SendTarget::User("") wouldn't normally be reachable through
        // parse_platform_id, but we still defensive-check here.
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "hi"}),
            files: vec![],
        };
        let err = validate_outbound(&SendTarget::User(String::new()), &msg)
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn validate_outbound_accepts_object_content() {
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "hi"}),
            files: vec![],
        };
        validate_outbound(&SendTarget::User("u".into()), &msg).unwrap();
    }

    #[test]
    fn validate_outbound_accepts_string_content() {
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!("hi"),
            files: vec![],
        };
        validate_outbound(&SendTarget::Group("g".into()), &msg).unwrap();
    }
}
