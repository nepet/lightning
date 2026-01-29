//! HTLC Hold/Release Mechanism for MPP Session Management
//!
//! This module provides a mechanism to hold HTLC hook responses until the
//! session state machine decides how to handle them. CLN's `htlc_accepted`
//! hook is synchronous, so we need to defer the response until the session
//! transitions to a state where we know what to do (forward or fail).
//!
//! # Architecture
//!
//! When an HTLC arrives:
//! 1. The hook handler creates a oneshot channel
//! 2. The sender is stored in `HtlcHolder` along with HTLC metadata
//! 3. The hook handler awaits on the receiver
//! 4. When the session transitions, `HtlcHolder::release_*` sends the response
//! 5. The hook handler receives the response and returns it to CLN

use std::collections::HashMap;

use tokio::sync::oneshot;

use crate::core::lsps2::session::{FailureCode, ForwardInstruction, SessionId};
use crate::core::tlv::TlvStream;
use crate::proto::lsps0::{Msat, ShortChannelId};

// ============================================================================
// HTLC Response Types
// ============================================================================

/// Response to send back to the htlc_accepted hook.
#[derive(Debug, Clone)]
pub enum HtlcResponse {
    /// Continue forwarding the HTLC with modified payload.
    Continue {
        /// The short_channel_id to forward to (the new channel's alias)
        forward_to: ShortChannelId,
        /// The modified onion payload
        payload: TlvStream,
        /// Extra TLVs to add (e.g., opening fee TLV 65537)
        extra_tlvs: TlvStream,
    },

    /// Fail the HTLC back upstream.
    Fail {
        /// The failure code to use
        failure_code: FailureCode,
    },
}

// ============================================================================
// Pending HTLC Types
// ============================================================================

/// Metadata about a held HTLC needed for later processing.
#[derive(Debug, Clone)]
pub struct HtlcInfo {
    /// The HTLC ID from the incoming HTLC
    pub htlc_id: u64,
    /// The amount in millisatoshis
    pub amount_msat: Msat,
    /// The CLTV expiry block height
    pub cltv_expiry: u32,
    /// The payment hash (32 bytes)
    pub payment_hash: [u8; 32],
    /// The original onion payload (may be modified for forwarding)
    pub payload: TlvStream,
}

/// A pending HTLC waiting for a response.
pub struct PendingHtlc {
    /// Metadata about the HTLC
    pub info: HtlcInfo,
    /// Channel to send the response back to the hook handler
    responder: oneshot::Sender<HtlcResponse>,
}

impl PendingHtlc {
    /// Create a new pending HTLC.
    pub fn new(info: HtlcInfo, responder: oneshot::Sender<HtlcResponse>) -> Self {
        Self { info, responder }
    }

    /// Send a response and consume the pending HTLC.
    ///
    /// Returns `Err` if the receiver was dropped (hook timed out or was cancelled).
    pub fn respond(self, response: HtlcResponse) -> Result<(), HtlcResponse> {
        self.responder.send(response)
    }
}

impl std::fmt::Debug for PendingHtlc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingHtlc")
            .field("info", &self.info)
            .field("responder", &"<oneshot::Sender>")
            .finish()
    }
}

// ============================================================================
// HTLC Holder
// ============================================================================

/// Manages pending HTLCs by session, allowing them to be held and released.
///
/// This is the bridge between the htlc_accepted hook and the session state
/// machine. When HTLCs arrive, they're held here until the session decides
/// what to do with them.
///
/// # Thread Safety
///
/// `HtlcHolder` uses internal synchronization via `tokio::sync::Mutex` and
/// is safe to use from multiple tasks. It's designed to be wrapped in an `Arc`
/// for sharing.
#[derive(Debug, Default)]
pub struct HtlcHolder {
    /// Pending HTLCs indexed by session ID
    pending: tokio::sync::Mutex<HashMap<SessionId, Vec<PendingHtlc>>>,
}

