//! CLN Implementation of SessionOutputHandler
//!
//! This module provides the CLN-specific implementation of the
//! `SessionOutputHandler` trait, which executes the outputs produced
//! by the session state machine.

use std::sync::Arc;

use async_trait::async_trait;
use log::{debug, warn};

use crate::core::lsps2::htlc_holder::HtlcHolder;
use crate::core::lsps2::provider::{SessionOutputError, SessionOutputHandler};
use crate::core::lsps2::session::SessionOutput;

// ============================================================================
// CLN Session Output Handler
// ============================================================================

/// CLN implementation of the session output handler.
///
/// This handler executes session outputs by calling the appropriate
/// CLN RPC methods or releasing held HTLCs via `HtlcHolder`.
///
/// # Implemented Outputs
///
/// - `ForwardHtlcs`: Releases held HTLCs with forward instructions
/// - `FailHtlcs`: Releases held HTLCs with failure codes
/// - `OpenChannel`: TODO - Will call fundchannel_start/fundchannel_complete
/// - `BroadcastFunding`: TODO - Will call sendpsbt
/// - `ReleaseChannel`: TODO - Will close/release the channel
pub struct ClnSessionOutputHandler {
    /// The HTLC holder for releasing held HTLCs
    htlc_holder: Arc<HtlcHolder>,
}

impl ClnSessionOutputHandler {
    /// Create a new CLN session output handler.
    ///
    /// # Arguments
    /// * `htlc_holder` - The HTLC holder for managing pending HTLCs
    pub fn new(htlc_holder: Arc<HtlcHolder>) -> Self {
        Self { htlc_holder }
    }
}

