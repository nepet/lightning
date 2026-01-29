use anyhow::Result;
use async_trait::async_trait;
use bitcoin::hashes::sha256::Hash;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Txid;

use crate::core::lsps2::session::{SessionEvent, SessionOutput};
use crate::proto::{
    lsps0::{Msat, ShortChannelId},
    lsps2::{
        DatastoreEntry, Lsps2PolicyGetChannelCapacityRequest,
        Lsps2PolicyGetChannelCapacityResponse, Lsps2PolicyGetInfoRequest,
        Lsps2PolicyGetInfoResponse, OpeningFeeParams,
    },
};

pub type Blockheight = u32;

// ============================================================================
// Channel Funding Types
// ============================================================================

/// Result from `fundchannel_start` RPC call.
#[derive(Debug, Clone)]
pub struct FundChannelStartResult {
    /// The address to send funding to for the channel.
    pub funding_address: String,
    /// The raw scriptPubkey for the address (hex encoded).
    pub scriptpubkey: String,
    /// Number of confirmations required before channel is active.
    pub mindepth: Option<u32>,
}

/// Result from `fundchannel_complete` RPC call.
#[derive(Debug, Clone)]
pub struct FundChannelCompleteResult {
    /// The channel_id of the resulting channel (32 bytes).
    pub channel_id: [u8; 32],
    /// Whether commitments are secured (always true on success).
    pub commitments_secured: bool,
}

/// Result from `sendpsbt` RPC call.
#[derive(Debug, Clone)]
pub struct SendPsbtResult {
    /// The raw transaction that was sent (hex encoded).
    pub tx: String,
    /// The txid of the transaction.
    pub txid: Txid,
}

/// Channel state from CLN's `listpeerchannels`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    /// Channel opening in progress (OPENINGD)
    Openingd,
    /// Waiting for funding confirmation (CHANNELD_AWAITING_LOCKIN)
    ChanneldAwaitingLockin,
    /// Channel is ready for normal operation (CHANNELD_NORMAL)
    ChanneldNormal,
    /// Channel is shutting down (CHANNELD_SHUTTING_DOWN)
    ChanneldShuttingDown,
    /// Closing signature exchange (CLOSINGD_SIGEXCHANGE)
    ClosingdSigexchange,
    /// Closing complete (CLOSINGD_COMPLETE)
    ClosingdComplete,
    /// Awaiting unilateral close (AWAITING_UNILATERAL)
    AwaitingUnilateral,
    /// Funding spend seen (FUNDING_SPEND_SEEN)
    FundingSpendSeen,
    /// On chain (ONCHAIN)
    Onchain,
    /// Dual-funding: open init (DUALOPEND_OPEN_INIT)
    DualopendOpenInit,
    /// Dual-funding: awaiting lockin (DUALOPEND_AWAITING_LOCKIN)
    DualopendAwaitingLockin,
    /// Dual-funding: open committed (DUALOPEND_OPEN_COMMITTED)
    DualopendOpenCommitted,
    /// Dual-funding: open commit ready (DUALOPEND_OPEN_COMMIT_READY)
    DualopendOpenCommitReady,
    /// Splice in progress (CHANNELD_AWAITING_SPLICE)
    ChanneldAwaitingSplice,
}

impl ChannelState {
    /// Parse channel state from CLN string representation.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "OPENINGD" => Some(Self::Openingd),
            "CHANNELD_AWAITING_LOCKIN" => Some(Self::ChanneldAwaitingLockin),
            "CHANNELD_NORMAL" => Some(Self::ChanneldNormal),
            "CHANNELD_SHUTTING_DOWN" => Some(Self::ChanneldShuttingDown),
            "CLOSINGD_SIGEXCHANGE" => Some(Self::ClosingdSigexchange),
            "CLOSINGD_COMPLETE" => Some(Self::ClosingdComplete),
            "AWAITING_UNILATERAL" => Some(Self::AwaitingUnilateral),
            "FUNDING_SPEND_SEEN" => Some(Self::FundingSpendSeen),
            "ONCHAIN" => Some(Self::Onchain),
            "DUALOPEND_OPEN_INIT" => Some(Self::DualopendOpenInit),
            "DUALOPEND_AWAITING_LOCKIN" => Some(Self::DualopendAwaitingLockin),
            "DUALOPEND_OPEN_COMMITTED" => Some(Self::DualopendOpenCommitted),
            "DUALOPEND_OPEN_COMMIT_READY" => Some(Self::DualopendOpenCommitReady),
            "CHANNELD_AWAITING_SPLICE" => Some(Self::ChanneldAwaitingSplice),
            _ => None,
        }
    }

    /// Returns true if this state indicates the channel is ready for normal operation.
    pub fn is_normal(&self) -> bool {
        matches!(self, Self::ChanneldNormal)
    }

    /// Returns true if this state indicates the channel is closed or closing.
    pub fn is_closed_or_closing(&self) -> bool {
        matches!(
            self,
            Self::ChanneldShuttingDown
                | Self::ClosingdSigexchange
                | Self::ClosingdComplete
                | Self::AwaitingUnilateral
                | Self::FundingSpendSeen
                | Self::Onchain
        )
    }
}

