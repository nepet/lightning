//! LSPS2 JIT Channel Session State Machine
//!
//! This module defines the core types for the MPP-capable JIT channel
//! state machine. The state machine is pure (no I/O) and testable
//! in isolation.

use crate::core::tlv::TlvStream;
use crate::proto::lsps0::{DateTime, Msat, ShortChannelId};
use crate::proto::lsps2::OpeningFeeParams;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Txid;
use std::time::Instant;

// ============================================================================
// Core Types
// ============================================================================

/// Unique identifier for a JIT channel session.
/// This is the SCID returned from `lsps2.buy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub ShortChannelId);

impl From<ShortChannelId> for SessionId {
    fn from(scid: ShortChannelId) -> Self {
        SessionId(scid)
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session:{}", self.0)
    }
}

/// 32-byte channel identifier from CLN.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelId(pub [u8; 32]);

impl ChannelId {
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        if bytes.len() == 32 {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(bytes);
            Some(ChannelId(arr))
        } else {
            None
        }
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Display for ChannelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

/// A single HTLC part being collected for an MPP payment.
#[derive(Debug, Clone)]
pub struct HtlcPart {
    /// Unique ID for this HTLC from CLN
    pub htlc_id: u64,
    /// Amount of this part in millisatoshis
    pub amount_msat: Msat,
    /// Absolute block height at which this HTLC expires
    pub cltv_expiry: u32,
    /// Payment hash (same for all parts of an MPP payment)
    pub payment_hash: [u8; 32],
    /// When this part arrived (for timeout tracking)
    pub arrived_at: Instant,
    /// Original onion payload for forwarding
    pub onion_payload: TlvStream,
    /// Extra TLVs from the sender
    pub extra_tlvs: TlvStream,
    /// The incoming short channel ID
    pub in_channel: ShortChannelId,
}

impl HtlcPart {
    /// Create a new HTLC part.
    pub fn new(
        htlc_id: u64,
        amount_msat: Msat,
        cltv_expiry: u32,
        payment_hash: [u8; 32],
        onion_payload: TlvStream,
        extra_tlvs: TlvStream,
        in_channel: ShortChannelId,
    ) -> Self {
        Self {
            htlc_id,
            amount_msat,
            cltv_expiry,
            payment_hash,
            arrived_at: Instant::now(),
            onion_payload,
            extra_tlvs,
            in_channel,
        }
    }
}

/// Configuration for a session, derived from the `lsps2.buy` request.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// The client's node ID (who we're opening a channel to)
    pub client_node_id: PublicKey,
    /// Fee parameters from the buy request (includes `valid_until`)
    pub opening_fee_params: OpeningFeeParams,
    /// Target sum for MPP payments (None = single-part / no-MPP)
    pub expected_payment_size: Option<Msat>,
    /// When the session was created
    pub created_at: Instant,
}

impl SessionConfig {
    pub fn new(
        client_node_id: PublicKey,
        opening_fee_params: OpeningFeeParams,
        expected_payment_size: Option<Msat>,
    ) -> Self {
        Self {
            client_node_id,
            opening_fee_params,
            expected_payment_size,
            created_at: Instant::now(),
        }
    }

    /// Returns the valid_until deadline from the opening fee params.
    pub fn valid_until(&self) -> DateTime {
        self.opening_fee_params.valid_until
    }
}

// ============================================================================
// Session State
// ============================================================================

/// Which phase the session is in (for logging/events).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPhase {
    Collecting,
    Opening,
    AwaitingChannelReady,
    Forwarding,
    WaitingPreimage,
    AwaitingRetry,
    Settling,
    Done,
    Failed,
    Abandoned,
}

impl std::fmt::Display for SessionPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionPhase::Collecting => write!(f, "collecting"),
            SessionPhase::Opening => write!(f, "opening"),
            SessionPhase::AwaitingChannelReady => write!(f, "awaiting_channel_ready"),
            SessionPhase::Forwarding => write!(f, "forwarding"),
            SessionPhase::WaitingPreimage => write!(f, "waiting_preimage"),
            SessionPhase::AwaitingRetry => write!(f, "awaiting_retry"),
            SessionPhase::Settling => write!(f, "settling"),
            SessionPhase::Done => write!(f, "done"),
            SessionPhase::Failed => write!(f, "failed"),
            SessionPhase::Abandoned => write!(f, "abandoned"),
        }
    }
}

