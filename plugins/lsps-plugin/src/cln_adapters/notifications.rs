//! CLN Notification Handlers for LSPS2 JIT Channel Sessions
//!
//! This module provides handlers for CLN event notifications that need to
//! trigger state machine transitions:
//!
//! - `channel_state_changed`: Detects when channel reaches CHANNELD_NORMAL
//! - `forward_event`: Detects preimage or payment rejection
//! - `disconnect`: Detects peer disconnection
//!
//! Each handler translates the notification into a `SessionInput` and applies
//! it to the relevant session via the `SessionManager`.

use bitcoin::secp256k1::PublicKey;
use cln_rpc::primitives::{ChannelState, ShortChannelId};
use serde::Deserialize;

use crate::core::lsps2::session::SessionInput;

// ============================================================================
// Custom Notification Types
// ============================================================================
// These notifications are not included in cln_rpc::notifications, so we
// define our own structs.

/// Forward event notification from CLN.
///
/// Sent when a forward's status changes (offered, settled, failed, local_failed).
/// Format matches `listforwards` output.
#[derive(Clone, Debug, Deserialize)]
pub struct ForwardEventNotification {
    /// The payment hash (64 hex chars)
    pub payment_hash: String,
    /// Incoming channel
    pub in_channel: ShortChannelId,
    /// Outgoing channel
    pub out_channel: ShortChannelId,
    /// Amount received from previous hop (millisatoshis)
    #[serde(default)]
    pub in_msat: u64,
    /// Amount sent to next hop (millisatoshis)
    #[serde(default)]
    pub out_msat: u64,
    /// Fee earned (millisatoshis)
    #[serde(default)]
    pub fee_msat: u64,
    /// Forward status
    pub status: ForwardStatus,
    /// When the HTLC was received
    #[serde(default)]
    pub received_time: f64,
    /// When the HTLC was resolved (only for settled/failed)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_time: Option<f64>,
    /// The preimage (only present when settled, 64 hex chars)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preimage: Option<String>,
    /// Failure code (only for local_failed)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failcode: Option<u16>,
    /// Failure reason (only for local_failed)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failreason: Option<String>,
}

/// Forward status values.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForwardStatus {
    /// HTLC has been offered to the next hop
    Offered,
    /// HTLC has been settled (preimage received)
    Settled,
    /// HTLC failed at a downstream hop
    Failed,
    /// HTLC failed locally
    LocalFailed,
}

/// Disconnect notification from CLN.
///
/// Sent when a peer disconnects.
#[derive(Clone, Debug, Deserialize)]
pub struct DisconnectNotification {
    /// The peer's node ID
    pub id: PublicKey,
}

/// Wrapper for parsing the forward_event notification envelope.
#[derive(Clone, Debug, Deserialize)]
pub struct ForwardEventWrapper {
    pub forward_event: ForwardEventNotification,
}

/// Wrapper for parsing the disconnect notification envelope.
#[derive(Clone, Debug, Deserialize)]
pub struct DisconnectWrapper {
    pub disconnect: DisconnectNotification,
}

// ============================================================================
// Notification Parsing Helpers
// ============================================================================

/// Parse a forward_event notification from JSON value.
pub fn parse_forward_event(value: &serde_json::Value) -> Option<ForwardEventNotification> {
    serde_json::from_value::<ForwardEventWrapper>(value.clone())
        .ok()
        .map(|w| w.forward_event)
}

/// Parse a disconnect notification from JSON value.
pub fn parse_disconnect(value: &serde_json::Value) -> Option<DisconnectNotification> {
    serde_json::from_value::<DisconnectWrapper>(value.clone())
        .ok()
        .map(|w| w.disconnect)
}

/// Parse a payment_hash string (64 hex chars) into [u8; 32].
pub fn parse_payment_hash(hex_str: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(hex_str).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Some(arr)
}

