use anyhow::Result;
use async_trait::async_trait;
use bitcoin::hashes::sha256::Hash;
use bitcoin::secp256k1::PublicKey;

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
    async fn fund_jit_channel(&self, peer_id: &PublicKey, amount: &Msat) -> Result<(Hash, String)>;
    async fn is_channel_ready(&self, peer_id: &PublicKey, channel_id: &Hash) -> Result<bool>;
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