impl std::fmt::Display for ChannelState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Openingd => "OPENINGD",
            Self::ChanneldAwaitingLockin => "CHANNELD_AWAITING_LOCKIN",
            Self::ChanneldNormal => "CHANNELD_NORMAL",
            Self::ChanneldShuttingDown => "CHANNELD_SHUTTING_DOWN",
            Self::ClosingdSigexchange => "CLOSINGD_SIGEXCHANGE",
            Self::ClosingdComplete => "CLOSINGD_COMPLETE",
            Self::AwaitingUnilateral => "AWAITING_UNILATERAL",
            Self::FundingSpendSeen => "FUNDING_SPEND_SEEN",
            Self::Onchain => "ONCHAIN",
            Self::DualopendOpenInit => "DUALOPEND_OPEN_INIT",
            Self::DualopendAwaitingLockin => "DUALOPEND_AWAITING_LOCKIN",
            Self::DualopendOpenCommitted => "DUALOPEND_OPEN_COMMITTED",
            Self::DualopendOpenCommitReady => "DUALOPEND_OPEN_COMMIT_READY",
            Self::ChanneldAwaitingSplice => "CHANNELD_AWAITING_SPLICE",
        };
        write!(f, "{}", s)
    }
}

/// Information about a channel from `listpeerchannels`.
#[derive(Debug, Clone)]
pub struct ChannelInfo {
    /// The channel state.
    pub state: ChannelState,
    /// Whether the peer is connected.
    pub peer_connected: bool,
    /// The channel_id (32 bytes), if available.
    pub channel_id: Option<[u8; 32]>,
    /// The short_channel_id, if available (when channel is confirmed).
    pub short_channel_id: Option<ShortChannelId>,
    /// The local alias SCID for forwarding before channel is confirmed.
    pub alias_scid: Option<ShortChannelId>,
    /// Whether channel funding is withheld (not broadcast).
    pub withheld: Option<bool>,
}

// ============================================================================
// Provider Traits
// ============================================================================

#[async_trait]
pub trait BlockheightProvider: Send + Sync {
    async fn get_blockheight(&self) -> Result<Blockheight>;
}

#[async_trait]
pub trait DatastoreProvider: Send + Sync {
    async fn store_buy_request(
        &self,
        scid: &ShortChannelId,
        peer_id: &PublicKey,
        offer: &OpeningFeeParams,
        expected_payment_size: &Option<Msat>,
    ) -> Result<bool>;

    async fn get_buy_request(&self, scid: &ShortChannelId) -> Result<DatastoreEntry>;
    async fn del_buy_request(&self, scid: &ShortChannelId) -> Result<()>;
}

#[async_trait]
pub trait LightningProvider: Send + Sync {
    // -------------------------------------------------------------------------
    // Legacy methods (for backward compatibility with no-MPP flow)
    // -------------------------------------------------------------------------

    /// Fund a JIT channel using the legacy single-shot flow.
    ///
    /// This method is used by the no-MPP flow and may be deprecated in favor
    /// of the new fund_channel_start/fund_channel_complete_withheld flow.
    async fn fund_jit_channel(&self, peer_id: &PublicKey, amount: &Msat) -> Result<(Hash, String)>;

    /// Check if a channel is ready for the legacy flow.
    async fn is_channel_ready(&self, peer_id: &PublicKey, channel_id: &Hash) -> Result<bool>;

    // -------------------------------------------------------------------------
    // New methods for MPP flow with withheld funding
    // -------------------------------------------------------------------------

    /// Initiate channel funding with a peer.
    ///
    /// Calls `fundchannel_start` RPC. Returns the funding address and scriptpubkey
    /// that should be used to construct the funding PSBT.
    ///
    /// **Important**: The funding transaction MUST NOT be broadcast until after
    /// `fund_channel_complete_withheld` succeeds.
    ///
    /// # Arguments
    /// * `peer_id` - The node public key of the peer
    /// * `amount_sat` - The channel capacity in satoshis
    /// * `announce` - Whether to announce this channel publicly
    /// * `mindepth` - Number of confirmations required (use 0 for zero-conf)
    async fn fund_channel_start(
        &self,
        peer_id: &PublicKey,
        amount_sat: u64,
        announce: bool,
        mindepth: Option<u32>,
    ) -> Result<FundChannelStartResult>;

    /// Complete channel funding with a PSBT, withholding broadcast.
    ///
    /// Calls `fundchannel_complete` RPC with `withhold=true`. This completes
    /// channel negotiation (funding_signed, channel_ready exchange) but does
    /// NOT broadcast the funding transaction.
    ///
    /// The channel will be marked as "withheld" and can be used for forwarding
    /// HTLCs. Once the preimage is received, call `broadcast_funding` to
    /// broadcast the funding transaction.
    ///
    /// # Arguments
    /// * `peer_id` - The node public key of the peer
    /// * `psbt` - The funding PSBT (does not need to be signed)
    async fn fund_channel_complete_withheld(
        &self,
        peer_id: &PublicKey,
        psbt: &str,
    ) -> Result<FundChannelCompleteResult>;

