//! End-to-end integration: spin up the socket server against an in-memory
//! central DB and drive it via the `cclaw` Unix-socket client.

use copperclaw_cclaw::{Caller, CclawClient};
use copperclaw_db::central::CentralDb;
use copperclaw_host::socket::run_server;
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn ncl_client_groups_list_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_path = tmp.path().join("cclaw.sock");
    let central = CentralDb::open_in_memory().unwrap();
    let shutdown = CancellationToken::new();
    let server_path = socket_path.clone();
    let server_cancel = shutdown.clone();
    let server_central = central.clone();
    let server_task = tokio::spawn(async move {
        run_server(server_path, server_central, server_cancel)
            .await
            .unwrap();
    });

    for _ in 0..80 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(socket_path.exists(), "socket should be up");

    let client = CclawClient::connect(socket_path.clone());
    let data = client
        .call("groups.list", json!({}), Caller::Host)
        .await
        .unwrap();
    assert!(data.is_array());
    assert!(data.as_array().unwrap().is_empty());

    // Mutation via `Caller::Host` is allowed.
    let created = client
        .call(
            "groups.create",
            json!({"folder": "g", "name": "Greeter"}),
            Caller::Host,
        )
        .await
        .unwrap();
    assert!(created["id"].is_string());

    // The list reflects the new row.
    let data = client
        .call("groups.list", json!({}), Caller::Host)
        .await
        .unwrap();
    assert_eq!(data.as_array().unwrap().len(), 1);

    shutdown.cancel();
    server_task.await.unwrap();
}

#[tokio::test]
async fn ncl_client_agent_caller_blocked_on_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_path = tmp.path().join("cclaw.sock");
    let central = CentralDb::open_in_memory().unwrap();
    let shutdown = CancellationToken::new();
    let server_path = socket_path.clone();
    let server_cancel = shutdown.clone();
    let server_central = central.clone();
    let server_task = tokio::spawn(async move {
        run_server(server_path, server_central, server_cancel)
            .await
            .unwrap();
    });

    for _ in 0..80 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let client = CclawClient::connect(socket_path.clone());
    let result = client
        .call(
            "groups.create",
            json!({"folder": "g", "name": "n"}),
            Caller::Agent {
                session_id: copperclaw_types::SessionId::nil(),
                agent_group_id: copperclaw_types::AgentGroupId::nil(),
                messaging_group_id: None,
            },
        )
        .await;
    match result {
        Err(copperclaw_cclaw::ClientError::Remote(err)) => {
            assert_eq!(err.code, "permission_denied");
        }
        other => panic!("expected permission_denied, got {other:?}"),
    }

    shutdown.cancel();
    server_task.await.unwrap();
}
