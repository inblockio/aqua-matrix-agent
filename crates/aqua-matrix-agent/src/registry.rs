//! Fleet-registry entry written to the agent's own global account data under
//! the custom event type `io.inblock.aqua.registry`.
//!
//! This lets the fleet discover, from a single account's account data, which
//! role an agent plays, the systemd unit that supervises it, and when it was
//! last online — without a separate registry service. Writes are upserts of a
//! single global account-data event keyed by the custom type string.

use serde::Serialize;

/// Custom global account-data event type that holds the fleet-registry entry.
pub(crate) const REGISTRY_EVENT_TYPE: &str = "io.inblock.aqua.registry";

/// JSON content stored under [`REGISTRY_EVENT_TYPE`].
#[derive(Debug, Serialize)]
pub(crate) struct RegistryContent {
    /// Decentralized identifier of this agent.
    pub did: String,
    /// Matrix user ID of this agent.
    pub user_id: String,
    /// Logical role, e.g. "heartbeat", "claude-channel", "chat".
    pub role: String,
    /// systemd unit supervising this agent, if any. Omitted (not `null`) for
    /// ad-hoc runs with no supervising unit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub systemd_unit: Option<String>,
    /// Unix seconds when this entry was last written.
    pub last_online: u64,
    /// Crate version that wrote this entry.
    pub version: String,
}