/// Parse a preimage string (64 hex chars) into [u8; 32].
pub fn parse_preimage(hex_str: &str) -> Option<[u8; 32]> {
    parse_payment_hash(hex_str) // Same format
}

/// Parse a channel_id from cln_rpc Sha256 (as byte array).
pub fn parse_channel_id(sha256: &cln_rpc::primitives::Sha256) -> [u8; 32] {
    use bitcoin::hashes::Hash;
    *sha256.as_byte_array()
}

// ============================================================================
// Session Input Mapping
// ============================================================================

/// Maps a channel_state_changed notification to a SessionInput.
///
/// Returns `Some(SessionInput)` if this notification should trigger a
/// state transition, `None` otherwise.
pub fn map_channel_state_changed(
    new_state: ChannelState,
    alias_scid: Option<ShortChannelId>,
) -> Option<SessionInput> {
    match new_state {
        ChannelState::CHANNELD_NORMAL => {
            // Channel is ready for forwarding
            // Use alias SCID if available, otherwise this won't work for unconfirmed channels
            alias_scid.map(|scid| SessionInput::ClientChannelReady { alias_scid: scid })
        }
        ChannelState::CHANNELD_SHUTTING_DOWN
        | ChannelState::CLOSINGD_SIGEXCHANGE
        | ChannelState::CLOSINGD_COMPLETE
        | ChannelState::AWAITING_UNILATERAL
        | ChannelState::FUNDING_SPEND_SEEN
        | ChannelState::ONCHAIN => {
            // Channel is closing or closed - treat as disconnect
            Some(SessionInput::ClientDisconnected)
        }
        _ => None, // Other states don't trigger transitions
    }
}

/// Maps a forward_event notification to a SessionInput.
///
/// Returns `Some(SessionInput)` if this forward event should trigger a
/// state transition, `None` otherwise.
pub fn map_forward_event(notif: &ForwardEventNotification) -> Option<SessionInput> {
    match notif.status {
        ForwardStatus::Settled => {
            // Need preimage to continue
            notif.preimage.as_ref().and_then(|p| {
                parse_preimage(p).map(|preimage| SessionInput::PreimageReceived { preimage })
            })
        }
        ForwardStatus::Failed | ForwardStatus::LocalFailed => {
            // Client rejected - will retry
            Some(SessionInput::ClientRejectedPayment)
        }
        ForwardStatus::Offered => {
            // Just offered, no action needed yet
            None
        }
    }
}

/// Maps a disconnect notification to a SessionInput.
///
/// Always returns `ClientDisconnected` since any disconnect should
/// trigger the session to handle it.
pub fn map_disconnect() -> SessionInput {
    SessionInput::ClientDisconnected
}

// ============================================================================
// Trait for State that has SessionManager
// ============================================================================

use crate::core::lsps2::manager::SessionManager;
use crate::core::lsps2::provider::{SessionEventEmitter, SessionOutputHandler};
use std::sync::Arc;

/// Trait for plugin state that provides access to a SessionManager.
///
/// This allows notification handlers to be generic over the plugin state type.
pub trait HasSessionManager {
    type EventEmitter: SessionEventEmitter;
    type OutputHandler: SessionOutputHandler;

    fn session_manager(&self) -> &Arc<SessionManager<Self::EventEmitter, Self::OutputHandler>>;
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_forward_event_settled() {
        let value = json!({
            "forward_event": {
                "payment_hash": "f5a6a059a25d1e329d9b094aeeec8c2191ca037d3f5b0662e21ae850debe8ea2",
                "in_channel": "103x2x1",
                "out_channel": "103x1x1",
                "in_msat": 100001001,
                "out_msat": 100000000,
                "fee_msat": 1001,
                "status": "settled",
                "received_time": 1560696342.368,
                "resolved_time": 1560696342.556,
                "preimage": "0000000000000000000000000000000000000000000000000000000000000001"
            }
        });

        let notif = parse_forward_event(&value).unwrap();
        assert_eq!(notif.status, ForwardStatus::Settled);
        assert!(notif.preimage.is_some());
        assert_eq!(notif.in_msat, 100001001);
        assert_eq!(notif.out_msat, 100000000);
    }