    /// Broadcast a withheld funding transaction.
    ///
    /// Calls `sendpsbt` RPC to broadcast the funding transaction after
    /// receiving the preimage from the client. This should only be called
    /// after a successful forward through the withheld channel.
    ///
    /// # Arguments
    /// * `psbt` - The fully signed funding PSBT
    async fn broadcast_funding(&self, psbt: &str) -> Result<SendPsbtResult>;

    /// Get information about a channel with a peer.
    ///
    /// Calls `listpeerchannels` RPC filtered by peer_id. If channel_id is
    /// provided, returns only the matching channel.
    ///
    /// # Arguments
    /// * `peer_id` - The node public key of the peer
    /// * `channel_id` - Optional channel_id to filter by
    ///
    /// # Returns
    /// * `Ok(Some(info))` - Channel found
    /// * `Ok(None)` - No matching channel found
    /// * `Err(_)` - RPC error
    async fn get_channel_info(
        &self,
        peer_id: &PublicKey,
        channel_id: Option<&[u8; 32]>,
    ) -> Result<Option<ChannelInfo>>;

    /// Get channel state for a specific channel.
    ///
    /// Convenience method that calls `get_channel_info` and extracts the state.
    ///
    /// # Returns
    /// * `Ok(Some(state))` - Channel found, returns its state
    /// * `Ok(None)` - No matching channel found
    /// * `Err(_)` - RPC error
    async fn get_channel_state(
        &self,
        peer_id: &PublicKey,
        channel_id: &[u8; 32],
    ) -> Result<Option<ChannelState>> {
        Ok(self
            .get_channel_info(peer_id, Some(channel_id))
            .await?
            .map(|info| info.state))
    }
}

#[async_trait]
pub trait Lsps2OfferProvider: Send + Sync {
    async fn get_offer(
        &self,
        request: &Lsps2PolicyGetInfoRequest,
    ) -> Result<Lsps2PolicyGetInfoResponse>;

    async fn get_channel_capacity(
        &self,
        params: &Lsps2PolicyGetChannelCapacityRequest,
    ) -> Result<Lsps2PolicyGetChannelCapacityResponse>;
}

// ============================================================================
// Session Event and Output Handlers
// ============================================================================

/// Error type for session output execution failures.
#[derive(Debug, Clone)]
pub enum SessionOutputError {
    /// RPC call failed
    RpcError(String),
    /// Channel operation failed
    ChannelError(String),
    /// HTLC operation failed
    HtlcError(String),
    /// Internal error
    Internal(String),
}

impl std::fmt::Display for SessionOutputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RpcError(e) => write!(f, "RPC call failed: {}", e),
            Self::ChannelError(e) => write!(f, "Channel operation failed: {}", e),
            Self::HtlcError(e) => write!(f, "HTLC operation failed: {}", e),
            Self::Internal(e) => write!(f, "Internal error: {}", e),
        }
    }
}

impl std::error::Error for SessionOutputError {}

/// Trait for handling session events (telemetry, logging, metrics).
///
/// Implementations can log to files, send to metrics services,
/// store for debugging, etc. Implementations should be fast and non-blocking.
#[async_trait]
pub trait SessionEventEmitter: Send + Sync {
    /// Emit a session event.
    async fn emit(&self, event: SessionEvent);

    /// Emit multiple events in order.
    async fn emit_all(&self, events: Vec<SessionEvent>) {
        for event in events {
            self.emit(event).await;
        }
    }
}

/// Trait for executing session outputs (RPC calls, hook responses).
///
/// Implementations translate `SessionOutput` commands into actual
/// CLN RPC calls, hook responses, etc.
#[async_trait]
pub trait SessionOutputHandler: Send + Sync {
    /// Execute a session output command.
    async fn execute(&self, output: SessionOutput) -> Result<(), SessionOutputError>;

    /// Execute multiple outputs in order. Stops on first error.
    async fn execute_all(&self, outputs: Vec<SessionOutput>) -> Result<(), SessionOutputError> {
        for output in outputs {
            self.execute(output).await?;
        }
        Ok(())
    }
}

/// No-op event emitter that discards all events.
///
/// Useful for testing when events don't need to be captured.
#[derive(Debug, Clone, Default)]
pub struct NoOpEventEmitter;

#[async_trait]
impl SessionEventEmitter for NoOpEventEmitter {
    async fn emit(&self, _event: SessionEvent) {
        // Intentionally empty - discard all events
    }
}

/// No-op output handler that succeeds without doing anything.
///
/// Useful for testing state transitions in isolation.
#[derive(Debug, Clone, Default)]
pub struct NoOpOutputHandler;

#[async_trait]
impl SessionOutputHandler for NoOpOutputHandler {
    async fn execute(&self, _output: SessionOutput) -> Result<(), SessionOutputError> {
        Ok(())
    }
}
