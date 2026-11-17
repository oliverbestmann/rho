//! `_rho/stats`: the monthly report of the web client's stats page (ADR 0075),
//! and `_rho/session_usage`: one session's requests, for its usage popup.

use agent_client_protocol::{JsonRpcRequest, JsonRpcResponse};
use serde::{Deserialize, Serialize};

use crate::store::{SessionUsage, Stats};

/// The client's days, so they follow its time zone.
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[serde(rename_all = "camelCase")]
#[request(method = "_rho/stats", response = StatsResponse)]
pub(super) struct StatsRequest {
    /// Bounds of each day, ascending, Unix milliseconds.
    pub days: Vec<i64>,
    /// `[from, to)` to compare with, e.g. the same part of the month before.
    pub compare: (i64, i64),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
pub(super) struct StatsResponse(pub serde_json::Value);

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[serde(rename_all = "camelCase")]
#[request(method = "_rho/session_usage", response = StatsResponse)]
pub(super) struct SessionUsageRequest {
    pub session_id: crate::store::SessionId,
}

impl From<SessionUsage> for StatsResponse {
    fn from(usage: SessionUsage) -> Self {
        Self(serde_json::to_value(usage).expect("usage is JSON"))
    }
}

impl From<Stats> for StatsResponse {
    fn from(stats: Stats) -> Self {
        Self(serde_json::to_value(stats).expect("stats are JSON"))
    }
}
