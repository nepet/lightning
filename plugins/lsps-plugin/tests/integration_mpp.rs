//! Integration tests for MPP JIT Channel Session Flow
//!
//! These tests verify the complete MPP payment flow using mock implementations
//! of the provider traits. They test the integration between:
//! - SessionManager
//! - Session state machine
//! - HtlcHolder
//! - Event emission
//! - Output execution

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use bitcoin::secp256k1::PublicKey;
use chrono::{TimeZone, Utc};

use cln_lsps::core::lsps2::htlc_holder::HtlcHolder;
use cln_lsps::core::lsps2::manager::SessionManager;
use cln_lsps::core::lsps2::provider::{
    SessionEventEmitter, SessionOutputError, SessionOutputHandler,
};
use cln_lsps::core::lsps2::session::{
    ChannelId, FailureCode, HtlcPart, SessionConfig, SessionEvent, SessionId, SessionInput,
    SessionOutput, SessionPhase,
};
use cln_lsps::core::tlv::TlvStream;
use cln_lsps::proto::lsps0::{Msat, Ppm, ShortChannelId};
use cln_lsps::proto::lsps2::{OpeningFeeParams, Promise};

// ============================================================================
// Mock Blockheight Provider
// ============================================================================

/// Mock blockheight provider for testing timeout scenarios.
#[derive(Debug)]
pub struct MockBlockheightProvider {
    height: AtomicU32,
}

impl MockBlockheightProvider {
    pub fn new(initial_height: u32) -> Self {
        Self {
            height: AtomicU32::new(initial_height),
        }
    }

    pub fn get(&self) -> u32 {
        self.height.load(Ordering::SeqCst)
    }

    pub fn set(&self, height: u32) {
        self.height.store(height, Ordering::SeqCst);
    }

    pub fn advance(&self, blocks: u32) {
        self.height.fetch_add(blocks, Ordering::SeqCst);
    }
}

// ============================================================================
// Capturing Event Emitter
// ============================================================================

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

    /// Returns true if any event matches the predicate.
    pub fn has_event<F>(&self, predicate: F) -> bool
    where
        F: Fn(&SessionEvent) -> bool,
    {
        self.events.lock().unwrap().iter().any(predicate)
    }
}

#[async_trait]
impl SessionEventEmitter for CapturingEventEmitter {
    async fn emit(&self, event: SessionEvent) {
        self.events.lock().unwrap().push(event);
    }
}

// ============================================================================
// Simulating Output Handler
// ============================================================================

/// Output handler that simulates CLN behavior and captures outputs.
///
/// This handler:
/// - Captures all outputs for inspection
/// - Releases HTLCs via HtlcHolder when ForwardHtlcs/FailHtlcs are executed
/// - Can be configured to simulate failures
pub struct SimulatingOutputHandler {
    /// Captured outputs for inspection
    outputs: Mutex<Vec<SessionOutput>>,
    /// HTLC holder for releasing held HTLCs
    htlc_holder: Arc<HtlcHolder>,
    /// Whether OpenChannel should fail
    open_channel_fails: Mutex<bool>,
    /// Channel ID to return from OpenChannel
    next_channel_id: Mutex<Option<ChannelId>>,
}

impl SimulatingOutputHandler {
    pub fn new(htlc_holder: Arc<HtlcHolder>) -> Self {
        Self {
            outputs: Mutex::new(Vec::new()),
            htlc_holder,
            open_channel_fails: Mutex::new(false),
            next_channel_id: Mutex::new(None),
        }
    }

    /// Configure the handler to fail OpenChannel operations.
    pub fn set_open_channel_fails(&self, fails: bool) {
        *self.open_channel_fails.lock().unwrap() = fails;
    }

    /// Set the channel ID to return from OpenChannel.
    #[allow(dead_code)]
    pub fn set_next_channel_id(&self, channel_id: ChannelId) {
        *self.next_channel_id.lock().unwrap() = Some(channel_id);
    }

    /// Returns a clone of all captured outputs.
    pub fn outputs(&self) -> Vec<SessionOutput> {
        self.outputs.lock().unwrap().clone()
    }

    /// Clears captured outputs and returns them.
    pub fn take_outputs(&self) -> Vec<SessionOutput> {
        std::mem::take(&mut *self.outputs.lock().unwrap())
    }