#[async_trait]
impl SessionOutputHandler for ClnSessionOutputHandler {
    async fn execute(&self, output: SessionOutput) -> Result<(), SessionOutputError> {
        match output {
            SessionOutput::OpenChannel {
                client_node_id,
                channel_size_sat,
            } => {
                // TODO: Implement channel opening via fundchannel_start/fundchannel_complete
                // This requires:
                // 1. Call fundchannel_start with the client_node_id and channel_size_sat
                // 2. Wait for the client to accept the channel
                // 3. Call fundchannel_complete with withheld=true
                //
                // For now, log and return success (placeholder)
                debug!(
                    "OpenChannel output received (not yet implemented): client_node_id={}, channel_size_sat={}",
                    client_node_id,
                    channel_size_sat
                );
                Ok(())
            }

            SessionOutput::ForwardHtlcs {
                session_id,
                instructions,
            } => {
                let count = instructions.len();
                let released = self
                    .htlc_holder
                    .release_forward(session_id, instructions)
                    .await;

                if released != count {
                    warn!(
                        "Mismatch between instructions and released HTLCs: session_id={:?}, expected={}, actual={}",
                        session_id,
                        count,
                        released
                    );
                }

                debug!(
                    "Released HTLCs with forward instructions: session_id={:?}, htlc_count={}",
                    session_id, released
                );
                Ok(())
            }

            SessionOutput::FailHtlcs {
                session_id,
                htlc_ids,
                failure_code,
            } => {
                let expected = htlc_ids.len();
                let released = self
                    .htlc_holder
                    .release_fail(session_id, failure_code)
                    .await;

                if released != expected {
                    warn!(
                        "Mismatch between htlc_ids and released HTLCs: session_id={:?}, expected={}, actual={}",
                        session_id,
                        expected,
                        released
                    );
                }

                debug!(
                    "Released HTLCs with failure: session_id={:?}, htlc_count={}, failure_code={:?}",
                    session_id,
                    released,
                    failure_code
                );
                Ok(())
            }

            SessionOutput::BroadcastFunding { psbt } => {
                // TODO: Implement funding broadcast via sendpsbt
                // This requires calling the sendpsbt RPC with the withheld PSBT
                //
                // For now, log and return success (placeholder)
                debug!(
                    "BroadcastFunding output received (not yet implemented): psbt_len={}",
                    psbt.len()
                );
                Ok(())
            }

            SessionOutput::ReleaseChannel { channel_id } => {
                // TODO: Implement channel release
                // This is needed for abandoned sessions where we need to
                // close the withheld channel and release the UTXOs
                //
                // For now, log and return success (placeholder)
                debug!(
                    "ReleaseChannel output received (not yet implemented): channel_id={:?}",
                    channel_id
                );
                Ok(())
            }
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::lsps2::htlc_holder::HtlcInfo;
    use crate::core::lsps2::session::{ChannelId, FailureCode, ForwardInstruction, SessionId};
    use crate::core::tlv::TlvStream;
    use crate::proto::lsps0::{Msat, ShortChannelId};
    use tokio::sync::oneshot;

    fn test_session_id() -> SessionId {
        SessionId::from(ShortChannelId::from(123u64))
    }

    fn test_htlc_info(htlc_id: u64) -> HtlcInfo {
        HtlcInfo {
            htlc_id,
            amount_msat: Msat::from_msat(100_000),
            cltv_expiry: 800_000,
            payment_hash: [1u8; 32],
            payload: TlvStream(vec![]),
        }
    }

    #[tokio::test]
    async fn test_forward_htlcs() {
        let htlc_holder = Arc::new(HtlcHolder::new());
        let handler = ClnSessionOutputHandler::new(htlc_holder.clone());

        let session_id = test_session_id();

        // Hold some HTLCs
        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();

        htlc_holder.hold(session_id, test_htlc_info(1), tx1).await;
        htlc_holder.hold(session_id, test_htlc_info(2), tx2).await;

        // Create forward instructions
        let instructions = vec![
            ForwardInstruction {
                htlc_id: 1,
                forward_to_channel_id: ChannelId([0u8; 32]),
                payload: TlvStream(vec![]),
                extra_tlvs: TlvStream(vec![]),
            },
            ForwardInstruction {
                htlc_id: 2,
                forward_to_channel_id: ChannelId([0u8; 32]),
                payload: TlvStream(vec![]),
                extra_tlvs: TlvStream(vec![]),
            },
        ];

        // Execute forward output
        let result = handler
            .execute(SessionOutput::ForwardHtlcs {
                session_id,
                instructions,
            })
            .await;

        assert!(result.is_ok());

        // Verify HTLCs were released with forward responses
        let resp1 = rx1.await.unwrap();
        let resp2 = rx2.await.unwrap();

        assert!(matches!(
            resp1,
            crate::core::lsps2::htlc_holder::HtlcResponse::Continue { .. }
        ));
        assert!(matches!(
            resp2,
            crate::core::lsps2::htlc_holder::HtlcResponse::Continue { .. }
        ));
    }

    #[tokio::test]
    async fn test_fail_htlcs() {
        let htlc_holder = Arc::new(HtlcHolder::new());
        let handler = ClnSessionOutputHandler::new(htlc_holder.clone());

        let session_id = test_session_id();

        // Hold some HTLCs
        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();

        htlc_holder.hold(session_id, test_htlc_info(1), tx1).await;
        htlc_holder.hold(session_id, test_htlc_info(2), tx2).await;

        // Execute fail output
        let result = handler
            .execute(SessionOutput::FailHtlcs {
                session_id,
                htlc_ids: vec![1, 2],
                failure_code: FailureCode::TemporaryChannelFailure,
            })
            .await;

        assert!(result.is_ok());

        // Verify HTLCs were released with failure responses
        let resp1 = rx1.await.unwrap();
        let resp2 = rx2.await.unwrap();

        assert!(matches!(
            resp1,
            crate::core::lsps2::htlc_holder::HtlcResponse::Fail {
                failure_code: FailureCode::TemporaryChannelFailure
            }
        ));
        assert!(matches!(
            resp2,
            crate::core::lsps2::htlc_holder::HtlcResponse::Fail {
                failure_code: FailureCode::TemporaryChannelFailure
            }
        ));
    }

    #[tokio::test]
    async fn test_forward_htlcs_no_pending() {
        let htlc_holder = Arc::new(HtlcHolder::new());
        let handler = ClnSessionOutputHandler::new(htlc_holder);

        let session_id = test_session_id();

        // Execute forward with no pending HTLCs
        let result = handler
            .execute(SessionOutput::ForwardHtlcs {
                session_id,
                instructions: vec![ForwardInstruction {
                    htlc_id: 1,
                    forward_to_channel_id: ChannelId([0u8; 32]),
                    payload: TlvStream(vec![]),
                    extra_tlvs: TlvStream(vec![]),
                }],
            })
            .await;

        // Should succeed but log a warning about mismatch
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_fail_htlcs_no_pending() {
        let htlc_holder = Arc::new(HtlcHolder::new());
        let handler = ClnSessionOutputHandler::new(htlc_holder);

        let session_id = test_session_id();

        // Execute fail with no pending HTLCs
        let result = handler
            .execute(SessionOutput::FailHtlcs {
                session_id,
                htlc_ids: vec![1, 2],
                failure_code: FailureCode::UnknownNextPeer,
            })
            .await;

        // Should succeed but log a warning about mismatch
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_open_channel_placeholder() {
        let htlc_holder = Arc::new(HtlcHolder::new());
        let handler = ClnSessionOutputHandler::new(htlc_holder);

        // Test that OpenChannel doesn't panic (placeholder implementation)
        let result = handler
            .execute(SessionOutput::OpenChannel {
                client_node_id: "02deadbeef".parse().unwrap_or_else(|_| {
                    // Use a valid test public key
                    bitcoin::secp256k1::PublicKey::from_slice(&[
                        2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                        1, 1, 1, 1, 1, 1, 1, 1,
                    ])
                    .unwrap()
                }),
                channel_size_sat: 100_000,
            })
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_broadcast_funding_placeholder() {
        let htlc_holder = Arc::new(HtlcHolder::new());
        let handler = ClnSessionOutputHandler::new(htlc_holder);

        // Test that BroadcastFunding doesn't panic (placeholder implementation)
        let result = handler
            .execute(SessionOutput::BroadcastFunding {
                psbt: "test_psbt".to_string(),
            })
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_release_channel_placeholder() {
        let htlc_holder = Arc::new(HtlcHolder::new());
        let handler = ClnSessionOutputHandler::new(htlc_holder);

        // Test that ReleaseChannel doesn't panic (placeholder implementation)
        let result = handler
            .execute(SessionOutput::ReleaseChannel {
                channel_id: ChannelId([1u8; 32]),
            })
            .await;

        assert!(result.is_ok());
    }
}