/// Reason for session failure (terminal state: Failed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureReason {
    /// Timed out waiting for all parts to arrive
    CollectTimeout,
    /// The `valid_until` deadline from opening_fee_params passed
    ValidUntilExpired,
    /// Too many HTLC parts received
    TooManyParts { count: usize, max: usize },
    /// CLTV expiry too close to current height - unsafe to hold
    UnsafeHold { min_cltv: u32, current_height: u32 },
    /// Client rejected the channel open
    ClientRejectedChannel { error: String },
    /// Client disconnected before funding_signed
    ClientDisconnected,
}

impl std::fmt::Display for FailureReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FailureReason::CollectTimeout => write!(f, "collect_timeout"),
            FailureReason::ValidUntilExpired => write!(f, "valid_until_expired"),
            FailureReason::TooManyParts { count, max } => {
                write!(f, "too_many_parts: {} > {}", count, max)
            }
            FailureReason::UnsafeHold {
                min_cltv,
                current_height,
            } => {
                write!(
                    f,
                    "unsafe_hold: min_cltv={}, height={}",
                    min_cltv, current_height
                )
            }
            FailureReason::ClientRejectedChannel { error } => {
                write!(f, "client_rejected_channel: {}", error)
            }
            FailureReason::ClientDisconnected => write!(f, "client_disconnected"),
        }
    }
}

/// Reason for session abandonment (terminal state: Abandoned).
/// Abandonment means a channel was negotiated but payment failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbandonReason {
    /// The `valid_until` deadline passed during retry
    ValidUntilExpired,
    /// Timed out waiting for retry parts
    CollectTimeout,
    /// CLTV expiry too close during retry
    UnsafeHold { min_cltv: u32, current_height: u32 },
}

impl std::fmt::Display for AbandonReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AbandonReason::ValidUntilExpired => write!(f, "valid_until_expired"),
            AbandonReason::CollectTimeout => write!(f, "collect_timeout"),
            AbandonReason::UnsafeHold {
                min_cltv,
                current_height,
            } => {
                write!(
                    f,
                    "unsafe_hold: min_cltv={}, height={}",
                    min_cltv, current_height
                )
            }
        }
    }
}

/// The core state machine for a JIT channel session.
///
/// Each variant contains the data needed for that specific phase.
/// Data moves between states as the session progresses.
#[derive(Debug, Clone)]
pub enum SessionState {
    /// Collecting HTLC parts until the expected sum is reached.
    Collecting {
        parts: Vec<HtlcPart>,
        first_part_at: Instant,
    },

    /// Channel funding in progress (fundchannel_start called).
    Opening { parts: Vec<HtlcPart> },

    /// Waiting for client to send channel_ready.
    AwaitingChannelReady {
        parts: Vec<HtlcPart>,
        channel_id: ChannelId,
        funding_txid: Txid,
        funding_outpoint: u32,
        funding_psbt: String,
    },

    /// Channel is ready, forwarding HTLCs into it.
    Forwarding {
        parts: Vec<HtlcPart>,
        channel_id: ChannelId,
        alias_scid: ShortChannelId,
        funding_psbt: String,
    },

    /// HTLCs forwarded, waiting for preimage from client.
    WaitingPreimage {
        parts: Vec<HtlcPart>,
        channel_id: ChannelId,
        alias_scid: ShortChannelId,
        funding_psbt: String,
        forwarded_at: Instant,
    },

    /// Client rejected payment, waiting for new HTLC parts.
    AwaitingRetry {
        parts: Vec<HtlcPart>,
        channel_id: ChannelId,
        alias_scid: ShortChannelId,
        funding_psbt: String,
        retry_count: u32,
        first_part_at: Instant,
    },

    /// Preimage received, broadcasting funding transaction.
    Settling {
        channel_id: ChannelId,
        preimage: [u8; 32],
        funding_psbt: String,
    },