    /// Returns true if any output matches the predicate.
    pub fn has_output<F>(&self, predicate: F) -> bool
    where
        F: Fn(&SessionOutput) -> bool,
    {
        self.outputs.lock().unwrap().iter().any(predicate)
    }

    /// Count outputs of a specific type.
    pub fn count_outputs<F>(&self, predicate: F) -> usize
    where
        F: Fn(&SessionOutput) -> bool,
    {
        self.outputs.lock().unwrap().iter().filter(|o| predicate(o)).count()
    }
}

#[async_trait]
impl SessionOutputHandler for SimulatingOutputHandler {
    async fn execute(
        &self,
        output: SessionOutput,
    ) -> Result<Option<SessionInput>, SessionOutputError> {
        // Capture the output first
        self.outputs.lock().unwrap().push(output.clone());

        match output {
            SessionOutput::OpenChannel { .. } => {
                if *self.open_channel_fails.lock().unwrap() {
                    return Err(SessionOutputError::ChannelError(
                        "simulated channel open failure".to_string(),
                    ));
                }
                // In real implementation, this would call fundchannel_start/complete
                // For tests, we just record the output and the test will inject
                // FundingSigned input to continue the flow
                Ok(None)
            }

            SessionOutput::ForwardHtlcs {
                session_id,
                instructions,
            } => {
                // Release HTLCs with forward instructions
                // The alias_scid is already in each ForwardInstruction
                self.htlc_holder
                    .release_forward(session_id, instructions)
                    .await;
                Ok(None)
            }

            SessionOutput::FailHtlcs {
                session_id,
                failure_code,
                ..
            } => {
                // Release HTLCs with failure
                self.htlc_holder.release_fail(session_id, failure_code).await;
                Ok(None)
            }

            SessionOutput::BroadcastFunding { .. } => {
                // Just record, no action needed in tests
                Ok(None)
            }

            SessionOutput::ReleaseChannel { .. } => {
                // Just record, no action needed in tests
                Ok(None)
            }
        }
    }
}

impl std::fmt::Debug for SimulatingOutputHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimulatingOutputHandler")
            .field("outputs", &self.outputs)
            .field("open_channel_fails", &self.open_channel_fails)
            .finish()
    }
}

// ============================================================================
// Test Helpers
// ============================================================================

fn test_public_key() -> PublicKey {
    "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
        .parse()
        .unwrap()
}

fn test_opening_fee_params() -> OpeningFeeParams {
    OpeningFeeParams {
        min_fee_msat: Msat::from_msat(1000),
        proportional: Ppm::from_ppm(1000), // 0.1%
        valid_until: Utc.with_ymd_and_hms(2100, 1, 1, 0, 0, 0).unwrap(),
        min_lifetime: 144,
        max_client_to_self_delay: 2016,
        min_payment_size_msat: Msat::from_msat(10_000),
        max_payment_size_msat: Msat::from_msat(100_000_000),
        promise: Promise::try_from("test_promise_hex_string_here").unwrap(),
    }
}

fn test_session_id() -> SessionId {
    SessionId::from(ShortChannelId::from(123456789u64))
}

fn test_channel_id() -> ChannelId {
    ChannelId([1u8; 32])
}

fn test_payment_hash() -> [u8; 32] {
    [0xab; 32]
}

/// Create an HTLC part for testing.
fn test_htlc_part(htlc_id: u64, amount_msat: u64) -> HtlcPart {
    HtlcPart {
        htlc_id,
        amount_msat: Msat::from_msat(amount_msat),
        cltv_expiry: 800_000, // Far in the future
        payment_hash: test_payment_hash(),
        arrived_at: Instant::now(),
        onion_payload: TlvStream::default(),
        extra_tlvs: TlvStream::default(),
        in_channel: ShortChannelId::from(111u64),
    }
}

/// Create a session config for MPP testing.
fn test_mpp_config(expected_payment_size: u64) -> SessionConfig {
    SessionConfig::new(
        test_public_key(),
        test_opening_fee_params(),
        Some(Msat::from_msat(expected_payment_size)),
    )
}

/// Create a session manager with capturing event emitter and simulating output handler.
fn create_test_manager() -> (
    SessionManager<CapturingEventEmitter, SimulatingOutputHandler>,
    Arc<CapturingEventEmitter>,
    Arc<SimulatingOutputHandler>,
    Arc<HtlcHolder>,
) {
    let htlc_holder = Arc::new(HtlcHolder::new());
    let event_emitter = Arc::new(CapturingEventEmitter::new());
    let output_handler = Arc::new(SimulatingOutputHandler::new(htlc_holder.clone()));

    let manager = SessionManager::new(event_emitter.clone(), output_handler.clone());

    (manager, event_emitter, output_handler, htlc_holder)
}

