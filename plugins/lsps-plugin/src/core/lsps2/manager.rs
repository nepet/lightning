//! Session Manager for LSPS2 JIT Channel Sessions
//!
//! This module provides thread-safe management of multiple concurrent
//! JIT channel sessions, coordinating state transitions with event
//! emission and output execution.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::provider::{SessionEventEmitter, SessionOutputError, SessionOutputHandler};
use super::session::{
    ApplyResult, Session, SessionConfig, SessionEvent, SessionId, SessionInput, SessionPhase,
};

// ============================================================================
// Error Types
// ============================================================================

/// Errors that can occur during session management operations.
#[derive(Debug, Clone)]
pub enum SessionManagerError {
    /// Session not found
    NotFound(SessionId),
    /// Session already exists
    AlreadyExists(SessionId),
    /// Output execution failed
    OutputError(SessionOutputError),
    /// Session is in terminal state
    SessionTerminated(SessionId),
}

impl std::fmt::Display for SessionManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "session not found: {}", id),
            Self::AlreadyExists(id) => write!(f, "session already exists: {}", id),
            Self::OutputError(e) => write!(f, "output execution failed: {}", e),
            Self::SessionTerminated(id) => write!(f, "session is terminated: {}", id),
        }
    }
}

impl std::error::Error for SessionManagerError {}

impl From<SessionOutputError> for SessionManagerError {
    fn from(e: SessionOutputError) -> Self {
        SessionManagerError::OutputError(e)
    }
}

// ============================================================================
// Session Manager
// ============================================================================

/// Manages the lifecycle of JIT channel sessions.
///
/// The `SessionManager` provides thread-safe access to sessions and coordinates
/// state transitions with event emission and output execution. It is generic
/// over the event emitter and output handler traits for testability.
///
/// # Concurrency
///
/// The manager uses a single async mutex to protect the session map. The lock
/// is held only during session lookup and state transitions, and is released
/// before executing I/O operations (event emission, output execution).
///
/// # Example
///
/// ```ignore
/// let manager = SessionManager::new(
///     Arc::new(NoOpEventEmitter),
///     Arc::new(NoOpOutputHandler),
/// );
///
/// // Create a new session
/// manager.create_session(session_id, config).await?;
///
/// // Apply an input (e.g., HTLC arrived)
/// let phase = manager.apply_input(session_id, SessionInput::PartArrived { part }).await?;
/// ```
pub struct SessionManager<E, O>
where
    E: SessionEventEmitter,
    O: SessionOutputHandler,
{
    /// All active sessions, keyed by session ID (JIT SCID)
    sessions: Arc<Mutex<HashMap<SessionId, Session>>>,
    /// Event emitter for telemetry/logging
    event_emitter: Arc<E>,
    /// Output handler for executing commands
    output_handler: Arc<O>,
}