    /// Terminal: Session completed successfully.
    Done {
        channel_id: ChannelId,
        funding_txid: Txid,
        preimage: [u8; 32],
    },

    /// Terminal: Session failed before channel was negotiated.
    Failed {
        reason: FailureReason,
        phase: SessionPhase,
    },

    /// Terminal: Session abandoned after channel negotiated but payment failed.
    Abandoned {
        reason: AbandonReason,
        channel_id: ChannelId,
        retry_count: u32,
    },
}

impl SessionState {
    /// Returns the phase of this state.
    pub fn phase(&self) -> SessionPhase {
        match self {
            SessionState::Collecting { .. } => SessionPhase::Collecting,
            SessionState::Opening { .. } => SessionPhase::Opening,
            SessionState::AwaitingChannelReady { .. } => SessionPhase::AwaitingChannelReady,
            SessionState::Forwarding { .. } => SessionPhase::Forwarding,
            SessionState::WaitingPreimage { .. } => SessionPhase::WaitingPreimage,
            SessionState::AwaitingRetry { .. } => SessionPhase::AwaitingRetry,
            SessionState::Settling { .. } => SessionPhase::Settling,
            SessionState::Done { .. } => SessionPhase::Done,
            SessionState::Failed { .. } => SessionPhase::Failed,
            SessionState::Abandoned { .. } => SessionPhase::Abandoned,
        }
    }

    /// Returns true if this is a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            SessionState::Done { .. }
                | SessionState::Failed { .. }
                | SessionState::Abandoned { .. }
        )
    }

    /// Returns the parts if the state has them.
    pub fn parts(&self) -> Option<&Vec<HtlcPart>> {
        match self {
            SessionState::Collecting { parts, .. } => Some(parts),
            SessionState::Opening { parts } => Some(parts),
            SessionState::AwaitingChannelReady { parts, .. } => Some(parts),
            SessionState::Forwarding { parts, .. } => Some(parts),
            SessionState::WaitingPreimage { parts, .. } => Some(parts),
            SessionState::AwaitingRetry { parts, .. } => Some(parts),
            _ => None,
        }
    }

    /// Returns the total amount of all parts in millisatoshis.
    pub fn parts_sum_msat(&self) -> u64 {
        self.parts()
            .map(|parts| parts.iter().map(|p| p.amount_msat.msat()).sum())
            .unwrap_or(0)
    }

    /// Returns the minimum CLTV expiry across all parts.
    pub fn min_cltv_expiry(&self) -> Option<u32> {
        self.parts()
            .and_then(|parts| parts.iter().map(|p| p.cltv_expiry).min())
    }
}

// ============================================================================
// Session Input
// ============================================================================

/// All possible inputs that can trigger state transitions.
#[derive(Debug, Clone)]
pub enum SessionInput {
    // ---- HTLC arrivals ----
    /// A new HTLC part arrived.
    PartArrived { part: HtlcPart },

    // ---- Timeouts (from background task) ----
    /// Collect timeout (90 seconds from first part).
    CollectTimeout,
    /// The valid_until deadline has passed.
    ValidUntilPassed,
    /// CLTV expiry too close to current height.
    UnsafeHold {
        min_cltv_expiry: u32,
        current_height: u32,
    },
    /// Too many HTLC parts received.
    TooManyParts { max_parts: usize },

    // ---- Channel negotiation ----
    /// Channel funding completed (fundchannel_complete returned).
    FundingSigned {
        channel_id: ChannelId,
        funding_txid: Txid,
        funding_outpoint: u32,
        funding_psbt: String,
    },
    /// Client sent channel_ready, channel is usable.
    ClientChannelReady { alias_scid: ShortChannelId },
    /// Client rejected the channel open.
    ClientRejectsChannel { error: String },
    /// Client disconnected.
    ClientDisconnected,

    // ---- HTLC forwarding ----
    /// All HTLCs have been forwarded and committed.
    ForwardsCommitted,

    // ---- Forward results ----
    /// Received the preimage from the client.
    PreimageReceived { preimage: [u8; 32] },
    /// Client rejected the payment (will retry).
    ClientRejectedPayment,

    // ---- Final step ----
    /// Funding transaction has been broadcast.
    FundingBroadcasted { txid: Txid },
}

