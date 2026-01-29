//! CLN Implementation of SessionOutputHandler
//!
//! This module provides the CLN-specific implementation of the
//! `SessionOutputHandler` trait, which executes the outputs produced
//! by the session state machine.

use std::sync::Arc;

use async_trait::async_trait;
use log::{debug, info, warn};

use crate::core::lsps2::htlc_holder::HtlcHolder;
use crate::core::lsps2::provider::{LightningProvider, SessionOutputError, SessionOutputHandler};
use crate::core::lsps2::psbt::{add_funding_output, extract_funding_info, P2WSH_OUTPUT_WEIGHT};
use crate::core::lsps2::session::{ChannelId, SessionInput, SessionOutput};

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
/// - `OpenChannel`: Executes withheld funding flow (fundchannel_start → fundpsbt →
///   add_funding_output → fundchannel_complete_withheld → signpsbt)
/// - `ForwardHtlcs`: Releases held HTLCs with forward instructions
/// - `FailHtlcs`: Releases held HTLCs with failure codes
/// - `BroadcastFunding`: TODO - Will call sendpsbt
/// - `ReleaseChannel`: TODO - Will close/release the channel
pub struct ClnSessionOutputHandler {
    /// The HTLC holder for releasing held HTLCs
    htlc_holder: Arc<HtlcHolder>,
    /// The lightning provider for RPC calls
    provider: Arc<dyn LightningProvider>,
}

impl ClnSessionOutputHandler {
    /// Create a new CLN session output handler.
    ///
    /// # Arguments
    /// * `htlc_holder` - The HTLC holder for managing pending HTLCs
    /// * `provider` - The lightning provider for RPC calls
    pub fn new(htlc_holder: Arc<HtlcHolder>, provider: Arc<dyn LightningProvider>) -> Self {
        Self {
            htlc_holder,
            provider,
        }
    }
}

