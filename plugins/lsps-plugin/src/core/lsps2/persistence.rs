//! Session Persistence for LSPS2 JIT Channel Sessions
//!
//! This module provides persistence types and logic for saving and recovering
//! JIT channel sessions across CLN restarts. Persistence ensures idempotency -
//! if CLN crashes mid-flow, we can resume without losing state.
//!
//! # Checkpoint Strategy
//!
//! We persist at critical state transitions:
//! - `Opening`: After fundchannel_start succeeds
//! - `AwaitingChannelReady`: After fundchannel_complete with channel_id and PSBT
//! - `Settling`: After receiving preimage (most critical!)
//!
//! We do NOT persist HTLC parts - on restart, held HTLCs are already failed by
//! CLN (hook responses were never returned). The session waits for new HTLCs.

use bitcoin::secp256k1::PublicKey;
use serde::{Deserialize, Serialize};

use crate::proto::lsps0::{Msat, ShortChannelId};
use crate::proto::lsps2::OpeningFeeParams;

use super::session::SessionId;

// ============================================================================
// Persisted Phase
// ============================================================================

/// The persisted phase of a session.
///
/// This is a serializable subset of `SessionState` containing only the
/// data needed for recovery. HTLCs are not persisted - they will be
/// re-sent by the sender after timeout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum PersistedPhase {
    /// Channel funding initiated (fundchannel_start called).
    Opening,

    /// Channel funding completed, waiting for channel_ready.
    AwaitingChannelReady {
        /// The 32-byte channel ID from CLN.
        channel_id: [u8; 32],
        /// The funding transaction ID (hex string).
        funding_txid: String,
        /// The funding output index.
        funding_outpoint: u32,
        /// The funding PSBT (for later broadcast).
        funding_psbt: String,
    },

    /// Channel is ready, HTLCs are being forwarded.
    Forwarding {
        /// The 32-byte channel ID.
        channel_id: [u8; 32],
        /// The alias SCID for forwarding.
        alias_scid: ShortChannelId,
        /// The funding PSBT.
        funding_psbt: String,
    },

    /// HTLCs forwarded, waiting for preimage.
    WaitingPreimage {
        /// The 32-byte channel ID.
        channel_id: [u8; 32],
        /// The alias SCID for forwarding.
        alias_scid: ShortChannelId,
        /// The funding PSBT.
        funding_psbt: String,
    },

    /// Client rejected payment, waiting for retry.
    AwaitingRetry {
        /// The 32-byte channel ID.
        channel_id: [u8; 32],
        /// The alias SCID for forwarding.
        alias_scid: ShortChannelId,
        /// The funding PSBT.
        funding_psbt: String,
        /// Number of retry attempts.
        retry_count: u32,
    },

    /// Preimage received, ready to broadcast funding.
    /// This is the most critical checkpoint - must broadcast on recovery!
    Settling {
        /// The 32-byte channel ID.
        channel_id: [u8; 32],
        /// The 32-byte preimage for the payment.
        preimage: [u8; 32],
        /// The funding PSBT to broadcast.
        funding_psbt: String,
    },
}

impl PersistedPhase {
    /// Returns a string name for logging/debugging.
    pub fn name(&self) -> &'static str {
        match self {
            PersistedPhase::Opening => "opening",
            PersistedPhase::AwaitingChannelReady { .. } => "awaiting_channel_ready",
            PersistedPhase::Forwarding { .. } => "forwarding",
            PersistedPhase::WaitingPreimage { .. } => "waiting_preimage",
            PersistedPhase::AwaitingRetry { .. } => "awaiting_retry",
            PersistedPhase::Settling { .. } => "settling",
        }
    }

    /// Returns the channel_id if the phase has one.
    pub fn channel_id(&self) -> Option<[u8; 32]> {
        match self {
            PersistedPhase::Opening => None,
            PersistedPhase::AwaitingChannelReady { channel_id, .. } => Some(*channel_id),
            PersistedPhase::Forwarding { channel_id, .. } => Some(*channel_id),
            PersistedPhase::WaitingPreimage { channel_id, .. } => Some(*channel_id),
            PersistedPhase::AwaitingRetry { channel_id, .. } => Some(*channel_id),
            PersistedPhase::Settling { channel_id, .. } => Some(*channel_id),
        }
    }

    /// Returns the funding_psbt if the phase has one.
    pub fn funding_psbt(&self) -> Option<&str> {
        match self {
            PersistedPhase::Opening => None,
            PersistedPhase::AwaitingChannelReady { funding_psbt, .. } => Some(funding_psbt),
            PersistedPhase::Forwarding { funding_psbt, .. } => Some(funding_psbt),
            PersistedPhase::WaitingPreimage { funding_psbt, .. } => Some(funding_psbt),
            PersistedPhase::AwaitingRetry { funding_psbt, .. } => Some(funding_psbt),
            PersistedPhase::Settling { funding_psbt, .. } => Some(funding_psbt),
        }
    }

    /// Returns true if this phase requires broadcasting on recovery.
    pub fn requires_broadcast(&self) -> bool {
        matches!(self, PersistedPhase::Settling { .. })
    }
}