impl<E, O> SessionManager<E, O>
where
    E: SessionEventEmitter,
    O: SessionOutputHandler,
{
    /// Creates a new session manager with the given event emitter and output handler.
    pub fn new(event_emitter: Arc<E>, output_handler: Arc<O>) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            event_emitter,
            output_handler,
        }
    }

    /// Creates a new session with the given ID and configuration.
    ///
    /// Returns an error if a session with the same ID already exists.
    ///
    /// Emits a `SessionCreated` event upon successful creation.
    pub async fn create_session(
        &self,
        id: SessionId,
        config: SessionConfig,
    ) -> Result<(), SessionManagerError> {
        // Capture data for event before taking ownership
        let scid = id.0;
        let client_node_id = config.client_node_id;
        let payment_size_msat = config.expected_payment_size;
        let valid_until = config.valid_until();

        // Create session and insert under lock
        {
            let mut sessions = self.sessions.lock().await;
            if sessions.contains_key(&id) {
                return Err(SessionManagerError::AlreadyExists(id));
            }
            let session = Session::new(id, config);
            sessions.insert(id, session);
        } // Lock released

        // Emit creation event
        self.event_emitter
            .emit(SessionEvent::SessionCreated {
                session_id: id,
                scid,
                client_node_id,
                payment_size_msat,
                valid_until,
            })
            .await;

        Ok(())
    }

    /// Applies an input to a session, triggering a state transition.
    ///
    /// This is the main method for driving the state machine. It:
    /// 1. Looks up the session and applies the input
    /// 2. Emits any events produced by the transition
    /// 3. Executes any outputs produced by the transition
    ///
    /// The lock is released before I/O operations to avoid holding it
    /// during potentially slow RPC calls.
    ///
    /// Returns the new phase of the session after the transition.
    pub async fn apply_input(
        &self,
        id: SessionId,
        input: SessionInput,
    ) -> Result<SessionPhase, SessionManagerError> {
        // Apply input under lock, extract results
        let (result, new_phase): (ApplyResult, SessionPhase) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .get_mut(&id)
                .ok_or(SessionManagerError::NotFound(id))?;

            let result = session.apply(input);
            let phase = session.phase();
            (result, phase)
        }; // Lock released here

        // Emit events (no lock held)
        self.event_emitter.emit_all(result.events).await;

        // Execute outputs (no lock held)
        // Some outputs produce feedback inputs (e.g., OpenChannel → FundingSigned)
        log::debug!(
            "SessionManager::apply_input: executing {} outputs for session {:?}",
            result.outputs.len(),
            id
        );
        for output in &result.outputs {
            log::debug!("  Output: {:?}", std::mem::discriminant(output));
        }
        let feedbacks = self.output_handler.execute_all(result.outputs).await?;
        log::debug!(
            "SessionManager::apply_input: execute_all returned {} feedbacks",
            feedbacks.len()
        );

        // Apply feedback inputs to the same session
        let mut final_phase = new_phase;
        for feedback in feedbacks {
            let sub_result = {
                let mut sessions = self.sessions.lock().await;
                if let Some(session) = sessions.get_mut(&id) {
                    let result = session.apply(feedback);
                    final_phase = session.phase();
                    Some(result)
                } else {
                    None
                }
            };
            if let Some(sub_result) = sub_result {
                self.event_emitter.emit_all(sub_result.events).await;
                // Feedback transitions (FundingSigned, FundingBroadcasted) produce
                // no further outputs, so we don't recurse.
            }
        }

        Ok(final_phase)
    }

    /// Returns a clone of the session with the given ID, if it exists.
    ///
    /// This is useful for inspecting session state without holding the lock.
    pub async fn get_session(&self, id: SessionId) -> Option<Session> {
        let sessions = self.sessions.lock().await;
        sessions.get(&id).cloned()
    }

    /// Returns the current phase of the session with the given ID.
    ///
    /// This is a lightweight alternative to `get_session` when only the
    /// phase is needed.
    pub async fn get_session_phase(&self, id: SessionId) -> Option<SessionPhase> {
        let sessions = self.sessions.lock().await;
        sessions.get(&id).map(|s| s.phase())
    }

    /// Checks if a session with the given ID exists.
    pub async fn session_exists(&self, id: SessionId) -> bool {
        let sessions = self.sessions.lock().await;
        sessions.contains_key(&id)
    }

    /// Removes and returns the session with the given ID.
    ///
    /// This should typically only be called for sessions in terminal states
    /// (Done, Failed, Abandoned) during cleanup.
    pub async fn remove_session(&self, id: SessionId) -> Option<Session> {
        let mut sessions = self.sessions.lock().await;
        sessions.remove(&id)
    }

    /// Gets an existing session or creates a new one if it doesn't exist.
    ///
    /// The `config_fn` is only called if the session doesn't exist, allowing
    /// lazy creation of the configuration.
    ///
    /// Returns `Ok(true)` if a new session was created, `Ok(false)` if the
    /// session already existed.
    pub async fn get_or_create_session<F>(
        &self,
        id: SessionId,
        config_fn: F,
    ) -> Result<bool, SessionManagerError>
    where
        F: FnOnce() -> SessionConfig,
    {
        // Check and create atomically under a single lock to avoid TOCTOU race.
        // We hold the lock while calling config_fn() to prevent race conditions
        // where two concurrent calls both see "session doesn't exist" and then
        // both try to create it.
        let event_data = {
            let mut sessions = self.sessions.lock().await;
            if sessions.contains_key(&id) {
                None // Session already exists
            } else {
                // Session doesn't exist - create config and session while holding lock
                let config = config_fn();
                let scid = id.0;
                let client_node_id = config.client_node_id;
                let payment_size_msat = config.expected_payment_size;
                let valid_until = config.valid_until();

                let session = Session::new(id, config);
                sessions.insert(id, session);

                Some((scid, client_node_id, payment_size_msat, valid_until))
            }
        }; // Lock released

        // Emit creation event if we created a new session
        if let Some((scid, client_node_id, payment_size_msat, valid_until)) = event_data {
            self.event_emitter
                .emit(SessionEvent::SessionCreated {
                    session_id: id,
                    scid,
                    client_node_id,
                    payment_size_msat,
                    valid_until,
                })
                .await;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Returns the number of active (non-terminal) sessions.
    ///
    /// Useful for monitoring and metrics.
    pub async fn active_session_count(&self) -> usize {
        let sessions = self.sessions.lock().await;
        sessions.values().filter(|s| !s.is_terminal()).count()
    }

    /// Returns the total number of sessions (including terminal ones).
    pub async fn total_session_count(&self) -> usize {
        let sessions = self.sessions.lock().await;
        sessions.len()
    }

    /// Returns all session IDs.
    ///
    /// Useful for iteration or batch operations.
    pub async fn session_ids(&self) -> Vec<SessionId> {
        let sessions = self.sessions.lock().await;
        sessions.keys().copied().collect()
    }

    // ========================================================================
    // Lookup Methods (for notification handlers)
    // ========================================================================

    /// Find a session by its channel_id.
    ///
    /// Returns the SessionId of the first session with a matching channel_id.
    /// Used by `channel_state_changed` notification handler.
    pub async fn find_by_channel_id(&self, channel_id: &[u8; 32]) -> Option<SessionId> {
        let sessions = self.sessions.lock().await;
        for (id, session) in sessions.iter() {
            if let Some(cid) = session.channel_id() {
                if cid.as_bytes() == channel_id {
                    return Some(*id);
                }
            }
        }
        None
    }

    /// Find a session by payment_hash.
    ///
    /// Returns the SessionId of the first session with HTLCs matching
    /// the given payment_hash. Used by `forward_event` notification handler.
    pub async fn find_by_payment_hash(&self, payment_hash: &[u8; 32]) -> Option<SessionId> {
        let sessions = self.sessions.lock().await;
        for (id, session) in sessions.iter() {
            if let Some(hash) = session.payment_hash() {
                if &hash == payment_hash {
                    return Some(*id);
                }
            }
        }
        None
    }

    /// Find all sessions for a given peer (client node ID).
    ///
    /// Returns all SessionIds where the client_node_id matches.
    /// Used by `disconnect` notification handler.
    pub async fn find_by_peer_id(&self, peer_id: &bitcoin::secp256k1::PublicKey) -> Vec<SessionId> {
        let sessions = self.sessions.lock().await;
        sessions
            .iter()
            .filter(|(_, session)| session.client_node_id() == peer_id)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Removes all sessions in terminal states.
    ///
    /// Returns the number of sessions removed.
    pub async fn cleanup_terminal_sessions(&self) -> usize {
        let mut sessions = self.sessions.lock().await;
        let before = sessions.len();
        sessions.retain(|_, s| !s.is_terminal());
        before - sessions.len()
    }
}

// Allow cloning the manager (shares the same session map)
impl<E, O> Clone for SessionManager<E, O>
where
    E: SessionEventEmitter,
    O: SessionOutputHandler,
{
    fn clone(&self) -> Self {
        Self {
            sessions: Arc::clone(&self.sessions),
            event_emitter: Arc::clone(&self.event_emitter),
            output_handler: Arc::clone(&self.output_handler),
        }
    }
}

// ============================================================================
// Convenience Constructor
// ============================================================================

impl SessionManager<super::provider::NoOpEventEmitter, super::provider::NoOpOutputHandler> {
    /// Creates a new session manager with no-op event emitter and output handler.
    ///
    /// Useful for testing when events and outputs don't need to be captured.
    pub fn new_no_op() -> Self {
        Self::new(
            Arc::new(super::provider::NoOpEventEmitter),
            Arc::new(super::provider::NoOpOutputHandler),
        )
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::lsps2::provider::{NoOpEventEmitter, NoOpOutputHandler};
    use crate::core::lsps2::session::{HtlcPart, SessionEvent, SessionOutput};
    use crate::core::tlv::TlvStream;
    use crate::proto::lsps0::{Msat, Ppm, ShortChannelId};
    use crate::proto::lsps2::{OpeningFeeParams, Promise};
    use bitcoin::secp256k1::PublicKey;
    use chrono::{TimeZone, Utc};
    use std::sync::Mutex as StdMutex;
    use std::time::Instant;

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

    fn test_session_id() -> SessionId {
        SessionId::from(ShortChannelId::from(123u64))
    }

    fn test_config() -> SessionConfig {
        SessionConfig::new(
            test_public_key(),
            test_opening_fee_params(),
            Some(Msat::from_msat(100_000)),
        )
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

    // ========================================================================
    // Capturing implementations for testing
    // ========================================================================

    #[derive(Debug, Default)]
    struct CapturingEventEmitter {
        events: StdMutex<Vec<SessionEvent>>,
    }

    impl CapturingEventEmitter {
        fn events(&self) -> Vec<SessionEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl SessionEventEmitter for CapturingEventEmitter {
        async fn emit(&self, event: SessionEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[derive(Debug, Default)]
    struct CapturingOutputHandler {
        outputs: StdMutex<Vec<SessionOutput>>,
    }

    impl CapturingOutputHandler {
        fn outputs(&self) -> Vec<SessionOutput> {
            self.outputs.lock().unwrap().clone()
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
    // Tests
    // ========================================================================

    #[tokio::test]
    async fn test_create_session() {
        let manager = SessionManager::new_no_op();
        let id = test_session_id();

        let result = manager.create_session(id, test_config()).await;
        assert!(result.is_ok());
        assert!(manager.session_exists(id).await);
    }

    #[tokio::test]
    async fn test_create_duplicate_session() {
        let manager = SessionManager::new_no_op();
        let id = test_session_id();

        manager.create_session(id, test_config()).await.unwrap();
        let result = manager.create_session(id, test_config()).await;

        assert!(matches!(result, Err(SessionManagerError::AlreadyExists(_))));
    }

    #[tokio::test]
    async fn test_apply_input_not_found() {
        let manager = SessionManager::new_no_op();
        let id = test_session_id();

        let result = manager
            .apply_input(
                id,
                SessionInput::PartArrived {
                    part: test_htlc_part(1, 50_000),
                },
            )
            .await;

        assert!(matches!(result, Err(SessionManagerError::NotFound(_))));
    }

    #[tokio::test]
    async fn test_apply_input_transitions_state() {
        let manager = SessionManager::new_no_op();
        let id = test_session_id();

        manager.create_session(id, test_config()).await.unwrap();

        // First part - not enough for sum
        let phase = manager
            .apply_input(
                id,
                SessionInput::PartArrived {
                    part: test_htlc_part(1, 50_000),
                },
            )
            .await
            .unwrap();
        assert_eq!(phase, SessionPhase::Collecting);

        // Second part - reaches sum, transitions to Opening
        let phase = manager
            .apply_input(
                id,
                SessionInput::PartArrived {
                    part: test_htlc_part(2, 60_000),
                },
            )
            .await
            .unwrap();
        assert_eq!(phase, SessionPhase::Opening);
    }

    #[tokio::test]
    async fn test_apply_input_emits_events() {
        let emitter = Arc::new(CapturingEventEmitter::default());
        let handler = Arc::new(NoOpOutputHandler);
        let manager = SessionManager::new(emitter.clone(), handler);
        let id = test_session_id();

        manager.create_session(id, test_config()).await.unwrap();

        // Should have emitted SessionCreated
        let events = emitter.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], SessionEvent::SessionCreated { .. }));

        // Apply input that doesn't reach sum
        manager
            .apply_input(
                id,
                SessionInput::PartArrived {
                    part: test_htlc_part(1, 50_000),
                },
            )
            .await
            .unwrap();

        // Should have emitted HtlcPartReceived
        let events = emitter.events();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[1], SessionEvent::HtlcPartReceived { .. }));
    }

    #[tokio::test]
    async fn test_apply_input_executes_outputs() {
        let emitter = Arc::new(NoOpEventEmitter);
        let handler = Arc::new(CapturingOutputHandler::default());
        let manager = SessionManager::new(emitter, handler.clone());
        let id = test_session_id();

        manager.create_session(id, test_config()).await.unwrap();

        // Apply input that reaches sum - should trigger OpenChannel output
        manager
            .apply_input(
                id,
                SessionInput::PartArrived {
                    part: test_htlc_part(1, 100_000),
                },
            )
            .await
            .unwrap();

        let outputs = handler.outputs();
        assert_eq!(outputs.len(), 1);
        assert!(matches!(outputs[0], SessionOutput::OpenChannel { .. }));
    }

    #[tokio::test]
    async fn test_get_session() {
        let manager = SessionManager::new_no_op();
        let id = test_session_id();

        assert!(manager.get_session(id).await.is_none());

        manager.create_session(id, test_config()).await.unwrap();

        let session = manager.get_session(id).await;
        assert!(session.is_some());
        assert_eq!(session.unwrap().phase(), SessionPhase::Collecting);
    }

    #[tokio::test]
    async fn test_get_session_phase() {
        let manager = SessionManager::new_no_op();
        let id = test_session_id();

        assert!(manager.get_session_phase(id).await.is_none());

        manager.create_session(id, test_config()).await.unwrap();

        let phase = manager.get_session_phase(id).await;
        assert_eq!(phase, Some(SessionPhase::Collecting));
    }

    #[tokio::test]
    async fn test_remove_session() {
        let manager = SessionManager::new_no_op();
        let id = test_session_id();

        manager.create_session(id, test_config()).await.unwrap();
        assert!(manager.session_exists(id).await);

        let removed = manager.remove_session(id).await;
        assert!(removed.is_some());
        assert!(!manager.session_exists(id).await);

        // Removing again returns None
        assert!(manager.remove_session(id).await.is_none());
    }

    #[tokio::test]
    async fn test_get_or_create_existing() {
        let manager = SessionManager::new_no_op();
        let id = test_session_id();

        // Create first
        manager.create_session(id, test_config()).await.unwrap();

        // get_or_create should return false (already exists)
        let created = manager
            .get_or_create_session(id, || {
                panic!("config_fn should not be called for existing session")
            })
            .await
            .unwrap();

        assert!(!created);
    }

    #[tokio::test]
    async fn test_get_or_create_new() {
        let manager = SessionManager::new_no_op();
        let id = test_session_id();

        let config_called = Arc::new(StdMutex::new(false));
        let config_called_clone = config_called.clone();

        let created = manager
            .get_or_create_session(id, move || {
                *config_called_clone.lock().unwrap() = true;
                test_config()
            })
            .await
            .unwrap();

        assert!(created);
        assert!(*config_called.lock().unwrap());
        assert!(manager.session_exists(id).await);
    }

    #[tokio::test]
    async fn test_active_session_count() {
        let manager = SessionManager::new_no_op();

        assert_eq!(manager.active_session_count().await, 0);

        // Create a session
        let id1 = SessionId::from(ShortChannelId::from(1u64));
        manager.create_session(id1, test_config()).await.unwrap();
        assert_eq!(manager.active_session_count().await, 1);

        // Create another
        let id2 = SessionId::from(ShortChannelId::from(2u64));
        manager.create_session(id2, test_config()).await.unwrap();
        assert_eq!(manager.active_session_count().await, 2);

        // Transition one to terminal state
        manager
            .apply_input(
                id1,
                SessionInput::PartArrived {
                    part: test_htlc_part(1, 50_000),
                },
            )
            .await
            .unwrap();
        manager
            .apply_input(id1, SessionInput::CollectTimeout)
            .await
            .unwrap();

        // One session is now terminal
        assert_eq!(manager.active_session_count().await, 1);
        assert_eq!(manager.total_session_count().await, 2);
    }

    #[tokio::test]
    async fn test_cleanup_terminal_sessions() {
        let manager = SessionManager::new_no_op();

        // Create two sessions
        let id1 = SessionId::from(ShortChannelId::from(1u64));
        let id2 = SessionId::from(ShortChannelId::from(2u64));
        manager.create_session(id1, test_config()).await.unwrap();
        manager.create_session(id2, test_config()).await.unwrap();

        // Transition one to terminal
        manager
            .apply_input(
                id1,
                SessionInput::PartArrived {
                    part: test_htlc_part(1, 50_000),
                },
            )
            .await
            .unwrap();
        manager
            .apply_input(id1, SessionInput::CollectTimeout)
            .await
            .unwrap();

        assert_eq!(manager.total_session_count().await, 2);

        // Cleanup terminal sessions
        let removed = manager.cleanup_terminal_sessions().await;
        assert_eq!(removed, 1);
        assert_eq!(manager.total_session_count().await, 1);
        assert!(!manager.session_exists(id1).await);
        assert!(manager.session_exists(id2).await);
    }

    #[tokio::test]
    async fn test_session_ids() {
        let manager = SessionManager::new_no_op();

        let id1 = SessionId::from(ShortChannelId::from(1u64));
        let id2 = SessionId::from(ShortChannelId::from(2u64));

        manager.create_session(id1, test_config()).await.unwrap();
        manager.create_session(id2, test_config()).await.unwrap();

        let ids = manager.session_ids().await;
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id2));
    }

    #[tokio::test]
    async fn test_manager_clone_shares_state() {
        let manager1 = SessionManager::new_no_op();
        let manager2 = manager1.clone();
        let id = test_session_id();

        // Create via manager1
        manager1.create_session(id, test_config()).await.unwrap();

        // Should be visible via manager2
        assert!(manager2.session_exists(id).await);

        // Apply input via manager2
        manager2
            .apply_input(
                id,
                SessionInput::PartArrived {
                    part: test_htlc_part(1, 100_000),
                },
            )
            .await
            .unwrap();

        // State change visible via manager1
        assert_eq!(
            manager1.get_session_phase(id).await,
            Some(SessionPhase::Opening)
        );
    }

    #[tokio::test]
    async fn test_concurrent_access() {
        let manager = SessionManager::new_no_op();
        let id = test_session_id();

        // Create session with no-MPP (any single part triggers Opening)
        let config = SessionConfig::new(test_public_key(), test_opening_fee_params(), None);
        manager.create_session(id, config).await.unwrap();

        // Spawn multiple tasks that try to apply inputs concurrently
        let manager_clone = manager.clone();
        let handle1 = tokio::spawn(async move {
            manager_clone
                .apply_input(
                    id,
                    SessionInput::PartArrived {
                        part: test_htlc_part(1, 50_000),
                    },
                )
                .await
        });

        let manager_clone = manager.clone();
        let handle2 = tokio::spawn(async move {
            manager_clone
                .apply_input(
                    id,
                    SessionInput::PartArrived {
                        part: test_htlc_part(2, 50_000),
                    },
                )
                .await
        });

        // Both should complete without panicking
        let result1 = handle1.await.unwrap();
        let result2 = handle2.await.unwrap();

        // At least one should succeed
        assert!(result1.is_ok() || result2.is_ok());
    }
}
