//! Tool call images on demand (ADR 0070): clients that ask for it get them
//! with a `rho-image:` uri instead of their data, and fetch the data with
//! `_rho/image` when they show the image.

use agent_client_protocol::schema::v1::SessionId;
use agent_client_protocol::{JsonRpcRequest, JsonRpcResponse};
use serde::{Deserialize, Serialize};

/// The data of the image that `uri` stands for.
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[serde(rename_all = "camelCase")]
#[request(method = "_rho/image", response = ImageResponse)]
pub(super) struct ImageRequest {
    pub session_id: SessionId,
    pub uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
#[serde(rename_all = "camelCase")]
pub(super) struct ImageResponse {
    pub mime_type: String,
    pub data: String,
}
