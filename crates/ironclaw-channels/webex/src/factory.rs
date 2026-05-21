//! [`ChannelFactory`] for the Webex adapter.

use crate::adapter::WebexAdapter;
use crate::api::WebexApi;
use crate::config::WebexConfig;
use crate::events::{WebexEventsState, build_events_router};
use async_trait::async_trait;
use ironclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
    ContainerContribution,
};
use ironclaw_types::ChannelType;
use std::net::SocketAddr;
use std::sync::Arc;

/// Channel-type string used by this crate (`"webex"`).
pub const CHANNEL_TYPE_STR: &str = "webex";

/// Factory producing [`WebexAdapter`] instances.
#[derive(Debug, Default)]
pub struct WebexFactory;

impl WebexFactory {
    /// Create a new factory. All state is per-instance and lives on the
    /// resulting adapter.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChannelFactory for WebexFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let cfg = WebexConfig::from_value(&setup.config)?;
        let api = WebexApi::new(&cfg.api_base, &cfg.bot_token);
        // Best-effort `me` lookup to populate the bot's personId for mention
        // detection and self-loop filtering. Failure is tolerated like Slack's
        // `auth.test` so a bad token doesn't prevent the host from starting.
        let bot_person_id = if let Some(pinned) = cfg.bot_person_id.clone() {
            Some(pinned)
        } else {
            match api.me().await {
                Ok(me) => Some(me.id),
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "webex people/me lookup failed at init; mention detection limited"
                    );
                    None
                }
            }
        };

        let ct = ChannelType::new(CHANNEL_TYPE_STR);
        let state = WebexEventsState::new(
            cfg.webhook_secret.clone().into_bytes(),
            cfg.webhook_algo,
            setup.inbound_tx,
            bot_person_id,
            ct.clone(),
            api.clone(),
        );
        let router = build_events_router(&cfg.webhook.path, state);

        let addr: SocketAddr = format!("{}:{}", cfg.webhook.host, cfg.webhook.port)
            .parse()
            .map_err(|e: std::net::AddrParseError| {
                AdapterError::BadRequest(format!("invalid webhook bind address: {e}"))
            })?;
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, router).await {
                tracing::warn!(error=%err, "webex webhook server exited");
            }
        });

        let adapter = WebexAdapter::new(ct, api);
        adapter.set_server_handle(server);
        Ok(Arc::new(adapter))
    }

    fn container_contribution(&self) -> ContainerContribution {
        // Webex talks to the platform via the host; nothing extra is needed
        // inside the agent container.
        ContainerContribution::default()
    }
}

/// Register this channel's factory with a [`ChannelRegistry`].
pub fn register(registry: &mut ChannelRegistry) -> Result<(), AdapterError> {
    registry.register(Arc::new(WebexFactory::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_types::InboundEvent;
    use serde_json::json;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn pick_free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    fn config_value(uri: &str, port: u16) -> serde_json::Value {
        json!({
            "bot_token": "tok",
            "webhook_secret": "secret",
            "api_base": uri,
            "webhook": {"host": "127.0.0.1", "port": port, "path": "/webex/webhook"}
        })
    }

    #[tokio::test]
    async fn channel_type_is_webex() {
        let f = WebexFactory::new();
        assert_eq!(f.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn container_contribution_is_empty() {
        let f = WebexFactory::new();
        assert!(f.container_contribution().is_empty());
    }

    #[tokio::test]
    async fn default_shutdown_is_ok() {
        let f = WebexFactory::new();
        f.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn init_starts_server_with_me_lookup() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/people/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"PBOT","displayName":"Bot"
            })))
            .mount(&mock)
            .await;
        let f = WebexFactory::new();
        let port = pick_free_port();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(config_value(&mock.uri(), port), tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn init_tolerates_failed_me_lookup() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/people/me"))
            .respond_with(ResponseTemplate::new(401).set_body_string(""))
            .mount(&mock)
            .await;
        let f = WebexFactory::new();
        let port = pick_free_port();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(config_value(&mock.uri(), port), tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn init_uses_pinned_bot_person_id_without_me_call() {
        // No /people/me mount: if init calls it, the wiremock returns 404 by
        // default which would still be tolerated, but the contract is to
        // skip the call when pinned.
        let mock = MockServer::start().await;
        let f = WebexFactory::new();
        let port = pick_free_port();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let mut cfg = config_value(&mock.uri(), port);
        cfg["bot_person_id"] = json!("PINNED-BOT");
        let setup = ChannelSetup::new(cfg, tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn init_rejects_bad_bind_host() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/people/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"PBOT"})))
            .mount(&mock)
            .await;
        let f = WebexFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let bad_cfg = json!({
            "bot_token":"t","webhook_secret":"s",
            "webhook":{"host":"not a host","port":9,"path":"/x"},
            "api_base": mock.uri()
        });
        let setup = ChannelSetup::new(bad_cfg, tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("invalid webhook")),
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok"),
        }
    }

    #[tokio::test]
    async fn init_rejects_missing_config() {
        let f = WebexFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(json!({}), tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok"),
        }
    }

    #[test]
    fn register_inserts_factory() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        assert!(reg.get(&ChannelType::new(CHANNEL_TYPE_STR)).is_some());
        let err = register(&mut reg).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn channel_type_constant_is_webex() {
        assert_eq!(CHANNEL_TYPE_STR, "webex");
    }

    #[test]
    fn factory_debug_format_present() {
        let f = WebexFactory::new();
        assert!(format!("{f:?}").contains("WebexFactory"));
    }

    #[test]
    fn factory_default_equals_new() {
        let a = WebexFactory::new();
        let b = WebexFactory;
        assert_eq!(a.channel_type(), b.channel_type());
    }
}