    #[test]
    fn test_parse_forward_event_failed() {
        let value = json!({
            "forward_event": {
                "payment_hash": "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                "in_channel": "103x2x1",
                "out_channel": "110x1x0",
                "in_msat": 100001001,
                "out_msat": 100000000,
                "fee_msat": 1001,
                "status": "local_failed",
                "failcode": 16392,
                "failreason": "WIRE_PERMANENT_CHANNEL_FAILURE",
                "received_time": 1560696343.052
            }
        });

        let notif = parse_forward_event(&value).unwrap();
        assert_eq!(notif.status, ForwardStatus::LocalFailed);
        assert_eq!(notif.failcode, Some(16392));
        assert!(notif.preimage.is_none());
    }

    #[test]
    fn test_parse_forward_event_offered() {
        let value = json!({
            "forward_event": {
                "payment_hash": "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
                "in_channel": "103x2x1",
                "out_channel": "103x1x1",
                "status": "offered",
                "received_time": 1560696342.368
            }
        });

        let notif = parse_forward_event(&value).unwrap();
        assert_eq!(notif.status, ForwardStatus::Offered);
    }

    #[test]
    fn test_parse_disconnect() {
        let value = json!({
            "disconnect": {
                "id": "02f6725f9c1c40333b67faea92fd211c183050f28df32cac3f9d69685fe9665432"
            }
        });

        let notif = parse_disconnect(&value).unwrap();
        assert_eq!(
            notif.id.to_string(),
            "02f6725f9c1c40333b67faea92fd211c183050f28df32cac3f9d69685fe9665432"
        );
    }

    #[test]
    fn test_parse_payment_hash() {
        let hash =
            parse_payment_hash("f5a6a059a25d1e329d9b094aeeec8c2191ca037d3f5b0662e21ae850debe8ea2");
        assert!(hash.is_some());
        let hash = hash.unwrap();
        assert_eq!(hash[0], 0xf5);
        assert_eq!(hash[31], 0xa2);
    }

    #[test]
    fn test_parse_payment_hash_invalid() {
        // Too short
        assert!(parse_payment_hash("f5a6").is_none());
        // Invalid hex
        assert!(parse_payment_hash("zzzz").is_none());
        // Too long
        assert!(parse_payment_hash(
            "f5a6a059a25d1e329d9b094aeeec8c2191ca037d3f5b0662e21ae850debe8ea2ff"
        )
        .is_none());
    }

    #[test]
    fn test_map_channel_state_normal() {
        let alias = "123x1x0".parse::<ShortChannelId>().unwrap();
        let input = map_channel_state_changed(ChannelState::CHANNELD_NORMAL, Some(alias));
        assert!(matches!(
            input,
            Some(SessionInput::ClientChannelReady { .. })
        ));
    }

    #[test]
    fn test_map_channel_state_normal_no_alias() {
        // Without alias, can't create the input
        let input = map_channel_state_changed(ChannelState::CHANNELD_NORMAL, None);
        assert!(input.is_none());
    }

    #[test]
    fn test_map_channel_state_closing() {
        let input = map_channel_state_changed(ChannelState::CHANNELD_SHUTTING_DOWN, None);
        assert!(matches!(input, Some(SessionInput::ClientDisconnected)));
    }

    #[test]
    fn test_map_channel_state_awaiting_lockin() {
        // Waiting for confirmations - no action
        let input = map_channel_state_changed(ChannelState::CHANNELD_AWAITING_LOCKIN, None);
        assert!(input.is_none());
    }