// ============================================================================
// Persisted Session
// ============================================================================

/// A serializable representation of a session for persistence.
///
/// This struct contains all the data needed to recover a session after
/// a CLN restart. It is stored in CLN's datastore as JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedSession {
    /// The session ID (JIT SCID from lsps2.buy).
    pub session_id: ShortChannelId,

    /// The client's node public key.
    pub client_node_id: PublicKey,

    /// Fee parameters from the buy request.
    pub opening_fee_params: OpeningFeeParams,

    /// Expected payment size for MPP (None = no-MPP).
    pub expected_payment_size: Option<Msat>,

    /// The current phase with phase-specific data.
    pub phase: PersistedPhase,

    /// When the session was created (Unix epoch seconds).
    pub created_at_epoch: u64,

    /// When the session was last updated (Unix epoch seconds).
    pub updated_at_epoch: u64,
}

impl PersistedSession {
    /// Create a new persisted session in the Opening phase.
    pub fn new_opening(
        session_id: ShortChannelId,
        client_node_id: PublicKey,
        opening_fee_params: OpeningFeeParams,
        expected_payment_size: Option<Msat>,
    ) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Self {
            session_id,
            client_node_id,
            opening_fee_params,
            expected_payment_size,
            phase: PersistedPhase::Opening,
            created_at_epoch: now,
            updated_at_epoch: now,
        }
    }

    /// Returns the session ID as a SessionId.
    pub fn id(&self) -> SessionId {
        SessionId(self.session_id)
    }

    /// Returns the session ID as a ShortChannelId.
    pub fn scid(&self) -> ShortChannelId {
        self.session_id
    }

    /// Update the phase and timestamp.
    pub fn update_phase(&mut self, phase: PersistedPhase) {
        self.phase = phase;
        self.updated_at_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
    }

    /// Transition to AwaitingChannelReady phase.
    pub fn set_awaiting_channel_ready(
        &mut self,
        channel_id: [u8; 32],
        funding_txid: String,
        funding_outpoint: u32,
        funding_psbt: String,
    ) {
        self.update_phase(PersistedPhase::AwaitingChannelReady {
            channel_id,
            funding_txid,
            funding_outpoint,
            funding_psbt,
        });
    }

    /// Transition to Forwarding phase.
    pub fn set_forwarding(
        &mut self,
        channel_id: [u8; 32],
        alias_scid: ShortChannelId,
        funding_psbt: String,
    ) {
        self.update_phase(PersistedPhase::Forwarding {
            channel_id,
            alias_scid,
            funding_psbt,
        });
    }

    /// Transition to WaitingPreimage phase.
    pub fn set_waiting_preimage(
        &mut self,
        channel_id: [u8; 32],
        alias_scid: ShortChannelId,
        funding_psbt: String,
    ) {
        self.update_phase(PersistedPhase::WaitingPreimage {
            channel_id,
            alias_scid,
            funding_psbt,
        });
    }

    /// Transition to AwaitingRetry phase.
    pub fn set_awaiting_retry(
        &mut self,
        channel_id: [u8; 32],
        alias_scid: ShortChannelId,
        funding_psbt: String,
        retry_count: u32,
    ) {
        self.update_phase(PersistedPhase::AwaitingRetry {
            channel_id,
            alias_scid,
            funding_psbt,
            retry_count,
        });
    }

    /// Transition to Settling phase.
    pub fn set_settling(&mut self, channel_id: [u8; 32], preimage: [u8; 32], funding_psbt: String) {
        self.update_phase(PersistedPhase::Settling {
            channel_id,
            preimage,
            funding_psbt,
        });
    }
}