// ============================================================================
// Session Event
// ============================================================================

/// Events emitted by the state machine for observability.
///
/// These events are consumed by an event emitter trait for logging,
/// metrics, or other telemetry purposes.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    // ---- Session lifecycle ----
    SessionCreated {
        session_id: SessionId,
        scid: ShortChannelId,
        client_node_id: PublicKey,
        payment_size_msat: Option<Msat>,
        valid_until: DateTime,
    },
    SessionCompleted {
        session_id: SessionId,
        duration_ms: u64,
        opening_fee_msat: u64,
        channel_id: ChannelId,
        funding_txid: Txid,
    },
    SessionFailed {
        session_id: SessionId,
        phase: SessionPhase,
        reason: String,
        duration_ms: u64,
    },
    SessionAbandoned {
        session_id: SessionId,
        phase: SessionPhase,
        reason: String,
        duration_ms: u64,
        retry_count: u32,
        channel_id: ChannelId,
    },

    // ---- HTLC events ----
    HtlcPartReceived {
        session_id: SessionId,
        htlc_id: u64,
        amount_msat: Msat,
        cltv_expiry: u32,
        payment_hash: [u8; 32],
        parts_count: usize,
        parts_sum_msat: u64,
    },
    HtlcPartReceivedRetry {
        session_id: SessionId,
        htlc_id: u64,
        amount_msat: Msat,
        retry_count: u32,
    },
    HtlcsForwarded {
        session_id: SessionId,
        htlc_count: usize,
        total_forwarded_msat: u64,
        fee_deducted_msat: u64,
    },
    HtlcsFulfilled {
        session_id: SessionId,
        payment_hash: [u8; 32],
        preimage: [u8; 32],
    },
    HtlcsFailedUpstream {
        session_id: SessionId,
        htlc_count: usize,
        error_code: String,
        reason: String,
    },
    HtlcsRejectedByClient {
        session_id: SessionId,
        payment_hash: [u8; 32],
        rejection_type: String,
        retry_count: u32,
    },

    // ---- Channel events ----
    ChannelOpenInitiated {
        session_id: SessionId,
        client_node_id: PublicKey,
        channel_size_sat: u64,
    },
    ChannelFundingSigned {
        session_id: SessionId,
        channel_id: ChannelId,
        funding_txid: Txid,
        funding_output_index: u32,
    },
    ChannelReady {
        session_id: SessionId,
        channel_id: ChannelId,
        alias_scid: ShortChannelId,
    },
    ChannelFundingBroadcast {
        session_id: SessionId,
        channel_id: ChannelId,
        funding_txid: Txid,
    },
    ChannelReleased {
        session_id: SessionId,
        channel_id: ChannelId,
        reason: String,
    },

    // ---- Timeout/safety events ----
    CollectTimeout {
        session_id: SessionId,
        elapsed_ms: u64,
        parts_count: usize,
        parts_sum_msat: u64,
    },
    CollectTimeoutRetry {
        session_id: SessionId,
        elapsed_ms: u64,
        retry_count: u32,
    },
    ValidUntilExpired {
        session_id: SessionId,
        phase: SessionPhase,
        valid_until: DateTime,
        current_time: DateTime,
    },
    UnsafeHoldDetected {
        session_id: SessionId,
        phase: SessionPhase,
        min_cltv_expiry: u32,
        current_height: u32,
        blocks_remaining: u32,
    },
    TooManyParts {
        session_id: SessionId,
        parts_count: usize,
        max_allowed: usize,
    },

    // ---- Security/anomaly events ----
    ClientRejectedChannel {
        session_id: SessionId,
        client_node_id: PublicKey,
        error_message: String,
    },
    ClientDisconnected {
        session_id: SessionId,
        client_node_id: PublicKey,
        phase: SessionPhase,
    },
}

// ============================================================================
// Session Struct
// ============================================================================

/// A JIT channel session, wrapping state with configuration.
#[derive(Debug, Clone)]
pub struct Session {
    /// Unique identifier for this session
    id: SessionId,
    /// Configuration derived from lsps2.buy request
    config: SessionConfig,
    /// Current state
    state: SessionState,
}