    #[test]
    fn test_map_forward_event_settled_with_preimage() {
        let notif = ForwardEventNotification {
            payment_hash: "f5a6a059a25d1e329d9b094aeeec8c2191ca037d3f5b0662e21ae850debe8ea2"
                .to_string(),
            in_channel: "103x2x1".parse().unwrap(),
            out_channel: "103x1x1".parse().unwrap(),
            in_msat: 100001001,
            out_msat: 100000000,
            fee_msat: 1001,
            status: ForwardStatus::Settled,
            received_time: 0.0,
            resolved_time: Some(0.0),
            preimage: Some(
                "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
            ),
            failcode: None,
            failreason: None,
        };

        let input = map_forward_event(&notif);
        assert!(matches!(input, Some(SessionInput::PreimageReceived { .. })));
    }

    #[test]
    fn test_map_forward_event_settled_no_preimage() {
        let notif = ForwardEventNotification {
            payment_hash: "f5a6a059a25d1e329d9b094aeeec8c2191ca037d3f5b0662e21ae850debe8ea2"
                .to_string(),
            in_channel: "103x2x1".parse().unwrap(),
            out_channel: "103x1x1".parse().unwrap(),
            in_msat: 0,
            out_msat: 0,
            fee_msat: 0,
            status: ForwardStatus::Settled,
            received_time: 0.0,
            resolved_time: None,
            preimage: None, // No preimage available
            failcode: None,
            failreason: None,
        };

        // Can't proceed without preimage
        let input = map_forward_event(&notif);
        assert!(input.is_none());
    }

    #[test]
    fn test_map_forward_event_failed() {
        let notif = ForwardEventNotification {
            payment_hash: "f5a6a059a25d1e329d9b094aeeec8c2191ca037d3f5b0662e21ae850debe8ea2"
                .to_string(),
            in_channel: "103x2x1".parse().unwrap(),
            out_channel: "103x1x1".parse().unwrap(),
            in_msat: 0,
            out_msat: 0,
            fee_msat: 0,
            status: ForwardStatus::Failed,
            received_time: 0.0,
            resolved_time: None,
            preimage: None,
            failcode: None,
            failreason: None,
        };

        let input = map_forward_event(&notif);
        assert!(matches!(input, Some(SessionInput::ClientRejectedPayment)));
    }

    #[test]
    fn test_map_forward_event_local_failed() {
        let notif = ForwardEventNotification {
            payment_hash: "f5a6a059a25d1e329d9b094aeeec8c2191ca037d3f5b0662e21ae850debe8ea2"
                .to_string(),
            in_channel: "103x2x1".parse().unwrap(),
            out_channel: "103x1x1".parse().unwrap(),
            in_msat: 0,
            out_msat: 0,
            fee_msat: 0,
            status: ForwardStatus::LocalFailed,
            received_time: 0.0,
            resolved_time: None,
            preimage: None,
            failcode: Some(16392),
            failreason: Some("WIRE_PERMANENT_CHANNEL_FAILURE".to_string()),
        };

        let input = map_forward_event(&notif);
        assert!(matches!(input, Some(SessionInput::ClientRejectedPayment)));
    }

    #[test]
    fn test_map_forward_event_offered() {
        let notif = ForwardEventNotification {
            payment_hash: "f5a6a059a25d1e329d9b094aeeec8c2191ca037d3f5b0662e21ae850debe8ea2"
                .to_string(),
            in_channel: "103x2x1".parse().unwrap(),
            out_channel: "103x1x1".parse().unwrap(),
            in_msat: 0,
            out_msat: 0,
            fee_msat: 0,
            status: ForwardStatus::Offered,
            received_time: 0.0,
            resolved_time: None,
            preimage: None,
            failcode: None,
            failreason: None,
        };

        // Offered doesn't trigger any action
        let input = map_forward_event(&notif);
        assert!(input.is_none());
    }

    #[test]
    fn test_map_disconnect() {
        let input = map_disconnect();
        assert!(matches!(input, SessionInput::ClientDisconnected));
    }
}
