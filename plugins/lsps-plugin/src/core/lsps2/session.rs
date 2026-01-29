//! LSPS2 JIT Channel Session State Machine
//!
//! This module defines the core types for the MPP-capable JIT channel
//! state machine. The state machine is pure (no I/O) and testable
//! in isolation.

use crate::core::tlv::{TlvStream, TLV_FORWARD_AMT};
use crate::proto::lsps0::{DateTime, Msat, ShortChannelId};
use crate::proto::lsps2::{compute_opening_fee, OpeningFeeParams};
use bitcoin::secp256k1::PublicKey;
use bitcoin::Txid;
use chrono::Utc;
use log::debug;
use std::time::Instant;

/// TLV type for the opening fee deducted from the forwarded amount.
/// Per LSPS2 spec, this is type 65537.
pub const TLV_OPENING_FEE: u64 = 65537;

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
// Session Output
// ============================================================================

/// HTLC failure codes per LSPS2 spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCode {
    /// Used for: timeout, client disconnect before funding_signed
    TemporaryChannelFailure,
    /// Used for: valid_until passed, too many parts, client rejects channel
    UnknownNextPeer,
}

impl FailureCode {
    /// Returns the wire failure code string for CLN.
    pub fn as_str(&self) -> &'static str {
        match self {
            FailureCode::TemporaryChannelFailure => "temporary_channel_failure",
            FailureCode::UnknownNextPeer => "unknown_next_peer",
        }
    }
}

/// Instruction for forwarding a single HTLC.
#[derive(Debug, Clone)]
pub struct ForwardInstruction {
    /// The HTLC ID to forward
    pub htlc_id: u64,
    /// The channel_id to forward to (32-byte channel identifier)
    pub forward_to_channel_id: ChannelId,
    /// Modified onion payload with adjusted forward amount
    pub payload: TlvStream,
    /// Extra TLVs to add (includes opening fee TLV 65537)
    pub extra_tlvs: TlvStream,
}

/// Actions that the external handler must execute after a state transition.
#[derive(Debug, Clone)]
pub enum SessionOutput {
    /// Initiate channel opening with the client.
    OpenChannel {
        client_node_id: PublicKey,
        channel_size_sat: u64,
    },

    /// Forward HTLCs into the newly ready channel.
    ///
    /// The output handler should call `HtlcHolder::release_forward` with
    /// the given session_id and instructions.
    ForwardHtlcs {
        session_id: SessionId,
        instructions: Vec<ForwardInstruction>,
    },

    /// Fail HTLCs back upstream with the given failure code.
    ///
    /// The output handler should call `HtlcHolder::release_fail` with
    /// the given session_id and failure_code.
    FailHtlcs {
        session_id: SessionId,
        htlc_ids: Vec<u64>,
        failure_code: FailureCode,
    },

    /// Broadcast the withheld funding transaction.
    BroadcastFunding { psbt: String },

    /// Release/close the channel (for abandoned sessions).
    /// Calls close + unreserveinputs to clean up the withheld channel.
    ReleaseChannel {
        channel_id: ChannelId,
        funding_psbt: String,
    },
}

/// Result of applying an input to the session state machine.
#[derive(Debug, Clone, Default)]
pub struct ApplyResult {
    /// Events to emit for observability
    pub events: Vec<SessionEvent>,
    /// Outputs/commands to execute
    pub outputs: Vec<SessionOutput>,
}

impl ApplyResult {
    /// Creates an empty result (no events, no outputs).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Creates a result with events only.
    pub fn with_events(events: Vec<SessionEvent>) -> Self {
        Self {
            events,
            outputs: Vec::new(),
        }
    }

    /// Creates a result with events and outputs.
    pub fn new(events: Vec<SessionEvent>, outputs: Vec<SessionOutput>) -> Self {
        Self { events, outputs }
    }
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
        debug!(
            "Session::new: id={}, expected_payment_size={:?}",
            id, config.expected_payment_size
        );
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

    /// Returns the payment hash from collected parts, if any.
    pub fn payment_hash(&self) -> Option<[u8; 32]> {
        self.state
            .parts()
            .and_then(|parts| parts.first())
            .map(|p| p.payment_hash)
    }

    /// Returns the channel_id if the session has progressed past Opening.
    pub fn channel_id(&self) -> Option<ChannelId> {
        match &self.state {
            SessionState::AwaitingChannelReady { channel_id, .. }
            | SessionState::Forwarding { channel_id, .. }
            | SessionState::WaitingPreimage { channel_id, .. }
            | SessionState::AwaitingRetry { channel_id, .. }
            | SessionState::Settling { channel_id, .. }
            | SessionState::Done { channel_id, .. }
            | SessionState::Abandoned { channel_id, .. } => Some(*channel_id),
            SessionState::Collecting { .. }
            | SessionState::Opening { .. }
            | SessionState::Failed { .. } => None,
        }
    }

    /// Returns the client's node ID.
    pub fn client_node_id(&self) -> &PublicKey {
        &self.config.client_node_id
    }

    /// Returns the HTLC IDs from collected parts.
    pub fn htlc_ids(&self) -> Vec<u64> {
        self.state
            .parts()
            .map(|parts| parts.iter().map(|p| p.htlc_id).collect())
            .unwrap_or_default()
    }

    // ========================================================================
    // Helper Methods
    // ========================================================================

    /// Check if collected parts sum reaches expected payment size.
    fn check_sum_reached(&self, parts: &[HtlcPart]) -> bool {
        let sum: u64 = parts.iter().map(|p| p.amount_msat.msat()).sum();
        debug!(
            "check_sum_reached: sum={}, expected={:?}, parts_count={}",
            sum,
            self.config.expected_payment_size,
            parts.len()
        );
        match self.config.expected_payment_size {
            Some(expected) => {
                let result = sum >= expected.msat();
                debug!(
                    "check_sum_reached: {} >= {} = {}",
                    sum,
                    expected.msat(),
                    result
                );
                result
            }
            None => true, // no-MPP: first part triggers immediately
        }
    }

    /// Compute the opening fee for the given parts.
    fn compute_fee(&self, parts: &[HtlcPart]) -> u64 {
        let total_msat: u64 = parts.iter().map(|p| p.amount_msat.msat()).sum();
        compute_opening_fee(
            total_msat,
            self.config.opening_fee_params.min_fee_msat.msat(),
            self.config.opening_fee_params.proportional.ppm() as u64,
        )
        .unwrap_or(0)
    }

    /// Compute channel size based on parts and fee params.
    fn compute_channel_size(&self, parts: &[HtlcPart]) -> u64 {
        let total_msat: u64 = parts.iter().map(|p| p.amount_msat.msat()).sum();
        let opening_fee = self.compute_fee(parts);
        let receivable = total_msat.saturating_sub(opening_fee);
        // Convert to satoshis (floor division)
        receivable / 1000
    }

    /// Generate forward instructions for all parts.
    fn generate_forward_instructions(
        &self,
        parts: &[HtlcPart],
        channel_id: ChannelId,
    ) -> Vec<ForwardInstruction> {
        let total_msat: u64 = parts.iter().map(|p| p.amount_msat.msat()).sum();
        let opening_fee = self.compute_fee(parts);

        // Deduct fee from each part proportionally
        parts
            .iter()
            .map(|part| {
                // Calculate this part's share of the fee
                let part_fee = if total_msat > 0 {
                    (opening_fee as u128 * part.amount_msat.msat() as u128 / total_msat as u128)
                        as u64
                } else {
                    0
                };
                let forward_amt = part.amount_msat.msat().saturating_sub(part_fee);

                // Create modified payload with adjusted forward amount
                let mut payload = part.onion_payload.clone();
                payload.set_tu64(TLV_FORWARD_AMT, forward_amt);

                // Add opening fee TLV to extra_tlvs
                let mut extra_tlvs = part.extra_tlvs.clone();
                extra_tlvs.set_u64(TLV_OPENING_FEE, part_fee);

                ForwardInstruction {
                    htlc_id: part.htlc_id,
                    forward_to_channel_id: channel_id,
                    payload,
                    extra_tlvs,
                }
            })
            .collect()
    }

    /// Extract payment hash from parts (all should be the same).
    fn extract_payment_hash(parts: &[HtlcPart]) -> [u8; 32] {
        parts.first().map(|p| p.payment_hash).unwrap_or([0u8; 32])
    }

    /// Extract HTLC IDs from parts.
    fn extract_htlc_ids(parts: &[HtlcPart]) -> Vec<u64> {
        parts.iter().map(|p| p.htlc_id).collect()
    }

    // ========================================================================
    // State Transition Logic
    // ========================================================================

    /// Apply an input to the state machine, returning events and outputs.
    ///
    /// This is the core state machine logic. It is pure (no I/O) and
    /// deterministic given the same state and input.
    pub fn apply(&mut self, input: SessionInput) -> ApplyResult {
        // Take ownership of current state temporarily
        let current_state = std::mem::replace(
            &mut self.state,
            SessionState::Failed {
                reason: FailureReason::CollectTimeout,
                phase: SessionPhase::Collecting,
            },
        );

        let (new_state, result) = self.apply_to_state(current_state, input);
        self.state = new_state;
        result
    }

