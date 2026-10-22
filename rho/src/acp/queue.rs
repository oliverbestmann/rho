//! Prompts queued in rho while a turn runs (ADR 0062), so they are sent
//! even when the client that queued them is gone, e.g. a phone with its
//! screen off. Zed keeps its own queue.

use agent_client_protocol::schema::v1::SessionId;
use agent_client_protocol::{JsonRpcRequest, JsonRpcResponse};
use serde::{Deserialize, Serialize};

/// Queues a prompt; it starts right away if the session is idle.
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[serde(rename_all = "camelCase")]
#[request(method = "_rho/queue", response = QueueResponse)]
pub(super) struct QueueRequest {
    pub session_id: SessionId,
    pub text: String,
}

/// Removes a queued prompt; one that started or is gone already is fine.
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[serde(rename_all = "camelCase")]
#[request(method = "_rho/unqueue", response = QueueResponse)]
pub(super) struct UnqueueRequest {
    pub session_id: SessionId,
    pub id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
pub(super) struct QueueResponse {}

/// A queued prompt; the clients get the queue in `_meta.rho.queue`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct Queued {
    pub id: u64,
    pub text: String,
}