// ============================================================================
// Integration Tests
// ============================================================================

/// Test the complete happy path for an MPP payment:
/// Collecting → Opening → AwaitingChannelReady → Forwarding → WaitingPreimage → Settling → Done
#[tokio::test]
async fn test_mpp_payment_happy_path() {
    let (manager, events, outputs, _htlc_holder) = create_test_manager();
    let session_id = test_session_id();

    // Expected total payment: 1,000,000 msat (two parts of 500,000 each)
    let config = test_mpp_config(1_000_000);

    // Configure output handler with channel info
        
    // Step 1: Create session
    manager.create_session(session_id, config).await.unwrap();
    assert_eq!(
        manager.get_session_phase(session_id).await.unwrap(),
        SessionPhase::Collecting
    );

    // Verify session_created event
    assert!(events.has_event(|e| matches!(e, SessionEvent::SessionCreated { .. })));

    // Step 2: First HTLC part arrives (500,000 msat)
    let part1 = test_htlc_part(1, 500_000);
    let phase = manager
        .apply_input(session_id, SessionInput::PartArrived { part: part1 })
        .await
        .unwrap();
    assert_eq!(phase, SessionPhase::Collecting);

    // Verify htlc_part_received event
    assert!(events.has_event(|e| matches!(e, SessionEvent::HtlcPartReceived { .. })));

    // Step 3: Second HTLC part arrives (500,000 msat) - triggers SumReached
    let part2 = test_htlc_part(2, 500_000);
    let phase = manager
        .apply_input(session_id, SessionInput::PartArrived { part: part2 })
        .await
        .unwrap();
    assert_eq!(phase, SessionPhase::Opening);

    // Verify channel_open_initiated event and OpenChannel output
    assert!(events.has_event(|e| matches!(e, SessionEvent::ChannelOpenInitiated { .. })));
    assert!(outputs.has_output(|o| matches!(o, SessionOutput::OpenChannel { .. })));

    // Step 4: Funding signed (channel negotiation complete)
    let phase = manager
        .apply_input(
            session_id,
            SessionInput::FundingSigned {
                channel_id: test_channel_id(),
                funding_txid: "0000000000000000000000000000000000000000000000000000000000000001"
                    .parse()
                    .unwrap(),
                funding_outpoint: 0,
                funding_psbt: "test_psbt".to_string(),
            },
        )
        .await
        .unwrap();
    assert_eq!(phase, SessionPhase::AwaitingChannelReady);

    // Verify channel_funding_signed event
    assert!(events.has_event(|e| matches!(e, SessionEvent::ChannelFundingSigned { .. })));

    // Step 5: Channel ready (channel_state_changed to CHANNELD_NORMAL)
    let phase = manager
        .apply_input(
            session_id,
            SessionInput::ClientChannelReady {
                alias_scid: ShortChannelId::from(999u64),
            },
        )
        .await
        .unwrap();
    assert_eq!(phase, SessionPhase::Forwarding);

    // Verify channel_ready event
    assert!(events.has_event(|e| matches!(e, SessionEvent::ChannelReady { .. })));

    // Step 6: HTLCs forwarded (ForwardHtlcs output should have been generated)
    // The state machine auto-transitions to WaitingPreimage after forwarding
    let phase = manager
        .apply_input(session_id, SessionInput::ForwardsCommitted)
        .await
        .unwrap();
    assert_eq!(phase, SessionPhase::WaitingPreimage);

    // Verify ForwardHtlcs output and htlcs_forwarded event
    assert!(outputs.has_output(|o| matches!(o, SessionOutput::ForwardHtlcs { .. })));
    assert!(events.has_event(|e| matches!(e, SessionEvent::HtlcsForwarded { .. })));

    // Step 7: Preimage received (forward_event with status=settled)
    let preimage = [0xcc; 32];
    let phase = manager
        .apply_input(session_id, SessionInput::PreimageReceived { preimage })
        .await
        .unwrap();
    assert_eq!(phase, SessionPhase::Settling);

    // Verify htlcs_fulfilled event
    assert!(events.has_event(|e| matches!(e, SessionEvent::HtlcsFulfilled { .. })));

    // Step 8: Funding broadcast
    let phase = manager
        .apply_input(session_id, SessionInput::FundingBroadcasted {
            txid: "0000000000000000000000000000000000000000000000000000000000000001"
                .parse()
                .unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(phase, SessionPhase::Done);

    // Verify BroadcastFunding output, channel_funding_broadcast and session_completed events
    assert!(outputs.has_output(|o| matches!(o, SessionOutput::BroadcastFunding { .. })));
    assert!(events.has_event(|e| matches!(e, SessionEvent::ChannelFundingBroadcast { .. })));
    assert!(events.has_event(|e| matches!(e, SessionEvent::SessionCompleted { .. })));

    // Verify session is terminal
    let session = manager.get_session(session_id).await.unwrap();
    assert!(session.is_terminal());
}

/// Test that collect timeout fails HTLCs and terminates the session.
#[tokio::test]
async fn test_collect_timeout_fails_session() {
    let (manager, events, outputs, _htlc_holder) = create_test_manager();
    let session_id = test_session_id();
    let config = test_mpp_config(1_000_000);

    // Create session and add one part (not enough for sum)
    manager.create_session(session_id, config).await.unwrap();

    let part1 = test_htlc_part(1, 500_000);
    manager
        .apply_input(session_id, SessionInput::PartArrived { part: part1 })
        .await
        .unwrap();

    // Apply collect timeout
    let phase = manager
        .apply_input(session_id, SessionInput::CollectTimeout)
        .await
        .unwrap();

    assert_eq!(phase, SessionPhase::Failed);

    // Verify events and outputs
    // Note: The session emits specific events (CollectTimeout, HtlcsFailedUpstream)
    // rather than a generic SessionFailed event
    assert!(events.has_event(|e| matches!(e, SessionEvent::CollectTimeout { .. })));
    assert!(events.has_event(|e| matches!(e, SessionEvent::HtlcsFailedUpstream { .. })));
    assert!(outputs.has_output(|o| matches!(
        o,
        SessionOutput::FailHtlcs {
            failure_code: FailureCode::TemporaryChannelFailure,
            ..
        }
    )));
}

/// Test that valid_until expiration fails the session.
#[tokio::test]
async fn test_valid_until_expiration_fails_session() {
    let (manager, events, outputs, _htlc_holder) = create_test_manager();
    let session_id = test_session_id();
    let config = test_mpp_config(1_000_000);

    manager.create_session(session_id, config).await.unwrap();

    let part1 = test_htlc_part(1, 500_000);
    manager
        .apply_input(session_id, SessionInput::PartArrived { part: part1 })
        .await
        .unwrap();

    // Apply valid_until expiration
    let phase = manager
        .apply_input(session_id, SessionInput::ValidUntilPassed)
        .await
        .unwrap();

    assert_eq!(phase, SessionPhase::Failed);

    // Verify events
    // Note: The session emits specific events rather than a generic SessionFailed event
    assert!(events.has_event(|e| matches!(e, SessionEvent::ValidUntilExpired { .. })));
    assert!(events.has_event(|e| matches!(e, SessionEvent::HtlcsFailedUpstream { .. })));
    assert!(outputs.has_output(|o| matches!(
        o,
        SessionOutput::FailHtlcs {
            failure_code: FailureCode::UnknownNextPeer,
            ..
        }
    )));
}

/// Test that unsafe hold detection fails the session.
#[tokio::test]
async fn test_unsafe_hold_fails_session() {
    let (manager, events, _outputs, _htlc_holder) = create_test_manager();
    let session_id = test_session_id();
    let config = test_mpp_config(1_000_000);

    manager.create_session(session_id, config).await.unwrap();

    let part1 = test_htlc_part(1, 500_000);
    manager
        .apply_input(session_id, SessionInput::PartArrived { part: part1 })
        .await
        .unwrap();

    // Apply unsafe hold (CLTV too close)
    let phase = manager
        .apply_input(
            session_id,
            SessionInput::UnsafeHold {
                min_cltv_expiry: 800_010,
                current_height: 800_005,
            },
        )
        .await
        .unwrap();

    assert_eq!(phase, SessionPhase::Failed);

    // Verify events
    // Note: The session emits specific events rather than a generic SessionFailed event
    assert!(events.has_event(|e| matches!(e, SessionEvent::UnsafeHoldDetected { .. })));
    assert!(events.has_event(|e| matches!(e, SessionEvent::HtlcsFailedUpstream { .. })));
}

/// Test client disconnect during Opening phase fails the session.
#[tokio::test]
async fn test_client_disconnect_during_opening() {
    let (manager, events, _outputs, _htlc_holder) = create_test_manager();
    let session_id = test_session_id();
    let config = test_mpp_config(1_000_000);

    // Get to Opening state
    manager.create_session(session_id, config).await.unwrap();
    manager
        .apply_input(
            session_id,
            SessionInput::PartArrived {
                part: test_htlc_part(1, 500_000),
            },
        )
        .await
        .unwrap();
    manager
        .apply_input(
            session_id,
            SessionInput::PartArrived {
                part: test_htlc_part(2, 500_000),
            },
        )
        .await
        .unwrap();

    assert_eq!(
        manager.get_session_phase(session_id).await.unwrap(),
        SessionPhase::Opening
    );

    // Client disconnects before funding_signed
    let phase = manager
        .apply_input(session_id, SessionInput::ClientDisconnected)
        .await
        .unwrap();

    assert_eq!(phase, SessionPhase::Failed);
    // Note: The session emits specific events rather than a generic SessionFailed event
    assert!(events.has_event(|e| matches!(e, SessionEvent::ClientDisconnected { .. })));
    assert!(events.has_event(|e| matches!(e, SessionEvent::HtlcsFailedUpstream { .. })));
}

/// Test client disconnect during AwaitingChannelReady fails the session.
#[tokio::test]
async fn test_client_disconnect_awaiting_channel_ready() {
    let (manager, events, _outputs, _htlc_holder) = create_test_manager();
    let session_id = test_session_id();
    let config = test_mpp_config(1_000_000);

    // Get to AwaitingChannelReady state
    manager.create_session(session_id, config).await.unwrap();
    manager
        .apply_input(
            session_id,
            SessionInput::PartArrived {
                part: test_htlc_part(1, 1_000_000),
            },
        )
        .await
        .unwrap();
    manager
        .apply_input(
            session_id,
            SessionInput::FundingSigned {
                channel_id: test_channel_id(),
                funding_txid: "0000000000000000000000000000000000000000000000000000000000000001"
                    .parse()
                    .unwrap(),
                funding_outpoint: 0,
                funding_psbt: "test_psbt".to_string(),
            },
        )
        .await
        .unwrap();

    assert_eq!(
        manager.get_session_phase(session_id).await.unwrap(),
        SessionPhase::AwaitingChannelReady
    );

    // Client disconnects
    let phase = manager
        .apply_input(session_id, SessionInput::ClientDisconnected)
        .await
        .unwrap();

    assert_eq!(phase, SessionPhase::Failed);
    assert!(events.has_event(|e| matches!(e, SessionEvent::ClientDisconnected { .. })));
}

/// Test that client rejection moves to AwaitingRetry and can recover.
#[tokio::test]
async fn test_retry_after_client_rejection() {
    let (manager, events, outputs, _htlc_holder) = create_test_manager();
    let session_id = test_session_id();
    let config = test_mpp_config(1_000_000);

        
    // Get to WaitingPreimage state
    manager.create_session(session_id, config).await.unwrap();
    manager
        .apply_input(
            session_id,
            SessionInput::PartArrived {
                part: test_htlc_part(1, 1_000_000),
            },
        )
        .await
        .unwrap();
    manager
        .apply_input(
            session_id,
            SessionInput::FundingSigned {
                channel_id: test_channel_id(),
                funding_txid: "0000000000000000000000000000000000000000000000000000000000000001"
                    .parse()
                    .unwrap(),
                funding_outpoint: 0,
                funding_psbt: "test_psbt".to_string(),
            },
        )
        .await
        .unwrap();
    manager
        .apply_input(
            session_id,
            SessionInput::ClientChannelReady {
                alias_scid: ShortChannelId::from(999u64),
            },
        )
        .await
        .unwrap();
    manager
        .apply_input(session_id, SessionInput::ForwardsCommitted)
        .await
        .unwrap();

    assert_eq!(
        manager.get_session_phase(session_id).await.unwrap(),
        SessionPhase::WaitingPreimage
    );

    // Client rejects the payment
    let phase = manager
        .apply_input(session_id, SessionInput::ClientRejectedPayment)
        .await
        .unwrap();

    assert_eq!(phase, SessionPhase::AwaitingRetry);
    assert!(events.has_event(|e| matches!(e, SessionEvent::HtlcsRejectedByClient { .. })));

    // Clear events/outputs for retry tracking
    events.take_events();
    outputs.take_outputs();

    // Retry: New HTLC parts arrive
    let phase = manager
        .apply_input(
            session_id,
            SessionInput::PartArrived {
                part: test_htlc_part(3, 1_000_000), // New HTLC ID
            },
        )
        .await
        .unwrap();

    // Sum reached again, should go to Forwarding (channel already exists)
    assert_eq!(phase, SessionPhase::Forwarding);

    // Continue to completion
    manager
        .apply_input(session_id, SessionInput::ForwardsCommitted)
        .await
        .unwrap();
    manager
        .apply_input(
            session_id,
            SessionInput::PreimageReceived {
                preimage: [0xdd; 32],
            },
        )
        .await
        .unwrap();
    let phase = manager
        .apply_input(session_id, SessionInput::FundingBroadcasted {
            txid: "0000000000000000000000000000000000000000000000000000000000000001"
                .parse()
                .unwrap(),
        })
        .await
        .unwrap();

    assert_eq!(phase, SessionPhase::Done);
    assert!(events.has_event(|e| matches!(e, SessionEvent::SessionCompleted { .. })));
}

/// Test that too many parts fails the session.
#[tokio::test]
async fn test_too_many_parts_fails_session() {
    let (manager, events, outputs, _htlc_holder) = create_test_manager();
    let session_id = test_session_id();
    let config = test_mpp_config(1_000_000);

    manager.create_session(session_id, config).await.unwrap();

    // Add one part
    manager
        .apply_input(
            session_id,
            SessionInput::PartArrived {
                part: test_htlc_part(1, 100_000),
            },
        )
        .await
        .unwrap();

    // Apply TooManyParts
    let phase = manager
        .apply_input(session_id, SessionInput::TooManyParts { max_parts: 10 })
        .await
        .unwrap();

    assert_eq!(phase, SessionPhase::Failed);
    assert!(events.has_event(|e| matches!(e, SessionEvent::TooManyParts { .. })));
    assert!(outputs.has_output(|o| matches!(
        o,
        SessionOutput::FailHtlcs {
            failure_code: FailureCode::UnknownNextPeer,
            ..
        }
    )));
}

/// Test that AwaitingRetry with valid_until expiration moves to Abandoned.
#[tokio::test]
async fn test_awaiting_retry_valid_until_abandons() {
    let (manager, events, outputs, _htlc_holder) = create_test_manager();
    let session_id = test_session_id();
    let config = test_mpp_config(1_000_000);

        
    // Get to AwaitingRetry state
    manager.create_session(session_id, config).await.unwrap();
    manager
        .apply_input(
            session_id,
            SessionInput::PartArrived {
                part: test_htlc_part(1, 1_000_000),
            },
        )
        .await
        .unwrap();
    manager
        .apply_input(
            session_id,
            SessionInput::FundingSigned {
                channel_id: test_channel_id(),
                funding_txid: "0000000000000000000000000000000000000000000000000000000000000001"
                    .parse()
                    .unwrap(),
                funding_outpoint: 0,
                funding_psbt: "test_psbt".to_string(),
            },
        )
        .await
        .unwrap();
    manager
        .apply_input(
            session_id,
            SessionInput::ClientChannelReady {
                alias_scid: ShortChannelId::from(999u64),
            },
        )
        .await
        .unwrap();
    manager
        .apply_input(session_id, SessionInput::ForwardsCommitted)
        .await
        .unwrap();
    manager
        .apply_input(session_id, SessionInput::ClientRejectedPayment)
        .await
        .unwrap();

    assert_eq!(
        manager.get_session_phase(session_id).await.unwrap(),
        SessionPhase::AwaitingRetry
    );

    // Valid until expires while awaiting retry
    let phase = manager
        .apply_input(session_id, SessionInput::ValidUntilPassed)
        .await
        .unwrap();

    assert_eq!(phase, SessionPhase::Abandoned);
    // Note: The session emits specific events rather than a generic SessionAbandoned event
    assert!(events.has_event(|e| matches!(e, SessionEvent::ValidUntilExpired { .. })));
    assert!(events.has_event(|e| matches!(e, SessionEvent::ChannelReleased { .. })));
    assert!(outputs.has_output(|o| matches!(o, SessionOutput::ReleaseChannel { .. })));
}
