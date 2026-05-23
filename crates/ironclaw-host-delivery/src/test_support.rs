//! Internal test helpers shared across module unit tests.

use crate::dispatch::{AdapterResolver, HostDispatcher};
use crate::service::{DeliveryService, FsSessionRoot, SessionPool, SessionRoot};
use chrono::Utc;
use dashmap::DashMap;
use ironclaw_channels_core::testing::MockAdapter;
use ironclaw_channels_core::ChannelAdapter;
use ironclaw_db::central::CentralDb;
use ironclaw_db::session::SessionPaths;
use ironclaw_db::tables::agent_groups::{create as create_ag, CreateAgentGroup};
use ironclaw_db::tables::messages_out::{insert as insert_msg, WriteOutbound};
use ironclaw_db::tables::sessions::{create as create_session, CreateSession};
use ironclaw_modules::DeliveryDispatcher;
use ironclaw_types::{
    AgentGroupId, ChannelType, ContainerStatus, MessageId, MessageKind, Session, SessionId,
    SessionStatus,
};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

/// In-memory-backed `SessionRoot` rooted at a tempdir path. Same shape as
/// `FsSessionRoot`, but the wrapping tempdir is held by the caller for the
/// duration of the test.
pub struct MockRoot {
    pub data_root: PathBuf,
}

impl MockRoot {
    pub fn new(data_root: PathBuf) -> Self {
        Self { data_root }
    }
}

impl SessionRoot for MockRoot {
    fn outbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<SessionPool, crate::error::DeliveryError> {
        let paths = SessionPaths::new(&self.data_root, *agent_group_id, *session_id);
        Ok(SessionPool::outbound(paths))
    }
    fn inbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<SessionPool, crate::error::DeliveryError> {
        let paths = SessionPaths::new(&self.data_root, *agent_group_id, *session_id);
        Ok(SessionPool::inbound(paths))
    }
}

/// Constructs a delivery service with:
/// - an in-memory central DB
/// - one `agent_group` and one `session` written to the central DB
/// - a tempfile-backed session root
/// - one `MockAdapter` registered as channel `"mock"`
pub async fn make_service() -> (
    Arc<DeliveryService>,
    Arc<tempfile::TempDir>,
    Session,
    Arc<MockAdapter>,
) {
    let tmp = Arc::new(tempfile::tempdir().unwrap());
    let central = CentralDb::open_in_memory().unwrap();
    let ag = create_ag(
        &central,
        CreateAgentGroup {
            name: "test".into(),
            folder: "test".into(),
            agent_provider: None,
        },
    )
    .unwrap();
    let sess_row = create_session(
        &central,
        CreateSession {
            agent_group_id: ag.id,
            messaging_group_id: None,
            thread_id: None,
            agent_provider: None,
            source_session_id: None,
        },
    )
    .unwrap();
    let mock = Arc::new(MockAdapter::new("mock"));
    let adapters: DashMap<ChannelType, Arc<dyn ChannelAdapter>> = DashMap::new();
    adapters.insert(ChannelType::new("mock"), mock.clone() as Arc<dyn ChannelAdapter>);

    // Resolver reads the same adapters map.
    let resolver_map = Arc::new(adapters.clone());
    let resolver: AdapterResolver = {
        let map = Arc::clone(&resolver_map);
        Arc::new(move |ct| map.get(ct).map(|r| r.clone()))
    };
    let dispatcher: Arc<dyn DeliveryDispatcher> = Arc::new(HostDispatcher::new(resolver));

    let root: Arc<dyn SessionRoot> = Arc::new(MockRoot::new(tmp.path().to_path_buf()));
    let service = DeliveryService::new(central, root, adapters, dispatcher);

    let session = Session {
        id: sess_row.id,
        agent_group_id: sess_row.agent_group_id,
        messaging_group_id: sess_row.messaging_group_id,
        thread_id: sess_row.thread_id.clone(),
        agent_provider: sess_row.agent_provider.clone(),
        source_session_id: None,
        status: SessionStatus::Active,
        container_status: ContainerStatus::Stopped,
        last_active: sess_row.last_active,
        created_at: sess_row.created_at,
    };
    (service, tmp, session, mock)
}

/// Insert a default chat row into the supplied outbound pool.
pub fn write_chat_row(pool: &SessionPool) {
    let row = WriteOutbound {
        id: MessageId::new(),
        in_reply_to: None,
        timestamp: Utc::now(),
        deliver_after: None,
        recurrence: None,
        kind: MessageKind::Chat,
        platform_id: Some("plat-1".into()),
        channel_type: Some(ChannelType::new("mock")),
        thread_id: None,
        content: json!({"text": "hi"}),
    };
    let conn = pool.connect().unwrap();
    insert_msg(&conn, &row).unwrap();
}

/// Build a placeholder session value for helper unit tests.
pub fn make_session() -> Session {
    Session {
        id: SessionId::new(),
        agent_group_id: AgentGroupId::new(),
        messaging_group_id: None,
        thread_id: None,
        agent_provider: None,
        source_session_id: None,
        status: SessionStatus::Active,
        container_status: ContainerStatus::Stopped,
        last_active: Utc::now(),
        created_at: Utc::now(),
    }
}

/// Exists so the `FsSessionRoot` import is reachable from sibling tests.
#[allow(dead_code)]
pub fn _touch_fs_root() {
    let _ = FsSessionRoot::new("/tmp/_unused");
}