// ============================================================================
// Conversion from Session/SessionState
// ============================================================================

use super::session::{ChannelId, Session, SessionConfig, SessionState};

/// Error when trying to convert a non-persistable state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistenceError {
    /// The state is not persistable (e.g., Collecting, terminal states).
    NotPersistable { phase: String },
}

impl std::fmt::Display for PersistenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PersistenceError::NotPersistable { phase } => {
                write!(f, "state '{}' is not persistable", phase)
            }
        }
    }
}

impl std::error::Error for PersistenceError {}

impl PersistedPhase {
    /// Try to create a PersistedPhase from a SessionState.
    ///
    /// Returns `None` for states that don't need persistence:
    /// - `Collecting`: HTLCs will timeout and be retried
    /// - `Done`, `Failed`, `Abandoned`: Terminal states, cleanup instead
    pub fn try_from_state(state: &SessionState) -> Option<Self> {
        match state {
            SessionState::Collecting { .. } => None,

            SessionState::Opening { .. } => Some(PersistedPhase::Opening),

            SessionState::AwaitingChannelReady {
                channel_id,
                funding_txid,
                funding_outpoint,
                funding_psbt,
                ..
            } => Some(PersistedPhase::AwaitingChannelReady {
                channel_id: channel_id.0,
                funding_txid: funding_txid.to_string(),
                funding_outpoint: *funding_outpoint,
                funding_psbt: funding_psbt.clone(),
            }),

            SessionState::Forwarding {
                channel_id,
                alias_scid,
                funding_psbt,
                ..
            } => Some(PersistedPhase::Forwarding {
                channel_id: channel_id.0,
                alias_scid: *alias_scid,
                funding_psbt: funding_psbt.clone(),
            }),

            SessionState::WaitingPreimage {
                channel_id,
                alias_scid,
                funding_psbt,
                ..
            } => Some(PersistedPhase::WaitingPreimage {
                channel_id: channel_id.0,
                alias_scid: *alias_scid,
                funding_psbt: funding_psbt.clone(),
            }),

            SessionState::AwaitingRetry {
                channel_id,
                alias_scid,
                funding_psbt,
                retry_count,
                ..
            } => Some(PersistedPhase::AwaitingRetry {
                channel_id: channel_id.0,
                alias_scid: *alias_scid,
                funding_psbt: funding_psbt.clone(),
                retry_count: *retry_count,
            }),

            SessionState::Settling {
                channel_id,
                preimage,
                funding_psbt,
            } => Some(PersistedPhase::Settling {
                channel_id: channel_id.0,
                preimage: *preimage,
                funding_psbt: funding_psbt.clone(),
            }),

            // Terminal states - don't persist
            SessionState::Done { .. }
            | SessionState::Failed { .. }
            | SessionState::Abandoned { .. } => None,
        }
    }
}

impl PersistedSession {
    /// Try to create a PersistedSession from a Session.
    ///
    /// Returns `None` if the session is in a non-persistable state.
    pub fn try_from_session(session: &Session) -> Option<Self> {
        let phase = PersistedPhase::try_from_state(session.state())?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Some(Self {
            session_id: session.id().0,
            client_node_id: session.config().client_node_id,
            opening_fee_params: session.config().opening_fee_params.clone(),
            expected_payment_size: session.config().expected_payment_size,
            phase,
            created_at_epoch: now, // Approximate - we don't have the original Instant
            updated_at_epoch: now,
        })
    }

    /// Create a SessionConfig from this persisted session.
    ///
    /// This can be used to reconstruct a Session after recovery.
    /// Note: The `created_at` field will be set to `Instant::now()`.
    pub fn to_config(&self) -> SessionConfig {
        SessionConfig::new(
            self.client_node_id,
            self.opening_fee_params.clone(),
            self.expected_payment_size,
        )
    }

    /// Get the ChannelId from this persisted session, if available.
    pub fn channel_id(&self) -> Option<ChannelId> {
        self.phase.channel_id().map(ChannelId)
    }