impl HtlcHolder {
    /// Create a new empty HTLC holder.
    pub fn new() -> Self {
        Self {
            pending: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Hold an HTLC for later processing.
    ///
    /// The HTLC will be stored until one of the release methods is called,
    /// or until it's explicitly removed.
    ///
    /// # Arguments
    /// * `session_id` - The session this HTLC belongs to
    /// * `info` - Metadata about the HTLC
    /// * `responder` - Channel to send the response when ready
    pub async fn hold(
        &self,
        session_id: SessionId,
        info: HtlcInfo,
        responder: oneshot::Sender<HtlcResponse>,
    ) {
        let pending_htlc = PendingHtlc::new(info, responder);
        let mut pending = self.pending.lock().await;
        pending.entry(session_id).or_default().push(pending_htlc);
    }

    /// Release all HTLCs for a session with forward instructions.
    ///
    /// Each HTLC is matched with its corresponding forward instruction by
    /// `htlc_id`. HTLCs without a matching instruction are failed with
    /// `TemporaryChannelFailure`.
    ///
    /// # Arguments
    /// * `session_id` - The session whose HTLCs to release
    /// * `instructions` - Forward instructions indexed by htlc_id
    ///
    /// # Returns
    /// The number of HTLCs released (both forwarded and failed).
    pub async fn release_forward(
        &self,
        session_id: SessionId,
        instructions: Vec<ForwardInstruction>,
    ) -> usize {
        let mut pending = self.pending.lock().await;
        let Some(htlcs) = pending.remove(&session_id) else {
            return 0;
        };

        // Build a map of htlc_id -> instruction for quick lookup
        let instruction_map: HashMap<u64, ForwardInstruction> =
            instructions.into_iter().map(|i| (i.htlc_id, i)).collect();

        let count = htlcs.len();
        for htlc in htlcs {
            let response = if let Some(instruction) = instruction_map.get(&htlc.info.htlc_id) {
                HtlcResponse::Continue {
                    forward_to: instruction.forward_to_scid,
                    payload: instruction.payload.clone(),
                    extra_tlvs: instruction.extra_tlvs.clone(),
                }
            } else {
                // No instruction for this HTLC - fail it
                HtlcResponse::Fail {
                    failure_code: FailureCode::TemporaryChannelFailure,
                }
            };

            // Ignore send errors - the receiver may have timed out
            let _ = htlc.respond(response);
        }

        count
    }

    /// Release all HTLCs for a session with a failure code.
    ///
    /// All HTLCs for the session will be failed with the given failure code.
    ///
    /// # Arguments
    /// * `session_id` - The session whose HTLCs to release
    /// * `failure_code` - The failure code to use for all HTLCs
    ///
    /// # Returns
    /// The number of HTLCs released.
    pub async fn release_fail(&self, session_id: SessionId, failure_code: FailureCode) -> usize {
        let mut pending = self.pending.lock().await;
        let Some(htlcs) = pending.remove(&session_id) else {
            return 0;
        };

        let count = htlcs.len();
        for htlc in htlcs {
            let response = HtlcResponse::Fail { failure_code };
            // Ignore send errors - the receiver may have timed out
            let _ = htlc.respond(response);
        }

        count
    }

    /// Get the number of pending HTLCs for a session.
    pub async fn count(&self, session_id: &SessionId) -> usize {
        let pending = self.pending.lock().await;
        pending.get(session_id).map(|v| v.len()).unwrap_or(0)
    }

    /// Get the total number of pending HTLCs across all sessions.
    pub async fn total_count(&self) -> usize {
        let pending = self.pending.lock().await;
        pending.values().map(|v| v.len()).sum()
    }

    /// Get all pending HTLC info for a session (without removing them).
    ///
    /// This is useful for inspecting the current state without modifying it.
    pub async fn get_htlc_info(&self, session_id: &SessionId) -> Vec<HtlcInfo> {
        let pending = self.pending.lock().await;
        pending
            .get(session_id)
            .map(|htlcs| htlcs.iter().map(|h| h.info.clone()).collect())
            .unwrap_or_default()
    }

    /// Remove all pending HTLCs for a session without responding.
    ///
    /// The oneshot senders will be dropped, causing the receivers to get
    /// an error. Use this when the hook handlers have already timed out
    /// or been cancelled.
    ///
    /// # Returns
    /// The number of HTLCs removed.
    pub async fn remove(&self, session_id: &SessionId) -> usize {
        let mut pending = self.pending.lock().await;
        pending.remove(session_id).map(|v| v.len()).unwrap_or(0)
    }

    /// Get all session IDs that have pending HTLCs.
    pub async fn session_ids(&self) -> Vec<SessionId> {
        let pending = self.pending.lock().await;
        pending.keys().cloned().collect()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn test_hold_and_count() {
        let holder = HtlcHolder::new();
        let session_id = test_session_id();

        assert_eq!(holder.count(&session_id).await, 0);

        let (tx1, _rx1) = oneshot::channel();
        let (tx2, _rx2) = oneshot::channel();

        holder.hold(session_id, test_htlc_info(1), tx1).await;
        assert_eq!(holder.count(&session_id).await, 1);

        holder.hold(session_id, test_htlc_info(2), tx2).await;
        assert_eq!(holder.count(&session_id).await, 2);
    }

    #[tokio::test]
    async fn test_release_forward() {
        let holder = HtlcHolder::new();
        let session_id = test_session_id();

        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();

        holder.hold(session_id, test_htlc_info(1), tx1).await;
        holder.hold(session_id, test_htlc_info(2), tx2).await;

        let instructions = vec![
            ForwardInstruction {
                htlc_id: 1,
                forward_to_scid: ShortChannelId::from(456u64),
                payload: TlvStream(vec![]),
                extra_tlvs: TlvStream(vec![]),
            },
            ForwardInstruction {
                htlc_id: 2,
                forward_to_scid: ShortChannelId::from(456u64),
                payload: TlvStream(vec![]),
                extra_tlvs: TlvStream(vec![]),
            },
        ];

        let released = holder.release_forward(session_id, instructions).await;
        assert_eq!(released, 2);
        assert_eq!(holder.count(&session_id).await, 0);

        // Check responses
        let resp1 = rx1.await.unwrap();
        let resp2 = rx2.await.unwrap();

        assert!(matches!(resp1, HtlcResponse::Continue { .. }));
        assert!(matches!(resp2, HtlcResponse::Continue { .. }));
    }

    #[tokio::test]
    async fn test_release_forward_missing_instruction() {
        let holder = HtlcHolder::new();
        let session_id = test_session_id();

        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();

        holder.hold(session_id, test_htlc_info(1), tx1).await;
        holder.hold(session_id, test_htlc_info(2), tx2).await;

        // Only provide instruction for htlc_id 1
        let instructions = vec![ForwardInstruction {
            htlc_id: 1,
            forward_to_scid: ShortChannelId::from(456u64),
            payload: TlvStream(vec![]),
            extra_tlvs: TlvStream(vec![]),
        }];

        let released = holder.release_forward(session_id, instructions).await;
        assert_eq!(released, 2);

        // HTLC 1 should be forwarded
        let resp1 = rx1.await.unwrap();
        assert!(matches!(resp1, HtlcResponse::Continue { .. }));

        // HTLC 2 should be failed (no instruction)
        let resp2 = rx2.await.unwrap();
        assert!(matches!(
            resp2,
            HtlcResponse::Fail {
                failure_code: FailureCode::TemporaryChannelFailure
            }
        ));
    }

    #[tokio::test]
    async fn test_release_fail() {
        let holder = HtlcHolder::new();
        let session_id = test_session_id();

        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();

        holder.hold(session_id, test_htlc_info(1), tx1).await;
        holder.hold(session_id, test_htlc_info(2), tx2).await;

        let released = holder
            .release_fail(session_id, FailureCode::UnknownNextPeer)
            .await;
        assert_eq!(released, 2);
        assert_eq!(holder.count(&session_id).await, 0);

        // Check responses
        let resp1 = rx1.await.unwrap();
        let resp2 = rx2.await.unwrap();

        assert!(matches!(
            resp1,
            HtlcResponse::Fail {
                failure_code: FailureCode::UnknownNextPeer
            }
        ));
        assert!(matches!(
            resp2,
            HtlcResponse::Fail {
                failure_code: FailureCode::UnknownNextPeer
            }
        ));
    }

    #[tokio::test]
    async fn test_release_nonexistent_session() {
        let holder = HtlcHolder::new();
        let session_id = test_session_id();

        let released = holder
            .release_fail(session_id, FailureCode::TemporaryChannelFailure)
            .await;
        assert_eq!(released, 0);

        let released = holder.release_forward(session_id, vec![]).await;
        assert_eq!(released, 0);
    }

    #[tokio::test]
    async fn test_remove() {
        let holder = HtlcHolder::new();
        let session_id = test_session_id();

        let (tx1, rx1) = oneshot::channel();
        let (tx2, _rx2) = oneshot::channel();

        holder.hold(session_id, test_htlc_info(1), tx1).await;
        holder.hold(session_id, test_htlc_info(2), tx2).await;

        let removed = holder.remove(&session_id).await;
        assert_eq!(removed, 2);
        assert_eq!(holder.count(&session_id).await, 0);

        // Receivers should get an error (sender dropped)
        let result = rx1.await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_htlc_info() {
        let holder = HtlcHolder::new();
        let session_id = test_session_id();

        let (tx1, _rx1) = oneshot::channel();
        let (tx2, _rx2) = oneshot::channel();

        holder.hold(session_id, test_htlc_info(1), tx1).await;
        holder.hold(session_id, test_htlc_info(2), tx2).await;

        let infos = holder.get_htlc_info(&session_id).await;
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].htlc_id, 1);
        assert_eq!(infos[1].htlc_id, 2);
    }

    #[tokio::test]
    async fn test_multiple_sessions() {
        let holder = HtlcHolder::new();
        let session1 = SessionId::from(ShortChannelId::from(100u64));
        let session2 = SessionId::from(ShortChannelId::from(200u64));

        let (tx1, _rx1) = oneshot::channel();
        let (tx2, _rx2) = oneshot::channel();
        let (tx3, rx3) = oneshot::channel();

        holder.hold(session1, test_htlc_info(1), tx1).await;
        holder.hold(session1, test_htlc_info(2), tx2).await;
        holder.hold(session2, test_htlc_info(3), tx3).await;

        assert_eq!(holder.count(&session1).await, 2);
        assert_eq!(holder.count(&session2).await, 1);
        assert_eq!(holder.total_count().await, 3);

        // Release session1, session2 should be unaffected
        holder
            .release_fail(session1, FailureCode::TemporaryChannelFailure)
            .await;

        assert_eq!(holder.count(&session1).await, 0);
        assert_eq!(holder.count(&session2).await, 1);
        assert_eq!(holder.total_count().await, 1);

        // Session2's HTLC should still be pending
        let infos = holder.get_htlc_info(&session2).await;
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].htlc_id, 3);

        // Release session2
        let instructions = vec![ForwardInstruction {
            htlc_id: 3,
            forward_to_scid: ShortChannelId::from(456u64),
            payload: TlvStream(vec![]),
            extra_tlvs: TlvStream(vec![]),
        }];
        holder.release_forward(session2, instructions).await;

        let resp = rx3.await.unwrap();
        assert!(matches!(resp, HtlcResponse::Continue { .. }));
    }

    #[tokio::test]
    async fn test_session_ids() {
        let holder = HtlcHolder::new();
        let session1 = SessionId::from(ShortChannelId::from(100u64));
        let session2 = SessionId::from(ShortChannelId::from(200u64));

        assert!(holder.session_ids().await.is_empty());

        let (tx1, _rx1) = oneshot::channel();
        let (tx2, _rx2) = oneshot::channel();

        holder.hold(session1, test_htlc_info(1), tx1).await;
        holder.hold(session2, test_htlc_info(2), tx2).await;

        let ids = holder.session_ids().await;
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&session1));
        assert!(ids.contains(&session2));
    }

    #[tokio::test]
    async fn test_dropped_receiver() {
        let holder = HtlcHolder::new();
        let session_id = test_session_id();

        let (tx, rx) = oneshot::channel();
        holder.hold(session_id, test_htlc_info(1), tx).await;

        // Drop the receiver before releasing
        drop(rx);

        // Release should not panic even with dropped receiver
        let released = holder
            .release_fail(session_id, FailureCode::TemporaryChannelFailure)
            .await;
        assert_eq!(released, 1);
    }
}