impl Session {
    /// Creates a new session in the Collecting state.
    pub fn new(id: SessionId, config: SessionConfig) -> Self {
        Self {
            id,
            config,
            state: SessionState::Collecting {
                parts: Vec::new(),
                first_part_at: Instant::now(),
            },
        }
    }

    /// Returns the session ID.
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// Returns a reference to the session configuration.
    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// Returns a reference to the current state.
    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Returns the current phase.
    pub fn phase(&self) -> SessionPhase {
        self.state.phase()
    }

    /// Returns true if the session is in a terminal state.
    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }

    /// Returns the duration since session creation.
    pub fn duration(&self) -> std::time::Duration {
        self.config.created_at.elapsed()
    }

    /// Returns the duration in milliseconds since session creation.
    pub fn duration_ms(&self) -> u64 {
        self.duration().as_millis() as u64
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
    fn test_session_id_display() {
        let scid = ShortChannelId::from(123456789u64);
        let session_id = SessionId::from(scid);
        let display = format!("{}", session_id);
        assert!(display.starts_with("session:"));
    }

    #[test]
    fn test_channel_id_from_slice() {
        let bytes = [1u8; 32];
        let channel_id = ChannelId::from_slice(&bytes).unwrap();
        assert_eq!(channel_id.as_bytes(), &bytes);

        // Too short
        assert!(ChannelId::from_slice(&[1u8; 31]).is_none());
        // Too long
        assert!(ChannelId::from_slice(&[1u8; 33]).is_none());
    }

    #[test]
    fn test_session_config_valid_until() {
        let config = SessionConfig::new(test_public_key(), test_opening_fee_params(), None);
        assert_eq!(
            config.valid_until(),
            Utc.with_ymd_and_hms(2100, 1, 1, 0, 0, 0).unwrap()
        );
    }

    #[test]
    fn test_session_starts_in_collecting() {
        let scid = ShortChannelId::from(123u64);
        let session_id = SessionId::from(scid);
        let config = SessionConfig::new(test_public_key(), test_opening_fee_params(), None);
        let session = Session::new(session_id, config);

        assert_eq!(session.phase(), SessionPhase::Collecting);
        assert!(!session.is_terminal());
    }

    #[test]
    fn test_session_state_is_terminal() {
        assert!(!SessionState::Collecting {
            parts: vec![],
            first_part_at: Instant::now()
        }
        .is_terminal());

        assert!(SessionState::Done {
            channel_id: ChannelId([0u8; 32]),
            funding_txid: "0000000000000000000000000000000000000000000000000000000000000000"
                .parse()
                .unwrap(),
            preimage: [0u8; 32],
        }
        .is_terminal());

        assert!(SessionState::Failed {
            reason: FailureReason::CollectTimeout,
            phase: SessionPhase::Collecting,
        }
        .is_terminal());

        assert!(SessionState::Abandoned {
            reason: AbandonReason::CollectTimeout,
            channel_id: ChannelId([0u8; 32]),
            retry_count: 1,
        }
        .is_terminal());
    }

    #[test]
    fn test_session_state_phase() {
        assert_eq!(
            SessionState::Collecting {
                parts: vec![],
                first_part_at: Instant::now()
            }
            .phase(),
            SessionPhase::Collecting
        );

        assert_eq!(
            SessionState::Opening { parts: vec![] }.phase(),
            SessionPhase::Opening
        );

        assert_eq!(
            SessionState::Failed {
                reason: FailureReason::CollectTimeout,
                phase: SessionPhase::Collecting,
            }
            .phase(),
            SessionPhase::Failed
        );
    }

    #[test]
    fn test_failure_reason_display() {
        assert_eq!(
            format!("{}", FailureReason::CollectTimeout),
            "collect_timeout"
        );
        assert_eq!(
            format!("{}", FailureReason::TooManyParts { count: 10, max: 5 }),
            "too_many_parts: 10 > 5"
        );
    }

    #[test]
    fn test_session_phase_display() {
        assert_eq!(format!("{}", SessionPhase::Collecting), "collecting");
        assert_eq!(
            format!("{}", SessionPhase::AwaitingChannelReady),
            "awaiting_channel_ready"
        );
    }
}