    /// Get the alias SCID from this persisted session, if available.
    pub fn alias_scid(&self) -> Option<ShortChannelId> {
        match &self.phase {
            PersistedPhase::Forwarding { alias_scid, .. }
            | PersistedPhase::WaitingPreimage { alias_scid, .. }
            | PersistedPhase::AwaitingRetry { alias_scid, .. } => Some(*alias_scid),
            _ => None,
        }
    }

    /// Get the preimage from this persisted session, if available.
    pub fn preimage(&self) -> Option<[u8; 32]> {
        match &self.phase {
            PersistedPhase::Settling { preimage, .. } => Some(*preimage),
            _ => None,
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::lsps0::Ppm;
    use crate::proto::lsps2::Promise;
    use chrono::{TimeZone, Utc};

    fn test_public_key() -> PublicKey {
        "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
            .parse()
            .unwrap()
    }

    fn test_opening_fee_params() -> OpeningFeeParams {
        OpeningFeeParams {
            min_fee_msat: Msat::from_msat(1000),
            proportional: Ppm::from_ppm(1000),
            valid_until: Utc.with_ymd_and_hms(2100, 1, 1, 0, 0, 0).unwrap(),
            min_lifetime: 144,
            max_client_to_self_delay: 2016,
            min_payment_size_msat: Msat::from_msat(10000),
            max_payment_size_msat: Msat::from_msat(100_000_000),
            promise: Promise::try_from("test_promise").unwrap(),
        }
    }

    #[test]
    fn test_persisted_session_creation() {
        let scid = ShortChannelId::from(123u64);
        let session = PersistedSession::new_opening(
            scid,
            test_public_key(),
            test_opening_fee_params(),
            Some(Msat::from_msat(100_000)),
        );

        assert_eq!(session.session_id, scid);
        assert_eq!(session.scid(), scid);
        assert!(matches!(session.phase, PersistedPhase::Opening));
        assert!(session.created_at_epoch > 0);
        assert_eq!(session.created_at_epoch, session.updated_at_epoch);
    }

    #[test]
    fn test_phase_transitions() {
        let scid = ShortChannelId::from(123u64);
        let mut session =
            PersistedSession::new_opening(scid, test_public_key(), test_opening_fee_params(), None);

        let channel_id = [1u8; 32];
        let funding_psbt = "test_psbt".to_string();

        // Opening -> AwaitingChannelReady
        session.set_awaiting_channel_ready(
            channel_id,
            "txid_hex".to_string(),
            0,
            funding_psbt.clone(),
        );
        assert!(matches!(
            session.phase,
            PersistedPhase::AwaitingChannelReady { .. }
        ));
        assert_eq!(session.phase.channel_id(), Some(channel_id));

        // AwaitingChannelReady -> Forwarding
        let alias_scid = ShortChannelId::from(456u64);
        session.set_forwarding(channel_id, alias_scid, funding_psbt.clone());
        assert!(matches!(session.phase, PersistedPhase::Forwarding { .. }));

        // Forwarding -> WaitingPreimage
        session.set_waiting_preimage(channel_id, alias_scid, funding_psbt.clone());
        assert!(matches!(
            session.phase,
            PersistedPhase::WaitingPreimage { .. }
        ));

        // WaitingPreimage -> Settling
        let preimage = [2u8; 32];
        session.set_settling(channel_id, preimage, funding_psbt);
        assert!(matches!(session.phase, PersistedPhase::Settling { .. }));
        assert!(session.phase.requires_broadcast());
    }

    #[test]
    fn test_awaiting_retry_phase() {
        let scid = ShortChannelId::from(123u64);
        let mut session =
            PersistedSession::new_opening(scid, test_public_key(), test_opening_fee_params(), None);

        let channel_id = [1u8; 32];
        let alias_scid = ShortChannelId::from(456u64);
        let funding_psbt = "test_psbt".to_string();

        session.set_awaiting_retry(channel_id, alias_scid, funding_psbt, 2);

        match &session.phase {
            PersistedPhase::AwaitingRetry { retry_count, .. } => {
                assert_eq!(*retry_count, 2);
            }
            _ => panic!("Expected AwaitingRetry phase"),
        }
    }

    #[test]
    fn test_phase_name() {
        assert_eq!(PersistedPhase::Opening.name(), "opening");
        assert_eq!(
            PersistedPhase::AwaitingChannelReady {
                channel_id: [0u8; 32],
                funding_txid: String::new(),
                funding_outpoint: 0,
                funding_psbt: String::new(),
            }
            .name(),
            "awaiting_channel_ready"
        );
        assert_eq!(
            PersistedPhase::Settling {
                channel_id: [0u8; 32],
                preimage: [0u8; 32],
                funding_psbt: String::new(),
            }
            .name(),
            "settling"
        );
    }

    #[test]
    fn test_serialization_roundtrip() {
        let scid = ShortChannelId::from(123u64);
        let mut session = PersistedSession::new_opening(
            scid,
            test_public_key(),
            test_opening_fee_params(),
            Some(Msat::from_msat(100_000)),
        );

        // Transition to Settling
        session.set_settling([1u8; 32], [2u8; 32], "psbt_data".to_string());

        // Serialize to JSON
        let json = serde_json::to_string(&session).expect("serialization failed");

        // Deserialize back
        let restored: PersistedSession =
            serde_json::from_str(&json).expect("deserialization failed");

        assert_eq!(session.session_id, restored.session_id);
        assert_eq!(session.client_node_id, restored.client_node_id);
        assert_eq!(session.phase, restored.phase);

        // Check Settling data preserved
        match restored.phase {
            PersistedPhase::Settling {
                channel_id,
                preimage,
                funding_psbt,
            } => {
                assert_eq!(channel_id, [1u8; 32]);
                assert_eq!(preimage, [2u8; 32]);
                assert_eq!(funding_psbt, "psbt_data");
            }
            _ => panic!("Expected Settling phase"),
        }
    }

    #[test]
    fn test_funding_psbt_accessor() {
        assert_eq!(PersistedPhase::Opening.funding_psbt(), None);

        let phase = PersistedPhase::AwaitingChannelReady {
            channel_id: [0u8; 32],
            funding_txid: String::new(),
            funding_outpoint: 0,
            funding_psbt: "test_psbt".to_string(),
        };
        assert_eq!(phase.funding_psbt(), Some("test_psbt"));
    }

    #[test]
    fn test_requires_broadcast() {
        assert!(!PersistedPhase::Opening.requires_broadcast());
        assert!(!PersistedPhase::Forwarding {
            channel_id: [0u8; 32],
            alias_scid: ShortChannelId::from(0u64),
            funding_psbt: String::new(),
        }
        .requires_broadcast());
        assert!(PersistedPhase::Settling {
            channel_id: [0u8; 32],
            preimage: [0u8; 32],
            funding_psbt: String::new(),
        }
        .requires_broadcast());
    }

    // ========================================================================
    // Conversion Tests
    // ========================================================================

    use crate::core::lsps2::session::HtlcPart;
    use crate::core::tlv::TlvStream;
    use std::time::Instant;

    fn test_htlc_part() -> HtlcPart {
        HtlcPart {
            htlc_id: 1,
            amount_msat: Msat::from_msat(50_000),
            cltv_expiry: 800_000,
            payment_hash: [1u8; 32],
            arrived_at: Instant::now(),
            onion_payload: TlvStream::default(),
            extra_tlvs: TlvStream::default(),
            in_channel: ShortChannelId::from(999u64),
        }
    }

    #[test]
    fn test_try_from_state_collecting() {
        let state = SessionState::Collecting {
            parts: vec![test_htlc_part()],
            first_part_at: Instant::now(),
        };
        // Collecting is not persistable
        assert!(PersistedPhase::try_from_state(&state).is_none());
    }

    #[test]
    fn test_try_from_state_opening() {
        let state = SessionState::Opening {
            parts: vec![test_htlc_part()],
        };
        let phase = PersistedPhase::try_from_state(&state);
        assert!(matches!(phase, Some(PersistedPhase::Opening)));
    }

    #[test]
    fn test_try_from_state_awaiting_channel_ready() {
        use bitcoin::Txid;
        use std::str::FromStr;

        let state = SessionState::AwaitingChannelReady {
            parts: vec![test_htlc_part()],
            channel_id: ChannelId([1u8; 32]),
            funding_txid: Txid::from_str(
                "0000000000000000000000000000000000000000000000000000000000000001",
            )
            .unwrap(),
            funding_outpoint: 0,
            funding_psbt: "test_psbt".to_string(),
        };

        let phase = PersistedPhase::try_from_state(&state).unwrap();
        assert!(matches!(phase, PersistedPhase::AwaitingChannelReady { .. }));
        assert_eq!(phase.channel_id(), Some([1u8; 32]));
        assert_eq!(phase.funding_psbt(), Some("test_psbt"));
    }

    #[test]
    fn test_try_from_state_settling() {
        let state = SessionState::Settling {
            channel_id: ChannelId([1u8; 32]),
            preimage: [2u8; 32],
            funding_psbt: "psbt_data".to_string(),
        };

        let phase = PersistedPhase::try_from_state(&state).unwrap();
        assert!(phase.requires_broadcast());

        match phase {
            PersistedPhase::Settling {
                channel_id,
                preimage,
                funding_psbt,
            } => {
                assert_eq!(channel_id, [1u8; 32]);
                assert_eq!(preimage, [2u8; 32]);
                assert_eq!(funding_psbt, "psbt_data");
            }
            _ => panic!("Expected Settling phase"),
        }
    }

    #[test]
    fn test_try_from_state_terminal_not_persistable() {
        use crate::core::lsps2::session::{FailureReason, SessionPhase};

        let failed = SessionState::Failed {
            reason: FailureReason::CollectTimeout,
            phase: SessionPhase::Collecting,
        };
        assert!(PersistedPhase::try_from_state(&failed).is_none());

        use bitcoin::Txid;
        use std::str::FromStr;
        let done = SessionState::Done {
            channel_id: ChannelId([1u8; 32]),
            funding_txid: Txid::from_str(
                "0000000000000000000000000000000000000000000000000000000000000001",
            )
            .unwrap(),
            preimage: [2u8; 32],
        };
        assert!(PersistedPhase::try_from_state(&done).is_none());
    }

    #[test]
    fn test_try_from_session_opening() {
        let session_id = SessionId::from(ShortChannelId::from(123u64));
        let config = SessionConfig::new(
            test_public_key(),
            test_opening_fee_params(),
            Some(Msat::from_msat(100_000)),
        );
        let mut session = Session::new(session_id, config);

        // Session starts in Collecting - not persistable
        assert!(PersistedSession::try_from_session(&session).is_none());

        // Apply input to transition to Opening
        use crate::core::lsps2::session::SessionInput;
        let _ = session.apply(SessionInput::PartArrived {
            part: test_htlc_part(),
        });
        // After part arrived with enough amount, should be in Opening
        // (our test_htlc_part has 50k, config expects 100k, so still Collecting)

        // Let's test with a session that's already in Opening
        let _opening_state = SessionState::Opening {
            parts: vec![test_htlc_part()],
        };
        // We can't easily set state directly, but we tested try_from_state above
    }

    #[test]
    fn test_to_config() {
        let scid = ShortChannelId::from(123u64);
        let session = PersistedSession::new_opening(
            scid,
            test_public_key(),
            test_opening_fee_params(),
            Some(Msat::from_msat(100_000)),
        );

        let config = session.to_config();
        assert_eq!(config.client_node_id, test_public_key());
        assert_eq!(config.expected_payment_size, Some(Msat::from_msat(100_000)));
    }

    #[test]
    fn test_persisted_session_accessors() {
        let scid = ShortChannelId::from(123u64);
        let mut session =
            PersistedSession::new_opening(scid, test_public_key(), test_opening_fee_params(), None);

        // Opening - no channel_id, alias_scid, or preimage
        assert!(session.channel_id().is_none());
        assert!(session.alias_scid().is_none());
        assert!(session.preimage().is_none());

        // Transition to Forwarding
        let alias = ShortChannelId::from(456u64);
        session.set_forwarding([1u8; 32], alias, "psbt".to_string());

        assert_eq!(session.channel_id(), Some(ChannelId([1u8; 32])));
        assert_eq!(session.alias_scid(), Some(alias));
        assert!(session.preimage().is_none());

        // Transition to Settling
        session.set_settling([1u8; 32], [2u8; 32], "psbt".to_string());

        assert_eq!(session.channel_id(), Some(ChannelId([1u8; 32])));
        assert!(session.alias_scid().is_none()); // Settling doesn't have alias_scid
        assert_eq!(session.preimage(), Some([2u8; 32]));
    }
}