    /// Internal method that applies input to a state and returns the new state and result.
    fn apply_to_state(
        &self,
        state: SessionState,
        input: SessionInput,
    ) -> (SessionState, ApplyResult) {
        match (state, input) {
            // ================================================================
            // Collecting State Transitions
            // ================================================================
            (
                SessionState::Collecting {
                    mut parts,
                    first_part_at,
                },
                SessionInput::PartArrived { part },
            ) => {
                parts.push(part.clone());
                let parts_count = parts.len();
                let parts_sum = parts.iter().map(|p| p.amount_msat.msat()).sum();

                if self.check_sum_reached(&parts) {
                    // Sum reached -> transition to Opening
                    let channel_size_sat = self.compute_channel_size(&parts);
                    let events = vec![SessionEvent::ChannelOpenInitiated {
                        session_id: self.id,
                        client_node_id: self.config.client_node_id,
                        channel_size_sat,
                    }];
                    let outputs = vec![SessionOutput::OpenChannel {
                        client_node_id: self.config.client_node_id,
                        channel_size_sat,
                    }];
                    (
                        SessionState::Opening { parts },
                        ApplyResult::new(events, outputs),
                    )
                } else {
                    // Still collecting
                    let events = vec![SessionEvent::HtlcPartReceived {
                        session_id: self.id,
                        htlc_id: part.htlc_id,
                        amount_msat: part.amount_msat,
                        cltv_expiry: part.cltv_expiry,
                        payment_hash: part.payment_hash,
                        parts_count,
                        parts_sum_msat: parts_sum,
                    }];
                    (
                        SessionState::Collecting {
                            parts,
                            first_part_at,
                        },
                        ApplyResult::with_events(events),
                    )
                }
            }

            (
                SessionState::Collecting {
                    parts,
                    first_part_at,
                },
                SessionInput::CollectTimeout,
            ) => {
                let elapsed_ms = first_part_at.elapsed().as_millis() as u64;
                let parts_count = parts.len();
                let parts_sum = parts.iter().map(|p| p.amount_msat.msat()).sum();
                let htlc_ids = Self::extract_htlc_ids(&parts);

                let events = vec![
                    SessionEvent::CollectTimeout {
                        session_id: self.id,
                        elapsed_ms,
                        parts_count,
                        parts_sum_msat: parts_sum,
                    },
                    SessionEvent::HtlcsFailedUpstream {
                        session_id: self.id,
                        htlc_count: parts_count,
                        error_code: FailureCode::TemporaryChannelFailure.as_str().to_string(),
                        reason: "collect_timeout".to_string(),
                    },
                ];
                let outputs = vec![SessionOutput::FailHtlcs {
                    session_id: self.id,
                    htlc_ids,
                    failure_code: FailureCode::TemporaryChannelFailure,
                }];

                (
                    SessionState::Failed {
                        reason: FailureReason::CollectTimeout,
                        phase: SessionPhase::Collecting,
                    },
                    ApplyResult::new(events, outputs),
                )
            }

            (SessionState::Collecting { parts, .. }, SessionInput::ValidUntilPassed) => {
                let htlc_ids = Self::extract_htlc_ids(&parts);
                let events = vec![
                    SessionEvent::ValidUntilExpired {
                        session_id: self.id,
                        phase: SessionPhase::Collecting,
                        valid_until: self.config.valid_until(),
                        current_time: Utc::now(),
                    },
                    SessionEvent::HtlcsFailedUpstream {
                        session_id: self.id,
                        htlc_count: parts.len(),
                        error_code: FailureCode::UnknownNextPeer.as_str().to_string(),
                        reason: "valid_until_expired".to_string(),
                    },
                ];
                let outputs = vec![SessionOutput::FailHtlcs {
                    session_id: self.id,
                    htlc_ids,
                    failure_code: FailureCode::UnknownNextPeer,
                }];

                (
                    SessionState::Failed {
                        reason: FailureReason::ValidUntilExpired,
                        phase: SessionPhase::Collecting,
                    },
                    ApplyResult::new(events, outputs),
                )
            }

            (SessionState::Collecting { parts, .. }, SessionInput::TooManyParts { max_parts }) => {
                let htlc_ids = Self::extract_htlc_ids(&parts);
                let events = vec![
                    SessionEvent::TooManyParts {
                        session_id: self.id,
                        parts_count: parts.len(),
                        max_allowed: max_parts,
                    },
                    SessionEvent::HtlcsFailedUpstream {
                        session_id: self.id,
                        htlc_count: parts.len(),
                        error_code: FailureCode::UnknownNextPeer.as_str().to_string(),
                        reason: "too_many_parts".to_string(),
                    },
                ];
                let outputs = vec![SessionOutput::FailHtlcs {
                    session_id: self.id,
                    htlc_ids,
                    failure_code: FailureCode::UnknownNextPeer,
                }];

                (
                    SessionState::Failed {
                        reason: FailureReason::TooManyParts {
                            count: parts.len(),
                            max: max_parts,
                        },
                        phase: SessionPhase::Collecting,
                    },
                    ApplyResult::new(events, outputs),
                )
            }

            (
                SessionState::Collecting { parts, .. },
                SessionInput::UnsafeHold {
                    min_cltv_expiry,
                    current_height,
                },
            ) => {
                let htlc_ids = Self::extract_htlc_ids(&parts);
                let blocks_remaining = min_cltv_expiry.saturating_sub(current_height);
                let events = vec![
                    SessionEvent::UnsafeHoldDetected {
                        session_id: self.id,
                        phase: SessionPhase::Collecting,
                        min_cltv_expiry,
                        current_height,
                        blocks_remaining,
                    },
                    SessionEvent::HtlcsFailedUpstream {
                        session_id: self.id,
                        htlc_count: parts.len(),
                        error_code: FailureCode::TemporaryChannelFailure.as_str().to_string(),
                        reason: "unsafe_hold".to_string(),
                    },
                ];
                let outputs = vec![SessionOutput::FailHtlcs {
                    session_id: self.id,
                    htlc_ids,
                    failure_code: FailureCode::TemporaryChannelFailure,
                }];

                (
                    SessionState::Failed {
                        reason: FailureReason::UnsafeHold {
                            min_cltv: min_cltv_expiry,
                            current_height,
                        },
                        phase: SessionPhase::Collecting,
                    },
                    ApplyResult::new(events, outputs),
                )
            }

            // ================================================================
            // Opening State Transitions
            // ================================================================
            (
                SessionState::Opening { parts },
                SessionInput::FundingSigned {
                    channel_id,
                    funding_txid,
                    funding_outpoint,
                    funding_psbt,
                },
            ) => {
                let events = vec![SessionEvent::ChannelFundingSigned {
                    session_id: self.id,
                    channel_id,
                    funding_txid,
                    funding_output_index: funding_outpoint,
                }];

                (
                    SessionState::AwaitingChannelReady {
                        parts,
                        channel_id,
                        funding_txid,
                        funding_outpoint,
                        funding_psbt,
                    },
                    ApplyResult::with_events(events),
                )
            }

            (SessionState::Opening { parts }, SessionInput::ClientRejectsChannel { error }) => {
                let htlc_ids = Self::extract_htlc_ids(&parts);
                let events = vec![
                    SessionEvent::ClientRejectedChannel {
                        session_id: self.id,
                        client_node_id: self.config.client_node_id,
                        error_message: error.clone(),
                    },
                    SessionEvent::HtlcsFailedUpstream {
                        session_id: self.id,
                        htlc_count: parts.len(),
                        error_code: FailureCode::UnknownNextPeer.as_str().to_string(),
                        reason: "client_rejected_channel".to_string(),
                    },
                ];
                let outputs = vec![SessionOutput::FailHtlcs {
                    session_id: self.id,
                    htlc_ids,
                    failure_code: FailureCode::UnknownNextPeer,
                }];

                (
                    SessionState::Failed {
                        reason: FailureReason::ClientRejectedChannel { error },
                        phase: SessionPhase::Opening,
                    },
                    ApplyResult::new(events, outputs),
                )
            }

            (SessionState::Opening { parts }, SessionInput::ClientDisconnected) => {
                let htlc_ids = Self::extract_htlc_ids(&parts);
                let events = vec![
                    SessionEvent::ClientDisconnected {
                        session_id: self.id,
                        client_node_id: self.config.client_node_id,
                        phase: SessionPhase::Opening,
                    },
                    SessionEvent::HtlcsFailedUpstream {
                        session_id: self.id,
                        htlc_count: parts.len(),
                        error_code: FailureCode::TemporaryChannelFailure.as_str().to_string(),
                        reason: "client_disconnected".to_string(),
                    },
                ];
                let outputs = vec![SessionOutput::FailHtlcs {
                    session_id: self.id,
                    htlc_ids,
                    failure_code: FailureCode::TemporaryChannelFailure,
                }];

                (
                    SessionState::Failed {
                        reason: FailureReason::ClientDisconnected,
                        phase: SessionPhase::Opening,
                    },
                    ApplyResult::new(events, outputs),
                )
            }

            (
                SessionState::Opening { parts },
                SessionInput::UnsafeHold {
                    min_cltv_expiry,
                    current_height,
                },
            ) => {
                let htlc_ids = Self::extract_htlc_ids(&parts);
                let blocks_remaining = min_cltv_expiry.saturating_sub(current_height);
                let events = vec![
                    SessionEvent::UnsafeHoldDetected {
                        session_id: self.id,
                        phase: SessionPhase::Opening,
                        min_cltv_expiry,
                        current_height,
                        blocks_remaining,
                    },
                    SessionEvent::HtlcsFailedUpstream {
                        session_id: self.id,
                        htlc_count: parts.len(),
                        error_code: FailureCode::TemporaryChannelFailure.as_str().to_string(),
                        reason: "unsafe_hold".to_string(),
                    },
                ];
                let outputs = vec![SessionOutput::FailHtlcs {
                    session_id: self.id,
                    htlc_ids,
                    failure_code: FailureCode::TemporaryChannelFailure,
                }];

                (
                    SessionState::Failed {
                        reason: FailureReason::UnsafeHold {
                            min_cltv: min_cltv_expiry,
                            current_height,
                        },
                        phase: SessionPhase::Opening,
                    },
                    ApplyResult::new(events, outputs),
                )
            }

            // ================================================================
            // AwaitingChannelReady State Transitions
            // ================================================================
            (
                SessionState::AwaitingChannelReady {
                    parts,
                    channel_id,
                    funding_psbt,
                    ..
                },
                SessionInput::ClientChannelReady { alias_scid },
            ) => {
                let instructions = self.generate_forward_instructions(&parts, channel_id);
                let events = vec![SessionEvent::ChannelReady {
                    session_id: self.id,
                    channel_id,
                    alias_scid,
                }];
                let outputs = vec![SessionOutput::ForwardHtlcs {
                    session_id: self.id,
                    instructions,
                }];

                (
                    SessionState::Forwarding {
                        parts,
                        channel_id,
                        alias_scid,
                        funding_psbt,
                    },
                    ApplyResult::new(events, outputs),
                )
            }

            (
                SessionState::AwaitingChannelReady { parts, .. },
                SessionInput::ClientDisconnected,
            ) => {
                let htlc_ids = Self::extract_htlc_ids(&parts);
                let events = vec![
                    SessionEvent::ClientDisconnected {
                        session_id: self.id,
                        client_node_id: self.config.client_node_id,
                        phase: SessionPhase::AwaitingChannelReady,
                    },
                    SessionEvent::HtlcsFailedUpstream {
                        session_id: self.id,
                        htlc_count: parts.len(),
                        error_code: FailureCode::TemporaryChannelFailure.as_str().to_string(),
                        reason: "client_disconnected".to_string(),
                    },
                ];
                let outputs = vec![SessionOutput::FailHtlcs {
                    session_id: self.id,
                    htlc_ids,
                    failure_code: FailureCode::TemporaryChannelFailure,
                }];

                (
                    SessionState::Failed {
                        reason: FailureReason::ClientDisconnected,
                        phase: SessionPhase::AwaitingChannelReady,
                    },
                    ApplyResult::new(events, outputs),
                )
            }

            (
                SessionState::AwaitingChannelReady { parts, .. },
                SessionInput::UnsafeHold {
                    min_cltv_expiry,
                    current_height,
                },
            ) => {
                let htlc_ids = Self::extract_htlc_ids(&parts);
                let blocks_remaining = min_cltv_expiry.saturating_sub(current_height);
                let events = vec![
                    SessionEvent::UnsafeHoldDetected {
                        session_id: self.id,
                        phase: SessionPhase::AwaitingChannelReady,
                        min_cltv_expiry,
                        current_height,
                        blocks_remaining,
                    },
                    SessionEvent::HtlcsFailedUpstream {
                        session_id: self.id,
                        htlc_count: parts.len(),
                        error_code: FailureCode::TemporaryChannelFailure.as_str().to_string(),
                        reason: "unsafe_hold".to_string(),
                    },
                ];
                let outputs = vec![SessionOutput::FailHtlcs {
                    session_id: self.id,
                    htlc_ids,
                    failure_code: FailureCode::TemporaryChannelFailure,
                }];

                (
                    SessionState::Failed {
                        reason: FailureReason::UnsafeHold {
                            min_cltv: min_cltv_expiry,
                            current_height,
                        },
                        phase: SessionPhase::AwaitingChannelReady,
                    },
                    ApplyResult::new(events, outputs),
                )
            }

            // ================================================================
            // Forwarding State Transitions
            // ================================================================
            (
                SessionState::Forwarding {
                    parts,
                    channel_id,
                    alias_scid,
                    funding_psbt,
                },
                SessionInput::ForwardsCommitted,
            ) => {
                let total_msat: u64 = parts.iter().map(|p| p.amount_msat.msat()).sum();
                let fee = self.compute_fee(&parts);
                let events = vec![SessionEvent::HtlcsForwarded {
                    session_id: self.id,
                    htlc_count: parts.len(),
                    total_forwarded_msat: total_msat.saturating_sub(fee),
                    fee_deducted_msat: fee,
                }];

                (
                    SessionState::WaitingPreimage {
                        parts,
                        channel_id,
                        alias_scid,
                        funding_psbt,
                        forwarded_at: Instant::now(),
                    },
                    ApplyResult::with_events(events),
                )
            }

            (
                SessionState::Forwarding { parts, .. },
                SessionInput::UnsafeHold {
                    min_cltv_expiry,
                    current_height,
                },
            ) => {
                let htlc_ids = Self::extract_htlc_ids(&parts);
                let blocks_remaining = min_cltv_expiry.saturating_sub(current_height);
                let events = vec![
                    SessionEvent::UnsafeHoldDetected {
                        session_id: self.id,
                        phase: SessionPhase::Forwarding,
                        min_cltv_expiry,
                        current_height,
                        blocks_remaining,
                    },
                    SessionEvent::HtlcsFailedUpstream {
                        session_id: self.id,
                        htlc_count: parts.len(),
                        error_code: FailureCode::TemporaryChannelFailure.as_str().to_string(),
                        reason: "unsafe_hold".to_string(),
                    },
                ];
                let outputs = vec![SessionOutput::FailHtlcs {
                    session_id: self.id,
                    htlc_ids,
                    failure_code: FailureCode::TemporaryChannelFailure,
                }];

                (
                    SessionState::Failed {
                        reason: FailureReason::UnsafeHold {
                            min_cltv: min_cltv_expiry,
                            current_height,
                        },
                        phase: SessionPhase::Forwarding,
                    },
                    ApplyResult::new(events, outputs),
                )
            }

            // ================================================================
            // WaitingPreimage State Transitions
            // ================================================================
            (
                SessionState::WaitingPreimage {
                    parts,
                    channel_id,
                    funding_psbt,
                    ..
                },
                SessionInput::PreimageReceived { preimage },
            ) => {
                let payment_hash = Self::extract_payment_hash(&parts);
                let events = vec![SessionEvent::HtlcsFulfilled {
                    session_id: self.id,
                    payment_hash,
                    preimage,
                }];
                let outputs = vec![SessionOutput::BroadcastFunding {
                    psbt: funding_psbt.clone(),
                }];

                (
                    SessionState::Settling {
                        channel_id,
                        preimage,
                        funding_psbt,
                    },
                    ApplyResult::new(events, outputs),
                )
            }

            (
                SessionState::WaitingPreimage {
                    parts,
                    channel_id,
                    alias_scid,
                    funding_psbt,
                    ..
                },
                SessionInput::ClientRejectedPayment,
            ) => {
                let payment_hash = Self::extract_payment_hash(&parts);
                let events = vec![SessionEvent::HtlcsRejectedByClient {
                    session_id: self.id,
                    payment_hash,
                    rejection_type: "client_rejected".to_string(),
                    retry_count: 1,
                }];

                (
                    SessionState::AwaitingRetry {
                        parts: Vec::new(), // Clear parts for retry
                        channel_id,
                        alias_scid,
                        funding_psbt,
                        retry_count: 1,
                        first_part_at: Instant::now(),
                    },
                    ApplyResult::with_events(events),
                )
            }

            (
                SessionState::WaitingPreimage { parts, .. },
                SessionInput::UnsafeHold {
                    min_cltv_expiry,
                    current_height,
                },
            ) => {
                let htlc_ids = Self::extract_htlc_ids(&parts);
                let blocks_remaining = min_cltv_expiry.saturating_sub(current_height);
                let events = vec![
                    SessionEvent::UnsafeHoldDetected {
                        session_id: self.id,
                        phase: SessionPhase::WaitingPreimage,
                        min_cltv_expiry,
                        current_height,
                        blocks_remaining,
                    },
                    SessionEvent::HtlcsFailedUpstream {
                        session_id: self.id,
                        htlc_count: parts.len(),
                        error_code: FailureCode::TemporaryChannelFailure.as_str().to_string(),
                        reason: "unsafe_hold".to_string(),
                    },
                ];
                let outputs = vec![SessionOutput::FailHtlcs {
                    session_id: self.id,
                    htlc_ids,
                    failure_code: FailureCode::TemporaryChannelFailure,
                }];

                (
                    SessionState::Failed {
                        reason: FailureReason::UnsafeHold {
                            min_cltv: min_cltv_expiry,
                            current_height,
                        },
                        phase: SessionPhase::WaitingPreimage,
                    },
                    ApplyResult::new(events, outputs),
                )
            }

            // ================================================================
            // AwaitingRetry State Transitions
            // ================================================================
            (
                SessionState::AwaitingRetry {
                    mut parts,
                    channel_id,
                    alias_scid,
                    funding_psbt,
                    retry_count,
                    first_part_at,
                },
                SessionInput::PartArrived { part },
            ) => {
                parts.push(part.clone());

                if self.check_sum_reached(&parts) {
                    // Sum reached -> transition to Forwarding
                    let instructions = self.generate_forward_instructions(&parts, channel_id);
                    let outputs = vec![SessionOutput::ForwardHtlcs {
                        session_id: self.id,
                        instructions,
                    }];

                    (
                        SessionState::Forwarding {
                            parts,
                            channel_id,
                            alias_scid,
                            funding_psbt,
                        },
                        ApplyResult::new(Vec::new(), outputs),
                    )
                } else {
                    // Still collecting
                    let events = vec![SessionEvent::HtlcPartReceivedRetry {
                        session_id: self.id,
                        htlc_id: part.htlc_id,
                        amount_msat: part.amount_msat,
                        retry_count,
                    }];

                    (
                        SessionState::AwaitingRetry {
                            parts,
                            channel_id,
                            alias_scid,
                            funding_psbt,
                            retry_count,
                            first_part_at,
                        },
                        ApplyResult::with_events(events),
                    )
                }
            }

            (
                SessionState::AwaitingRetry {
                    parts,
                    channel_id,
                    funding_psbt,
                    retry_count,
                    ..
                },
                SessionInput::ValidUntilPassed,
            ) => {
                let htlc_ids = Self::extract_htlc_ids(&parts);
                let events = vec![
                    SessionEvent::ValidUntilExpired {
                        session_id: self.id,
                        phase: SessionPhase::AwaitingRetry,
                        valid_until: self.config.valid_until(),
                        current_time: Utc::now(),
                    },
                    SessionEvent::ChannelReleased {
                        session_id: self.id,
                        channel_id,
                        reason: "valid_until_expired".to_string(),
                    },
                ];
                let mut outputs = vec![SessionOutput::ReleaseChannel {
                    channel_id,
                    funding_psbt: funding_psbt.clone(),
                }];
                if !htlc_ids.is_empty() {
                    outputs.push(SessionOutput::FailHtlcs {
                        session_id: self.id,
                        htlc_ids,
                        failure_code: FailureCode::UnknownNextPeer,
                    });
                }

                (
                    SessionState::Abandoned {
                        reason: AbandonReason::ValidUntilExpired,
                        channel_id,
                        retry_count,
                    },
                    ApplyResult::new(events, outputs),
                )
            }

            (
                SessionState::AwaitingRetry {
                    parts,
                    channel_id,
                    funding_psbt,
                    retry_count,
                    first_part_at,
                    ..
                },
                SessionInput::CollectTimeout,
            ) => {
                let htlc_ids = Self::extract_htlc_ids(&parts);
                let elapsed_ms = first_part_at.elapsed().as_millis() as u64;
                let events = vec![
                    SessionEvent::CollectTimeoutRetry {
                        session_id: self.id,
                        elapsed_ms,
                        retry_count,
                    },
                    SessionEvent::ChannelReleased {
                        session_id: self.id,
                        channel_id,
                        reason: "collect_timeout".to_string(),
                    },
                ];
                let mut outputs = vec![SessionOutput::ReleaseChannel {
                    channel_id,
                    funding_psbt: funding_psbt.clone(),
                }];
                if !htlc_ids.is_empty() {
                    outputs.push(SessionOutput::FailHtlcs {
                        session_id: self.id,
                        htlc_ids,
                        failure_code: FailureCode::TemporaryChannelFailure,
                    });
                }

                (
                    SessionState::Abandoned {
                        reason: AbandonReason::CollectTimeout,
                        channel_id,
                        retry_count,
                    },
                    ApplyResult::new(events, outputs),
                )
            }

            (
                SessionState::AwaitingRetry {
                    parts,
                    channel_id,
                    funding_psbt,
                    retry_count,
                    ..
                },
                SessionInput::UnsafeHold {
                    min_cltv_expiry,
                    current_height,
                },
            ) => {
                let htlc_ids = Self::extract_htlc_ids(&parts);
                let blocks_remaining = min_cltv_expiry.saturating_sub(current_height);
                let events = vec![
                    SessionEvent::UnsafeHoldDetected {
                        session_id: self.id,
                        phase: SessionPhase::AwaitingRetry,
                        min_cltv_expiry,
                        current_height,
                        blocks_remaining,
                    },
                    SessionEvent::ChannelReleased {
                        session_id: self.id,
                        channel_id,
                        reason: "unsafe_hold".to_string(),
                    },
                ];
                let mut outputs = vec![SessionOutput::ReleaseChannel {
                    channel_id,
                    funding_psbt: funding_psbt.clone(),
                }];
                if !htlc_ids.is_empty() {
                    outputs.push(SessionOutput::FailHtlcs {
                        session_id: self.id,
                        htlc_ids,
                        failure_code: FailureCode::TemporaryChannelFailure,
                    });
                }

                (
                    SessionState::Abandoned {
                        reason: AbandonReason::UnsafeHold {
                            min_cltv: min_cltv_expiry,
                            current_height,
                        },
                        channel_id,
                        retry_count,
                    },
                    ApplyResult::new(events, outputs),
                )
            }

            // ================================================================
            // Settling State Transitions
            // ================================================================
            (
                SessionState::Settling {
                    channel_id,
                    preimage,
                    ..
                },
                SessionInput::FundingBroadcasted { txid },
            ) => {
                let events = vec![
                    SessionEvent::ChannelFundingBroadcast {
                        session_id: self.id,
                        channel_id,
                        funding_txid: txid,
                    },
                    SessionEvent::SessionCompleted {
                        session_id: self.id,
                        duration_ms: self.duration_ms(),
                        opening_fee_msat: 0, // TODO: track actual fee
                        channel_id,
                        funding_txid: txid,
                    },
                ];

                (
                    SessionState::Done {
                        channel_id,
                        funding_txid: txid,
                        preimage,
                    },
                    ApplyResult::with_events(events),
                )
            }

            // ================================================================
            // Invalid/Ignored Transitions
            // ================================================================
            // Terminal states ignore all inputs
            (state @ SessionState::Done { .. }, _)
            | (state @ SessionState::Failed { .. }, _)
            | (state @ SessionState::Abandoned { .. }, _) => (state, ApplyResult::empty()),

            // Any other unhandled transition - return state unchanged
            (state, _) => (state, ApplyResult::empty()),
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::lsps2::provider::{
        SessionEventEmitter, SessionOutputError, SessionOutputHandler,
    };
    use crate::proto::lsps0::Ppm;
    use crate::proto::lsps2::Promise;
    use chrono::{TimeZone, Utc};
    use std::sync::Mutex;

    // ========================================================================
    // Capturing Test Implementations
    // ========================================================================

    /// Event emitter that captures all events for test inspection.
    #[derive(Debug, Default)]
    pub struct CapturingEventEmitter {
        events: Mutex<Vec<SessionEvent>>,
    }

    impl CapturingEventEmitter {
        pub fn new() -> Self {
            Self::default()
        }

        /// Returns a clone of all captured events.
        pub fn events(&self) -> Vec<SessionEvent> {
            self.events.lock().unwrap().clone()
        }

        /// Clears captured events and returns them.
        pub fn take_events(&self) -> Vec<SessionEvent> {
            std::mem::take(&mut *self.events.lock().unwrap())
        }
    }

    #[async_trait::async_trait]
    impl SessionEventEmitter for CapturingEventEmitter {
        async fn emit(&self, event: SessionEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    /// Output handler that captures all outputs for test inspection.
    #[derive(Debug, Default)]
    pub struct CapturingOutputHandler {
        outputs: Mutex<Vec<SessionOutput>>,
    }

    impl CapturingOutputHandler {
        pub fn new() -> Self {
            Self::default()
        }

        /// Returns a clone of all captured outputs.
        pub fn outputs(&self) -> Vec<SessionOutput> {
            self.outputs.lock().unwrap().clone()
        }

        /// Clears captured outputs and returns them.
        pub fn take_outputs(&self) -> Vec<SessionOutput> {
            std::mem::take(&mut *self.outputs.lock().unwrap())
        }
    }

    #[async_trait::async_trait]
    impl SessionOutputHandler for CapturingOutputHandler {
        async fn execute(
            &self,
            output: SessionOutput,
        ) -> Result<Option<SessionInput>, SessionOutputError> {
            self.outputs.lock().unwrap().push(output);
            Ok(None)
        }
    }

    // ========================================================================
    // Test Helpers
    // ========================================================================

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

    // ========================================================================
    // State Transition Tests
    // ========================================================================

    fn test_session() -> Session {
        let scid = ShortChannelId::from(123u64);
        let session_id = SessionId::from(scid);
        let config = SessionConfig::new(
            test_public_key(),
            test_opening_fee_params(),
            Some(Msat::from_msat(100_000)), // MPP: expect 100k msat
        );
        Session::new(session_id, config)
    }

    fn test_htlc_part(htlc_id: u64, amount_msat: u64) -> HtlcPart {
        HtlcPart {
            htlc_id,
            amount_msat: Msat::from_msat(amount_msat),
            cltv_expiry: 800_000,
            payment_hash: [1u8; 32],
            arrived_at: Instant::now(),
            onion_payload: TlvStream::default(),
            extra_tlvs: TlvStream::default(),
            in_channel: ShortChannelId::from(999u64),
        }
    }

    fn test_txid() -> Txid {
        "0000000000000000000000000000000000000000000000000000000000000001"
            .parse()
            .unwrap()
    }

    #[test]
    fn test_collecting_part_arrived_not_enough() {
        let mut session = test_session();
        let part = test_htlc_part(1, 50_000); // Only 50k, need 100k

        let result = session.apply(SessionInput::PartArrived { part });

        assert_eq!(session.phase(), SessionPhase::Collecting);
        assert_eq!(result.events.len(), 1);
        assert!(matches!(
            result.events[0],
            SessionEvent::HtlcPartReceived { parts_count: 1, .. }
        ));
        assert!(result.outputs.is_empty());
    }

    #[test]
    fn test_collecting_part_arrived_sum_reached() {
        let mut session = test_session();
        let part = test_htlc_part(1, 100_000); // Exactly 100k

        let result = session.apply(SessionInput::PartArrived { part });

        assert_eq!(session.phase(), SessionPhase::Opening);
        assert_eq!(result.events.len(), 1);
        assert!(matches!(
            result.events[0],
            SessionEvent::ChannelOpenInitiated { .. }
        ));
        assert_eq!(result.outputs.len(), 1);
        assert!(matches!(
            result.outputs[0],
            SessionOutput::OpenChannel { .. }
        ));
    }

    #[test]
    fn test_collecting_multiple_parts_sum_reached() {
        let mut session = test_session();

        // First part: 60k
        let result1 = session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 60_000),
        });
        assert_eq!(session.phase(), SessionPhase::Collecting);
        assert!(result1.outputs.is_empty());

        // Second part: 50k (total 110k >= 100k)
        let result2 = session.apply(SessionInput::PartArrived {
            part: test_htlc_part(2, 50_000),
        });
        assert_eq!(session.phase(), SessionPhase::Opening);
        assert_eq!(result2.outputs.len(), 1);
    }

    #[test]
    fn test_collecting_timeout() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 50_000),
        });

        let result = session.apply(SessionInput::CollectTimeout);

        assert_eq!(session.phase(), SessionPhase::Failed);
        assert!(session.is_terminal());
        assert_eq!(result.events.len(), 2);
        assert!(matches!(
            result.events[0],
            SessionEvent::CollectTimeout { .. }
        ));
        assert_eq!(result.outputs.len(), 1);
        assert!(matches!(
            result.outputs[0],
            SessionOutput::FailHtlcs {
                failure_code: FailureCode::TemporaryChannelFailure,
                ..
            }
        ));
    }

    #[test]
    fn test_collecting_valid_until_passed() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 50_000),
        });

        let result = session.apply(SessionInput::ValidUntilPassed);

        assert_eq!(session.phase(), SessionPhase::Failed);
        assert!(matches!(
            result.outputs[0],
            SessionOutput::FailHtlcs {
                failure_code: FailureCode::UnknownNextPeer,
                ..
            }
        ));
    }

    #[test]
    fn test_collecting_unsafe_hold() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 50_000),
        });

        let result = session.apply(SessionInput::UnsafeHold {
            min_cltv_expiry: 800_000,
            current_height: 799_998,
        });

        assert_eq!(session.phase(), SessionPhase::Failed);
        assert!(matches!(
            result.events[0],
            SessionEvent::UnsafeHoldDetected {
                blocks_remaining: 2,
                ..
            }
        ));
    }

    #[test]
    fn test_opening_funding_signed() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 100_000),
        });
        assert_eq!(session.phase(), SessionPhase::Opening);

        let result = session.apply(SessionInput::FundingSigned {
            channel_id: ChannelId([1u8; 32]),
            funding_txid: test_txid(),
            funding_outpoint: 0,
            funding_psbt: "psbt_data".to_string(),
        });

        assert_eq!(session.phase(), SessionPhase::AwaitingChannelReady);
        assert_eq!(result.events.len(), 1);
        assert!(matches!(
            result.events[0],
            SessionEvent::ChannelFundingSigned { .. }
        ));
    }

    #[test]
    fn test_opening_client_rejects() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 100_000),
        });

        let result = session.apply(SessionInput::ClientRejectsChannel {
            error: "nope".to_string(),
        });

        assert_eq!(session.phase(), SessionPhase::Failed);
        assert!(matches!(
            result.outputs[0],
            SessionOutput::FailHtlcs {
                failure_code: FailureCode::UnknownNextPeer,
                ..
            }
        ));
    }

    #[test]
    fn test_awaiting_channel_ready_to_forwarding() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 100_000),
        });
        session.apply(SessionInput::FundingSigned {
            channel_id: ChannelId([1u8; 32]),
            funding_txid: test_txid(),
            funding_outpoint: 0,
            funding_psbt: "psbt".to_string(),
        });
        assert_eq!(session.phase(), SessionPhase::AwaitingChannelReady);

        let result = session.apply(SessionInput::ClientChannelReady {
            alias_scid: ShortChannelId::from(456u64),
        });

        assert_eq!(session.phase(), SessionPhase::Forwarding);
        assert_eq!(result.outputs.len(), 1);
        assert!(matches!(
            result.outputs[0],
            SessionOutput::ForwardHtlcs { .. }
        ));
    }

    #[test]
    fn test_forwarding_commits_to_waiting_preimage() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 100_000),
        });
        session.apply(SessionInput::FundingSigned {
            channel_id: ChannelId([1u8; 32]),
            funding_txid: test_txid(),
            funding_outpoint: 0,
            funding_psbt: "psbt".to_string(),
        });
        session.apply(SessionInput::ClientChannelReady {
            alias_scid: ShortChannelId::from(456u64),
        });
        assert_eq!(session.phase(), SessionPhase::Forwarding);

        let result = session.apply(SessionInput::ForwardsCommitted);

        assert_eq!(session.phase(), SessionPhase::WaitingPreimage);
        assert!(matches!(
            result.events[0],
            SessionEvent::HtlcsForwarded { .. }
        ));
    }

    #[test]
    fn test_waiting_preimage_received() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 100_000),
        });
        session.apply(SessionInput::FundingSigned {
            channel_id: ChannelId([1u8; 32]),
            funding_txid: test_txid(),
            funding_outpoint: 0,
            funding_psbt: "psbt".to_string(),
        });
        session.apply(SessionInput::ClientChannelReady {
            alias_scid: ShortChannelId::from(456u64),
        });
        session.apply(SessionInput::ForwardsCommitted);
        assert_eq!(session.phase(), SessionPhase::WaitingPreimage);

        let result = session.apply(SessionInput::PreimageReceived {
            preimage: [2u8; 32],
        });

        assert_eq!(session.phase(), SessionPhase::Settling);
        assert!(matches!(
            result.events[0],
            SessionEvent::HtlcsFulfilled { .. }
        ));
        assert!(matches!(
            result.outputs[0],
            SessionOutput::BroadcastFunding { .. }
        ));
    }

    #[test]
    fn test_waiting_preimage_client_rejected_to_retry() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 100_000),
        });
        session.apply(SessionInput::FundingSigned {
            channel_id: ChannelId([1u8; 32]),
            funding_txid: test_txid(),
            funding_outpoint: 0,
            funding_psbt: "psbt".to_string(),
        });
        session.apply(SessionInput::ClientChannelReady {
            alias_scid: ShortChannelId::from(456u64),
        });
        session.apply(SessionInput::ForwardsCommitted);

        let result = session.apply(SessionInput::ClientRejectedPayment);

        assert_eq!(session.phase(), SessionPhase::AwaitingRetry);
        assert!(matches!(
            result.events[0],
            SessionEvent::HtlcsRejectedByClient { retry_count: 1, .. }
        ));
    }

    #[test]
    fn test_awaiting_retry_part_arrived_sum_reached() {
        let mut session = test_session();
        // Get to AwaitingRetry state
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 100_000),
        });
        session.apply(SessionInput::FundingSigned {
            channel_id: ChannelId([1u8; 32]),
            funding_txid: test_txid(),
            funding_outpoint: 0,
            funding_psbt: "psbt".to_string(),
        });
        session.apply(SessionInput::ClientChannelReady {
            alias_scid: ShortChannelId::from(456u64),
        });
        session.apply(SessionInput::ForwardsCommitted);
        session.apply(SessionInput::ClientRejectedPayment);
        assert_eq!(session.phase(), SessionPhase::AwaitingRetry);

        // New payment attempt
        let result = session.apply(SessionInput::PartArrived {
            part: test_htlc_part(2, 100_000),
        });

        assert_eq!(session.phase(), SessionPhase::Forwarding);
        assert!(matches!(
            result.outputs[0],
            SessionOutput::ForwardHtlcs { .. }
        ));
    }

    #[test]
    fn test_awaiting_retry_timeout_abandons() {
        let mut session = test_session();
        // Get to AwaitingRetry state
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 100_000),
        });
        session.apply(SessionInput::FundingSigned {
            channel_id: ChannelId([1u8; 32]),
            funding_txid: test_txid(),
            funding_outpoint: 0,
            funding_psbt: "psbt".to_string(),
        });
        session.apply(SessionInput::ClientChannelReady {
            alias_scid: ShortChannelId::from(456u64),
        });
        session.apply(SessionInput::ForwardsCommitted);
        session.apply(SessionInput::ClientRejectedPayment);

        let result = session.apply(SessionInput::CollectTimeout);

        assert_eq!(session.phase(), SessionPhase::Abandoned);
        assert!(session.is_terminal());
        assert!(matches!(
            result.outputs[0],
            SessionOutput::ReleaseChannel { .. }
        ));
    }

    #[test]
    fn test_settling_funding_broadcasted() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 100_000),
        });
        session.apply(SessionInput::FundingSigned {
            channel_id: ChannelId([1u8; 32]),
            funding_txid: test_txid(),
            funding_outpoint: 0,
            funding_psbt: "psbt".to_string(),
        });
        session.apply(SessionInput::ClientChannelReady {
            alias_scid: ShortChannelId::from(456u64),
        });
        session.apply(SessionInput::ForwardsCommitted);
        session.apply(SessionInput::PreimageReceived {
            preimage: [2u8; 32],
        });
        assert_eq!(session.phase(), SessionPhase::Settling);

        let result = session.apply(SessionInput::FundingBroadcasted { txid: test_txid() });

        assert_eq!(session.phase(), SessionPhase::Done);
        assert!(session.is_terminal());
        assert!(matches!(
            result.events[1],
            SessionEvent::SessionCompleted { .. }
        ));
    }

    #[test]
    fn test_terminal_states_ignore_inputs() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 50_000),
        });
        session.apply(SessionInput::CollectTimeout);
        assert_eq!(session.phase(), SessionPhase::Failed);

        // Any input should be ignored
        let result = session.apply(SessionInput::PartArrived {
            part: test_htlc_part(2, 50_000),
        });

        assert_eq!(session.phase(), SessionPhase::Failed);
        assert!(result.events.is_empty());
        assert!(result.outputs.is_empty());
    }

    #[test]
    fn test_no_mpp_single_part_triggers_immediately() {
        let scid = ShortChannelId::from(123u64);
        let session_id = SessionId::from(scid);
        let config = SessionConfig::new(
            test_public_key(),
            test_opening_fee_params(),
            None, // No MPP - no expected_payment_size
        );
        let mut session = Session::new(session_id, config);

        // Any single part should trigger Opening immediately
        let result = session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 50_000),
        });

        assert_eq!(session.phase(), SessionPhase::Opening);
        assert!(matches!(
            result.outputs[0],
            SessionOutput::OpenChannel { .. }
        ));
    }

    #[test]
    fn test_forward_instructions_include_fee_deduction() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 100_000),
        });
        session.apply(SessionInput::FundingSigned {
            channel_id: ChannelId([1u8; 32]),
            funding_txid: test_txid(),
            funding_outpoint: 0,
            funding_psbt: "psbt".to_string(),
        });

        let result = session.apply(SessionInput::ClientChannelReady {
            alias_scid: ShortChannelId::from(456u64),
        });

        if let SessionOutput::ForwardHtlcs { instructions, .. } = &result.outputs[0] {
            assert_eq!(instructions.len(), 1);
            assert_eq!(instructions[0].htlc_id, 1);
            assert_eq!(
                instructions[0].forward_to_channel_id,
                ChannelId([1u8; 32])
            );
            // Verify extra_tlvs contains opening fee
            assert!(instructions[0].extra_tlvs.get(TLV_OPENING_FEE).is_some());
        } else {
            panic!("Expected ForwardHtlcs output");
        }
    }

    // ========================================================================
    // Capturing Implementation Tests
    // ========================================================================

    #[tokio::test]
    async fn test_capturing_event_emitter() {
        let emitter = CapturingEventEmitter::new();

        // Emit some events
        emitter
            .emit(SessionEvent::SessionCreated {
                session_id: SessionId::from(ShortChannelId::from(123u64)),
                scid: ShortChannelId::from(123u64),
                client_node_id: test_public_key(),
                payment_size_msat: Some(Msat::from_msat(100_000)),
                valid_until: Utc::now(),
            })
            .await;

        emitter
            .emit(SessionEvent::CollectTimeout {
                session_id: SessionId::from(ShortChannelId::from(123u64)),
                elapsed_ms: 90_000,
                parts_count: 1,
                parts_sum_msat: 50_000,
            })
            .await;

        // Verify events were captured
        let events = emitter.events();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], SessionEvent::SessionCreated { .. }));
        assert!(matches!(events[1], SessionEvent::CollectTimeout { .. }));

        // Verify take_events clears the list
        let taken = emitter.take_events();
        assert_eq!(taken.len(), 2);
        assert!(emitter.events().is_empty());
    }

    #[tokio::test]
    async fn test_capturing_output_handler() {
        let handler = CapturingOutputHandler::new();

        // Execute some outputs
        handler
            .execute(SessionOutput::OpenChannel {
                client_node_id: test_public_key(),
                channel_size_sat: 100_000,
            })
            .await
            .unwrap();

        handler
            .execute(SessionOutput::FailHtlcs {
                session_id: SessionId::from(ShortChannelId::from(123u64)),
                htlc_ids: vec![1, 2, 3],
                failure_code: FailureCode::TemporaryChannelFailure,
            })
            .await
            .unwrap();

        // Verify outputs were captured
        let outputs = handler.outputs();
        assert_eq!(outputs.len(), 2);
        assert!(matches!(outputs[0], SessionOutput::OpenChannel { .. }));
        assert!(matches!(outputs[1], SessionOutput::FailHtlcs { .. }));

        // Verify take_outputs clears the list
        let taken = handler.take_outputs();
        assert_eq!(taken.len(), 2);
        assert!(handler.outputs().is_empty());
    }

    #[tokio::test]
    async fn test_emit_all() {
        let emitter = CapturingEventEmitter::new();
        let session_id = SessionId::from(ShortChannelId::from(123u64));

        let events = vec![
            SessionEvent::CollectTimeout {
                session_id,
                elapsed_ms: 90_000,
                parts_count: 1,
                parts_sum_msat: 50_000,
            },
            SessionEvent::HtlcsFailedUpstream {
                session_id,
                htlc_count: 1,
                error_code: "temporary_channel_failure".to_string(),
                reason: "collect_timeout".to_string(),
            },
        ];

        emitter.emit_all(events).await;

        let captured = emitter.events();
        assert_eq!(captured.len(), 2);
    }

    #[tokio::test]
    async fn test_execute_all() {
        let handler = CapturingOutputHandler::new();

        let outputs = vec![
            SessionOutput::OpenChannel {
                client_node_id: test_public_key(),
                channel_size_sat: 100_000,
            },
            SessionOutput::BroadcastFunding {
                psbt: "psbt_data".to_string(),
            },
        ];

        handler.execute_all(outputs).await.unwrap();

        let captured = handler.outputs();
        assert_eq!(captured.len(), 2);
    }

    // ========================================================================
    // Test Helpers for Reaching Each State
    // ========================================================================

    fn session_in_opening() -> Session {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 100_000),
        });
        assert_eq!(session.phase(), SessionPhase::Opening);
        session
    }

    fn session_in_awaiting_channel_ready() -> Session {
        let mut session = session_in_opening();
        session.apply(SessionInput::FundingSigned {
            channel_id: ChannelId([1u8; 32]),
            funding_txid: test_txid(),
            funding_outpoint: 0,
            funding_psbt: "psbt".to_string(),
        });
        assert_eq!(session.phase(), SessionPhase::AwaitingChannelReady);
        session
    }

    fn session_in_forwarding() -> Session {
        let mut session = session_in_awaiting_channel_ready();
        session.apply(SessionInput::ClientChannelReady {
            alias_scid: ShortChannelId::from(456u64),
        });
        assert_eq!(session.phase(), SessionPhase::Forwarding);
        session
    }

    fn session_in_waiting_preimage() -> Session {
        let mut session = session_in_forwarding();
        session.apply(SessionInput::ForwardsCommitted);
        assert_eq!(session.phase(), SessionPhase::WaitingPreimage);
        session
    }

    fn session_in_awaiting_retry() -> Session {
        let mut session = session_in_waiting_preimage();
        session.apply(SessionInput::ClientRejectedPayment);
        assert_eq!(session.phase(), SessionPhase::AwaitingRetry);
        session
    }

    fn session_in_settling() -> Session {
        let mut session = session_in_waiting_preimage();
        session.apply(SessionInput::PreimageReceived {
            preimage: [0xab; 32],
        });
        assert_eq!(session.phase(), SessionPhase::Settling);
        session
    }

    // ========================================================================
    // Missing State Transition Tests
    // ========================================================================

    #[test]
    fn test_collecting_too_many_parts() {
        let mut session = test_session();

        // Add a part first
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 10_000),
        });

        // Then trigger TooManyParts
        let result = session.apply(SessionInput::TooManyParts { max_parts: 5 });

        assert_eq!(session.phase(), SessionPhase::Failed);
        assert!(matches!(
            session.state(),
            SessionState::Failed {
                reason: FailureReason::TooManyParts { .. },
                ..
            }
        ));
        // Should emit TooManyParts and HtlcsFailedUpstream events
        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::TooManyParts { .. })));
        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::HtlcsFailedUpstream { .. })));
        // Should output FailHtlcs
        assert!(result
            .outputs
            .iter()
            .any(|o| matches!(o, SessionOutput::FailHtlcs { .. })));
    }

    #[test]
    fn test_opening_client_disconnected() {
        let mut session = session_in_opening();

        let result = session.apply(SessionInput::ClientDisconnected);

        assert_eq!(session.phase(), SessionPhase::Failed);
        assert!(matches!(
            session.state(),
            SessionState::Failed {
                reason: FailureReason::ClientDisconnected,
                ..
            }
        ));
        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::ClientDisconnected { .. })));
    }

    #[test]
    fn test_opening_unsafe_hold() {
        let mut session = session_in_opening();

        let result = session.apply(SessionInput::UnsafeHold {
            min_cltv_expiry: 100,
            current_height: 95,
        });

        assert_eq!(session.phase(), SessionPhase::Failed);
        assert!(matches!(
            session.state(),
            SessionState::Failed {
                reason: FailureReason::UnsafeHold { .. },
                ..
            }
        ));
        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::UnsafeHoldDetected { .. })));
    }

    #[test]
    fn test_awaiting_channel_ready_client_disconnect() {
        let mut session = session_in_awaiting_channel_ready();

        let result = session.apply(SessionInput::ClientDisconnected);

        assert_eq!(session.phase(), SessionPhase::Failed);
        assert!(matches!(
            session.state(),
            SessionState::Failed {
                reason: FailureReason::ClientDisconnected,
                ..
            }
        ));
        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::ClientDisconnected { .. })));
    }

    #[test]
    fn test_awaiting_channel_ready_unsafe_hold() {
        let mut session = session_in_awaiting_channel_ready();

        let result = session.apply(SessionInput::UnsafeHold {
            min_cltv_expiry: 100,
            current_height: 95,
        });

        assert_eq!(session.phase(), SessionPhase::Failed);
        assert!(matches!(
            session.state(),
            SessionState::Failed {
                reason: FailureReason::UnsafeHold { .. },
                ..
            }
        ));
        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::UnsafeHoldDetected { .. })));
    }

    #[test]
    fn test_forwarding_unsafe_hold() {
        let mut session = session_in_forwarding();

        let result = session.apply(SessionInput::UnsafeHold {
            min_cltv_expiry: 100,
            current_height: 95,
        });

        assert_eq!(session.phase(), SessionPhase::Failed);
        assert!(matches!(
            session.state(),
            SessionState::Failed {
                reason: FailureReason::UnsafeHold { .. },
                ..
            }
        ));
        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::UnsafeHoldDetected { .. })));
    }

    #[test]
    fn test_waiting_preimage_unsafe_hold() {
        let mut session = session_in_waiting_preimage();

        let result = session.apply(SessionInput::UnsafeHold {
            min_cltv_expiry: 100,
            current_height: 95,
        });

        assert_eq!(session.phase(), SessionPhase::Failed);
        assert!(matches!(
            session.state(),
            SessionState::Failed {
                reason: FailureReason::UnsafeHold { .. },
                ..
            }
        ));
        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::UnsafeHoldDetected { .. })));
    }

    #[test]
    fn test_awaiting_retry_valid_until_passed() {
        let mut session = session_in_awaiting_retry();

        let result = session.apply(SessionInput::ValidUntilPassed);

        assert_eq!(session.phase(), SessionPhase::Abandoned);
        assert!(matches!(
            session.state(),
            SessionState::Abandoned {
                reason: AbandonReason::ValidUntilExpired,
                ..
            }
        ));
        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::ValidUntilExpired { .. })));
        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::ChannelReleased { .. })));
    }

    #[test]
    fn test_awaiting_retry_unsafe_hold() {
        let mut session = session_in_awaiting_retry();

        let result = session.apply(SessionInput::UnsafeHold {
            min_cltv_expiry: 100,
            current_height: 95,
        });

        assert_eq!(session.phase(), SessionPhase::Abandoned);
        assert!(matches!(
            session.state(),
            SessionState::Abandoned {
                reason: AbandonReason::UnsafeHold { .. },
                ..
            }
        ));
        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::UnsafeHoldDetected { .. })));
        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::ChannelReleased { .. })));
    }

    // ========================================================================
    // Invalid Transition Tests
    // ========================================================================

    #[test]
    fn test_invalid_funding_signed_in_collecting() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 50_000), // Not enough, stays in Collecting
        });

        // FundingSigned should be ignored in Collecting state
        let result = session.apply(SessionInput::FundingSigned {
            channel_id: ChannelId([1u8; 32]),
            funding_txid: test_txid(),
            funding_outpoint: 0,
            funding_psbt: "psbt".to_string(),
        });

        assert_eq!(session.phase(), SessionPhase::Collecting);
        assert!(result.events.is_empty());
        assert!(result.outputs.is_empty());
    }

    #[test]
    fn test_invalid_client_channel_ready_in_collecting() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 50_000),
        });

        // ClientChannelReady should be ignored in Collecting state
        let result = session.apply(SessionInput::ClientChannelReady {
            alias_scid: ShortChannelId::from(456u64),
        });

        assert_eq!(session.phase(), SessionPhase::Collecting);
        assert!(result.events.is_empty());
        assert!(result.outputs.is_empty());
    }

    #[test]
    fn test_invalid_preimage_received_in_collecting() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 50_000),
        });

        // PreimageReceived should be ignored in Collecting state
        let result = session.apply(SessionInput::PreimageReceived {
            preimage: [0xab; 32],
        });

        assert_eq!(session.phase(), SessionPhase::Collecting);
        assert!(result.events.is_empty());
        assert!(result.outputs.is_empty());
    }

    #[test]
    fn test_invalid_part_arrived_in_opening() {
        let mut session = session_in_opening();

        // PartArrived should be ignored in Opening state
        let result = session.apply(SessionInput::PartArrived {
            part: test_htlc_part(2, 50_000),
        });

        assert_eq!(session.phase(), SessionPhase::Opening);
        assert!(result.events.is_empty());
        assert!(result.outputs.is_empty());
    }

    #[test]
    fn test_invalid_forwards_committed_in_collecting() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 50_000),
        });

        // ForwardsCommitted should be ignored in Collecting state
        let result = session.apply(SessionInput::ForwardsCommitted);

        assert_eq!(session.phase(), SessionPhase::Collecting);
        assert!(result.events.is_empty());
        assert!(result.outputs.is_empty());
    }

    #[test]
    fn test_invalid_funding_broadcasted_in_opening() {
        let mut session = session_in_opening();

        // FundingBroadcasted should be ignored in Opening state
        let result = session.apply(SessionInput::FundingBroadcasted { txid: test_txid() });

        assert_eq!(session.phase(), SessionPhase::Opening);
        assert!(result.events.is_empty());
        assert!(result.outputs.is_empty());
    }

    // ========================================================================
    // Event Emission Verification Tests
    // ========================================================================

    #[test]
    fn test_sum_reached_emits_channel_open_initiated() {
        let mut session = test_session();

        let result = session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 100_000),
        });

        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::ChannelOpenInitiated { .. })));
    }

    #[test]
    fn test_funding_signed_emits_channel_funding_signed() {
        let mut session = session_in_opening();

        let result = session.apply(SessionInput::FundingSigned {
            channel_id: ChannelId([1u8; 32]),
            funding_txid: test_txid(),
            funding_outpoint: 0,
            funding_psbt: "psbt".to_string(),
        });

        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::ChannelFundingSigned { .. })));
    }

    #[test]
    fn test_channel_ready_emits_channel_ready_event() {
        let mut session = session_in_awaiting_channel_ready();

        let result = session.apply(SessionInput::ClientChannelReady {
            alias_scid: ShortChannelId::from(456u64),
        });

        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::ChannelReady { .. })));
    }

    #[test]
    fn test_forwards_committed_emits_htlcs_forwarded() {
        let mut session = session_in_forwarding();

        let result = session.apply(SessionInput::ForwardsCommitted);

        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::HtlcsForwarded { .. })));
    }

    #[test]
    fn test_preimage_received_emits_htlcs_fulfilled() {
        let mut session = session_in_waiting_preimage();

        let result = session.apply(SessionInput::PreimageReceived {
            preimage: [0xab; 32],
        });

        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::HtlcsFulfilled { .. })));
    }

    #[test]
    fn test_funding_broadcasted_emits_session_completed() {
        let mut session = session_in_settling();

        let result = session.apply(SessionInput::FundingBroadcasted { txid: test_txid() });

        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::ChannelFundingBroadcast { .. })));
        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::SessionCompleted { .. })));
    }

    #[test]
    fn test_client_rejected_payment_emits_htlcs_rejected() {
        let mut session = session_in_waiting_preimage();

        let result = session.apply(SessionInput::ClientRejectedPayment);

        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, SessionEvent::HtlcsRejectedByClient { .. })));
    }

    // ========================================================================
    // Output Verification Tests
    // ========================================================================

    #[test]
    fn test_sum_reached_outputs_open_channel() {
        let mut session = test_session();

        let result = session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 100_000),
        });

        assert!(result
            .outputs
            .iter()
            .any(|o| matches!(o, SessionOutput::OpenChannel { .. })));
    }

    #[test]
    fn test_channel_ready_outputs_forward_htlcs() {
        let mut session = session_in_awaiting_channel_ready();

        let result = session.apply(SessionInput::ClientChannelReady {
            alias_scid: ShortChannelId::from(456u64),
        });

        assert!(result
            .outputs
            .iter()
            .any(|o| matches!(o, SessionOutput::ForwardHtlcs { .. })));
    }

    #[test]
    fn test_timeout_outputs_fail_htlcs() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 50_000),
        });

        let result = session.apply(SessionInput::CollectTimeout);

        assert!(result
            .outputs
            .iter()
            .any(|o| matches!(o, SessionOutput::FailHtlcs { .. })));
    }

    #[test]
    fn test_preimage_received_outputs_broadcast_funding() {
        let mut session = session_in_waiting_preimage();

        let result = session.apply(SessionInput::PreimageReceived {
            preimage: [0xab; 32],
        });

        assert!(result
            .outputs
            .iter()
            .any(|o| matches!(o, SessionOutput::BroadcastFunding { .. })));
    }

    #[test]
    fn test_abandoned_outputs_release_channel() {
        let mut session = session_in_awaiting_retry();

        let result = session.apply(SessionInput::CollectTimeout);

        assert!(result
            .outputs
            .iter()
            .any(|o| matches!(o, SessionOutput::ReleaseChannel { .. })));
    }

    #[test]
    fn test_fail_htlcs_has_correct_failure_code_timeout() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 50_000),
        });

        let result = session.apply(SessionInput::CollectTimeout);

        if let Some(SessionOutput::FailHtlcs { failure_code, .. }) = result.outputs.first() {
            assert_eq!(*failure_code, FailureCode::TemporaryChannelFailure);
        } else {
            panic!("Expected FailHtlcs output");
        }
    }

    #[test]
    fn test_fail_htlcs_has_correct_failure_code_valid_until() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 50_000),
        });

        let result = session.apply(SessionInput::ValidUntilPassed);

        if let Some(SessionOutput::FailHtlcs { failure_code, .. }) = result.outputs.first() {
            assert_eq!(*failure_code, FailureCode::UnknownNextPeer);
        } else {
            panic!("Expected FailHtlcs output");
        }
    }

    // ========================================================================
    // Idempotency and Edge Case Tests
    // ========================================================================

    #[test]
    fn test_terminal_done_ignores_all_inputs() {
        let mut session = session_in_settling();
        session.apply(SessionInput::FundingBroadcasted { txid: test_txid() });
        assert_eq!(session.phase(), SessionPhase::Done);

        // All inputs should be ignored
        let inputs = vec![
            SessionInput::PartArrived {
                part: test_htlc_part(99, 50_000),
            },
            SessionInput::CollectTimeout,
            SessionInput::ValidUntilPassed,
            SessionInput::FundingSigned {
                channel_id: ChannelId([2u8; 32]),
                funding_txid: test_txid(),
                funding_outpoint: 0,
                funding_psbt: "psbt2".to_string(),
            },
            SessionInput::PreimageReceived {
                preimage: [0xcd; 32],
            },
        ];

        for input in inputs {
            let result = session.apply(input);
            assert_eq!(session.phase(), SessionPhase::Done);
            assert!(result.events.is_empty());
            assert!(result.outputs.is_empty());
        }
    }

    #[test]
    fn test_terminal_failed_ignores_all_inputs() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 50_000),
        });
        session.apply(SessionInput::CollectTimeout);
        assert_eq!(session.phase(), SessionPhase::Failed);

        // All inputs should be ignored
        let result = session.apply(SessionInput::PartArrived {
            part: test_htlc_part(2, 50_000),
        });
        assert_eq!(session.phase(), SessionPhase::Failed);
        assert!(result.events.is_empty());
        assert!(result.outputs.is_empty());
    }

    #[test]
    fn test_terminal_abandoned_ignores_all_inputs() {
        let mut session = session_in_awaiting_retry();
        session.apply(SessionInput::CollectTimeout);
        assert_eq!(session.phase(), SessionPhase::Abandoned);

        // All inputs should be ignored
        let result = session.apply(SessionInput::PartArrived {
            part: test_htlc_part(2, 50_000),
        });
        assert_eq!(session.phase(), SessionPhase::Abandoned);
        assert!(result.events.is_empty());
        assert!(result.outputs.is_empty());
    }

    #[test]
    fn test_multiple_parts_accumulate() {
        let mut session = test_session();

        // First part
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 30_000),
        });
        assert_eq!(session.phase(), SessionPhase::Collecting);

        // Second part
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(2, 30_000),
        });
        assert_eq!(session.phase(), SessionPhase::Collecting);

        // Third part - should reach sum
        let result = session.apply(SessionInput::PartArrived {
            part: test_htlc_part(3, 40_000),
        });
        assert_eq!(session.phase(), SessionPhase::Opening);
        assert!(result
            .outputs
            .iter()
            .any(|o| matches!(o, SessionOutput::OpenChannel { .. })));
    }

    #[test]
    fn test_overpayment_still_transitions() {
        let mut session = test_session(); // expects 100_000

        // Send more than expected
        let result = session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 150_000),
        });

        assert_eq!(session.phase(), SessionPhase::Opening);
        assert!(result
            .outputs
            .iter()
            .any(|o| matches!(o, SessionOutput::OpenChannel { .. })));
    }

    #[test]
    fn test_retry_clears_old_parts_and_accumulates_new() {
        let mut session = session_in_awaiting_retry();

        // Add new parts in retry state
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(10, 50_000),
        });
        assert_eq!(session.phase(), SessionPhase::AwaitingRetry);

        // Add enough to reach sum
        let result = session.apply(SessionInput::PartArrived {
            part: test_htlc_part(11, 60_000),
        });

        // Should transition back to Forwarding
        assert_eq!(session.phase(), SessionPhase::Forwarding);
        assert!(result
            .outputs
            .iter()
            .any(|o| matches!(o, SessionOutput::ForwardHtlcs { .. })));
    }

    #[test]
    fn test_retry_preserves_channel_info() {
        let session = session_in_awaiting_retry();

        // Verify channel_id is preserved
        if let SessionState::AwaitingRetry { channel_id, .. } = session.state() {
            assert_eq!(channel_id.as_bytes(), &[1u8; 32]);
        } else {
            panic!("Expected AwaitingRetry state");
        }
    }

    #[test]
    fn test_forward_instructions_contain_all_htlc_ids() {
        let mut session = test_session();

        // Add multiple parts
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 40_000),
        });
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(2, 30_000),
        });
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(3, 40_000),
        });

        session.apply(SessionInput::FundingSigned {
            channel_id: ChannelId([1u8; 32]),
            funding_txid: test_txid(),
            funding_outpoint: 0,
            funding_psbt: "psbt".to_string(),
        });

        let result = session.apply(SessionInput::ClientChannelReady {
            alias_scid: ShortChannelId::from(456u64),
        });

        if let Some(SessionOutput::ForwardHtlcs { instructions, .. }) = result.outputs.first() {
            assert_eq!(instructions.len(), 3);
            let htlc_ids: Vec<u64> = instructions.iter().map(|i| i.htlc_id).collect();
            assert!(htlc_ids.contains(&1));
            assert!(htlc_ids.contains(&2));
            assert!(htlc_ids.contains(&3));
        } else {
            panic!("Expected ForwardHtlcs output");
        }
    }

    #[test]
    fn test_fail_htlcs_contain_all_htlc_ids() {
        let mut session = test_session();

        // Add multiple parts
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 30_000),
        });
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(2, 20_000),
        });

        let result = session.apply(SessionInput::CollectTimeout);

        if let Some(SessionOutput::FailHtlcs { htlc_ids, .. }) = result.outputs.first() {
            assert_eq!(htlc_ids.len(), 2);
            assert!(htlc_ids.contains(&1));
            assert!(htlc_ids.contains(&2));
        } else {
            panic!("Expected FailHtlcs output");
        }
    }

    #[test]
    fn test_session_duration_tracked() {
        let session = test_session();

        // Duration should be very small (just created)
        let duration = session.duration();
        assert!(duration.as_millis() < 100);

        // Duration in ms should also work
        let duration_ms = session.duration_ms();
        assert!(duration_ms < 100);
    }

    #[test]
    fn test_session_payment_hash_from_parts() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 50_000),
        });

        let payment_hash = session.payment_hash();
        assert!(payment_hash.is_some());
        assert_eq!(payment_hash.unwrap(), [1u8; 32]); // From test_htlc_part
    }

    #[test]
    fn test_session_htlc_ids() {
        let mut session = test_session();
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(1, 30_000),
        });
        session.apply(SessionInput::PartArrived {
            part: test_htlc_part(2, 30_000),
        });

        let htlc_ids = session.htlc_ids();
        assert_eq!(htlc_ids.len(), 2);
        assert!(htlc_ids.contains(&1));
        assert!(htlc_ids.contains(&2));
    }
}