#[async_trait]
impl SessionOutputHandler for ClnSessionOutputHandler {
    async fn execute(
        &self,
        output: SessionOutput,
    ) -> Result<Option<SessionInput>, SessionOutputError> {
        match output {
            SessionOutput::OpenChannel {
                client_node_id,
                channel_size_sat,
            } => {
                info!(
                    "Opening withheld channel: peer={}, size={}sat",
                    client_node_id, channel_size_sat
                );

                // Step 1: Initiate channel funding, get the funding scriptpubkey
                let start_result = self
                    .provider
                    .fund_channel_start(&client_node_id, channel_size_sat, false, Some(0))
                    .await
                    .map_err(|e| {
                        SessionOutputError::ChannelError(format!(
                            "fundchannel_start failed: {}",
                            e
                        ))
                    })?;

                debug!(
                    "fundchannel_start succeeded: scriptpubkey={}",
                    &start_result.scriptpubkey
                );

                // Step 2: Select and reserve wallet UTXOs
                let fund_result = self
                    .provider
                    .fund_psbt(channel_size_sat, "normal", P2WSH_OUTPUT_WEIGHT)
                    .await
                    .map_err(|e| {
                        SessionOutputError::ChannelError(format!("fundpsbt failed: {}", e))
                    })?;

                debug!(
                    "fundpsbt succeeded: feerate_per_kw={}, weight={}",
                    fund_result.feerate_per_kw, fund_result.estimated_final_weight
                );

                // Step 3: Add the funding output to the PSBT
                let assembled_psbt =
                    add_funding_output(&fund_result.psbt, channel_size_sat, &start_result.scriptpubkey)
                        .map_err(|e| {
                            SessionOutputError::ChannelError(format!(
                                "PSBT assembly failed: {}",
                                e
                            ))
                        })?;

                // Step 4: Extract funding txid and outpoint from the assembled PSBT
                let (funding_txid, funding_outpoint) =
                    extract_funding_info(&assembled_psbt).map_err(|e| {
                        SessionOutputError::ChannelError(format!(
                            "extracting funding info failed: {}",
                            e
                        ))
                    })?;

                debug!(
                    "PSBT assembled: funding_txid={}, funding_outpoint={}",
                    funding_txid, funding_outpoint
                );

                // Step 5: Complete channel negotiation with withheld broadcast
                let complete_result = self
                    .provider
                    .fund_channel_complete_withheld(&client_node_id, &assembled_psbt)
                    .await
                    .map_err(|e| {
                        SessionOutputError::ChannelError(format!(
                            "fundchannel_complete (withheld) failed: {}",
                            e
                        ))
                    })?;

                debug!(
                    "fundchannel_complete succeeded: channel_id={:?}",
                    hex::encode(complete_result.channel_id)
                );

                // Step 6: Sign the PSBT (wallet inputs)
                let sign_result = self
                    .provider
                    .sign_psbt(&assembled_psbt)
                    .await
                    .map_err(|e| {
                        SessionOutputError::ChannelError(format!("signpsbt failed: {}", e))
                    })?;

                info!(
                    "Withheld channel opened: channel_id={}, funding_txid={}",
                    hex::encode(complete_result.channel_id),
                    funding_txid
                );

                // Return feedback: FundingSigned transitions Opening → AwaitingChannelReady
                Ok(Some(SessionInput::FundingSigned {
                    channel_id: ChannelId(complete_result.channel_id),
                    funding_txid,
                    funding_outpoint,
                    funding_psbt: sign_result.signed_psbt,
                }))
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
                Ok(None)
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
                Ok(None)
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
                Ok(None)
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
                Ok(None)
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
    use crate::core::lsps2::provider::{
        ChannelInfo, FundChannelCompleteResult, FundChannelStartResult, FundPsbtResult,
        SendPsbtResult, SignPsbtResult,
    };
    use bitcoin::hashes::sha256::Hash;
    use crate::core::lsps2::session::{ChannelId, FailureCode, ForwardInstruction, SessionId};
    use crate::core::tlv::TlvStream;
    use crate::proto::lsps0::{Msat, ShortChannelId};
    use anyhow::Result as AnyResult;
    use bitcoin::secp256k1::PublicKey;
    use tokio::sync::oneshot;

    /// Mock provider that panics on any call.
    /// Used for tests that don't exercise the OpenChannel path.
    struct PanicProvider;

    #[async_trait]
    impl LightningProvider for PanicProvider {
        async fn fund_jit_channel(
            &self,
            _: &PublicKey,
            _: &Msat,
        ) -> AnyResult<(Hash, String)> {
            unimplemented!("not needed for this test")
        }
        async fn is_channel_ready(&self, _: &PublicKey, _: &Hash) -> AnyResult<bool> {
            unimplemented!("not needed for this test")
        }
        async fn fund_channel_start(
            &self,
            _: &PublicKey,
            _: u64,
            _: bool,
            _: Option<u32>,
        ) -> AnyResult<FundChannelStartResult> {
            unimplemented!("not needed for this test")
        }
        async fn fund_channel_complete_withheld(
            &self,
            _: &PublicKey,
            _: &str,
        ) -> AnyResult<FundChannelCompleteResult> {
            unimplemented!("not needed for this test")
        }
        async fn broadcast_funding(&self, _: &str) -> AnyResult<SendPsbtResult> {
            unimplemented!("not needed for this test")
        }
        async fn get_channel_info(
            &self,
            _: &PublicKey,
            _: Option<&[u8; 32]>,
        ) -> AnyResult<Option<ChannelInfo>> {
            unimplemented!("not needed for this test")
        }
        async fn fund_psbt(&self, _: u64, _: &str, _: u32) -> AnyResult<FundPsbtResult> {
            unimplemented!("not needed for this test")
        }
        async fn sign_psbt(&self, _: &str) -> AnyResult<SignPsbtResult> {
            unimplemented!("not needed for this test")
        }
        async fn unreserve_inputs(&self, _: &str) -> AnyResult<()> {
            unimplemented!("not needed for this test")
        }
        async fn close_channel(&self, _: &[u8; 32]) -> AnyResult<()> {
            unimplemented!("not needed for this test")
        }
    }

    fn mock_provider() -> Arc<dyn LightningProvider> {
        Arc::new(PanicProvider)
    }

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
        let handler = ClnSessionOutputHandler::new(htlc_holder.clone(), mock_provider());

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
        let handler = ClnSessionOutputHandler::new(htlc_holder.clone(), mock_provider());

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
        let handler = ClnSessionOutputHandler::new(htlc_holder, mock_provider());

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
        let handler = ClnSessionOutputHandler::new(htlc_holder, mock_provider());

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

    /// Mock provider that returns valid responses for the OpenChannel flow.
    struct OpenChannelMockProvider;

    #[async_trait]
    impl LightningProvider for OpenChannelMockProvider {
        async fn fund_jit_channel(
            &self,
            _: &PublicKey,
            _: &Msat,
        ) -> AnyResult<(Hash, String)> {
            unimplemented!()
        }
        async fn is_channel_ready(&self, _: &PublicKey, _: &Hash) -> AnyResult<bool> {
            unimplemented!()
        }
        async fn fund_channel_start(
            &self,
            _: &PublicKey,
            _: u64,
            _: bool,
            _: Option<u32>,
        ) -> AnyResult<FundChannelStartResult> {
            // Return a P2WSH scriptpubkey (OP_0 <32-byte hash>)
            let scriptpubkey = "0020".to_string() + &"ab".repeat(32);
            Ok(FundChannelStartResult {
                funding_address: "bc1qtest".to_string(),
                scriptpubkey,
                mindepth: Some(0),
            })
        }
        async fn fund_channel_complete_withheld(
            &self,
            _: &PublicKey,
            _: &str,
        ) -> AnyResult<FundChannelCompleteResult> {
            Ok(FundChannelCompleteResult {
                channel_id: [42u8; 32],
                commitments_secured: true,
            })
        }
        async fn broadcast_funding(&self, _: &str) -> AnyResult<SendPsbtResult> {
            unimplemented!()
        }
        async fn get_channel_info(
            &self,
            _: &PublicKey,
            _: Option<&[u8; 32]>,
        ) -> AnyResult<Option<ChannelInfo>> {
            unimplemented!()
        }
        async fn fund_psbt(&self, _: u64, _: &str, _: u32) -> AnyResult<FundPsbtResult> {
            // Create a minimal valid PSBT with one input
            use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
            use bitcoin::blockdata::transaction::{Transaction, TxIn};
            use bitcoin::psbt::{Input as PsbtInput, Psbt};
            use bitcoin::{absolute, transaction};

            let tx = Transaction {
                version: transaction::Version(2),
                lock_time: absolute::LockTime::ZERO,
                input: vec![TxIn::default()],
                output: vec![],
            };
            let psbt = Psbt {
                unsigned_tx: tx,
                version: 0,
                xpub: Default::default(),
                proprietary: Default::default(),
                unknown: Default::default(),
                inputs: vec![PsbtInput::default()],
                outputs: vec![],
            };
            Ok(FundPsbtResult {
                psbt: BASE64.encode(&psbt.serialize()),
                feerate_per_kw: 2500,
                estimated_final_weight: 800,
            })
        }
        async fn sign_psbt(&self, psbt: &str) -> AnyResult<SignPsbtResult> {
            // Return the same PSBT as "signed"
            Ok(SignPsbtResult {
                signed_psbt: psbt.to_string(),
            })
        }
        async fn unreserve_inputs(&self, _: &str) -> AnyResult<()> {
            Ok(())
        }
        async fn close_channel(&self, _: &[u8; 32]) -> AnyResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_open_channel_withheld_flow() {
        let htlc_holder = Arc::new(HtlcHolder::new());
        let provider: Arc<dyn LightningProvider> = Arc::new(OpenChannelMockProvider);
        let handler = ClnSessionOutputHandler::new(htlc_holder, provider);

        let client_node_id = bitcoin::secp256k1::PublicKey::from_slice(&[
            2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
            1, 1, 1, 1, 1,
        ])
        .unwrap();

        let result = handler
            .execute(SessionOutput::OpenChannel {
                client_node_id,
                channel_size_sat: 100_000,
            })
            .await;

        assert!(result.is_ok());
        let feedback = result.unwrap();
        assert!(feedback.is_some());

        // Verify the feedback is a FundingSigned input
        match feedback.unwrap() {
            SessionInput::FundingSigned {
                channel_id,
                funding_txid,
                funding_outpoint,
                funding_psbt,
            } => {
                assert_eq!(channel_id, ChannelId([42u8; 32]));
                // The funding output is the only output (index 0)
                assert_eq!(funding_outpoint, 0);
                // Txid should be non-empty
                assert!(!funding_txid.to_string().is_empty());
                // The signed PSBT should be non-empty
                assert!(!funding_psbt.is_empty());
            }
            other => panic!("Expected FundingSigned, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_open_channel_fundchannel_start_failure() {
        /// Mock that fails on fund_channel_start
        struct FailingStartProvider;

        #[async_trait]
        impl LightningProvider for FailingStartProvider {
            async fn fund_jit_channel(
                &self,
                _: &PublicKey,
                _: &Msat,
            ) -> AnyResult<(Hash, String)> {
                unimplemented!()
            }
            async fn is_channel_ready(&self, _: &PublicKey, _: &Hash) -> AnyResult<bool> {
                unimplemented!()
            }
            async fn fund_channel_start(
                &self,
                _: &PublicKey,
                _: u64,
                _: bool,
                _: Option<u32>,
            ) -> AnyResult<FundChannelStartResult> {
                anyhow::bail!("peer disconnected")
            }
            async fn fund_channel_complete_withheld(
                &self,
                _: &PublicKey,
                _: &str,
            ) -> AnyResult<FundChannelCompleteResult> {
                unimplemented!()
            }
            async fn broadcast_funding(&self, _: &str) -> AnyResult<SendPsbtResult> {
                unimplemented!()
            }
            async fn get_channel_info(
                &self,
                _: &PublicKey,
                _: Option<&[u8; 32]>,
            ) -> AnyResult<Option<ChannelInfo>> {
                unimplemented!()
            }
            async fn fund_psbt(&self, _: u64, _: &str, _: u32) -> AnyResult<FundPsbtResult> {
                unimplemented!()
            }
            async fn sign_psbt(&self, _: &str) -> AnyResult<SignPsbtResult> {
                unimplemented!()
            }
            async fn unreserve_inputs(&self, _: &str) -> AnyResult<()> {
                unimplemented!()
            }
            async fn close_channel(&self, _: &[u8; 32]) -> AnyResult<()> {
                unimplemented!()
            }
        }

        let htlc_holder = Arc::new(HtlcHolder::new());
        let provider: Arc<dyn LightningProvider> = Arc::new(FailingStartProvider);
        let handler = ClnSessionOutputHandler::new(htlc_holder, provider);

        let client_node_id = bitcoin::secp256k1::PublicKey::from_slice(&[
            2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
            1, 1, 1, 1, 1,
        ])
        .unwrap();

        let result = handler
            .execute(SessionOutput::OpenChannel {
                client_node_id,
                channel_size_sat: 100_000,
            })
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("fundchannel_start failed"),
            "Expected fundchannel_start error, got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_broadcast_funding_placeholder() {
        let htlc_holder = Arc::new(HtlcHolder::new());
        let handler = ClnSessionOutputHandler::new(htlc_holder, mock_provider());

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
        let handler = ClnSessionOutputHandler::new(htlc_holder, mock_provider());

        // Test that ReleaseChannel doesn't panic (placeholder implementation)
        let result = handler
            .execute(SessionOutput::ReleaseChannel {
                channel_id: ChannelId([1u8; 32]),
            })
            .await;

        assert!(result.is_ok());
    }
}
