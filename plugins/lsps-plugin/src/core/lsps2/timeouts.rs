//! Timeout Management for LSPS2 JIT Channel Sessions
//!
//! This module provides a background task that monitors active sessions
//! for timeout conditions and triggers appropriate state transitions.
//!
//! # Timeout Types
//!
//! 1. **Collect Timeout**: 90 seconds from the first HTLC part arrival.
//!    Applies to `Collecting` and `AwaitingRetry` states.
//!
//! 2. **Valid Until**: Absolute deadline from the opening_fee_params.
//!    Applies to all non-terminal states.
//!
//! 3. **Unsafe Hold**: CLTV expiry too close to current block height.
//!    Applies to all non-terminal states with HTLC parts.
//!
//! # Usage
//!
//! ```ignore
//! let manager = TimeoutManager::new(
//!     session_manager,
//!     blockheight_provider,
//!     TimeoutConfig::default(),
//! );
//!
//! // Start background checking
//! let handle = manager.spawn();
//!
//! // ... later, to stop
//! handle.abort();
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use log::{debug, trace, warn};
use tokio::task::JoinHandle;

use super::manager::SessionManager;
use super::provider::{BlockheightProvider, SessionEventEmitter, SessionOutputHandler};
use super::session::{Session, SessionInput, SessionState};

// ============================================================================
// Configuration
// ============================================================================

/// Configuration for timeout checks.
#[derive(Debug, Clone)]
pub struct TimeoutConfig {
    /// How often to check for timeouts.
    ///
    /// Default: 1 second
    pub check_interval: Duration,

    /// Collect timeout duration (time since first HTLC part).
    ///
    /// LSPS2 spec: 90 seconds
    pub collect_timeout: Duration,

    /// Minimum CLTV margin before triggering UnsafeHold.
    ///
    /// If the minimum CLTV expiry of held HTLCs is less than
    /// `current_height + min_cltv_margin`, trigger UnsafeHold.
    ///
    /// Default: 2 blocks (per LSPS2 spec)
    pub min_cltv_margin: u32,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            check_interval: Duration::from_secs(1),
            collect_timeout: Duration::from_secs(90),
            min_cltv_margin: 2,
        }
    }
}

impl TimeoutConfig {
    /// Create a new timeout configuration with custom values.
    pub fn new(check_interval: Duration, collect_timeout: Duration, min_cltv_margin: u32) -> Self {
        Self {
            check_interval,
            collect_timeout,
            min_cltv_margin,
        }
    }

    /// Create a configuration for testing with shorter timeouts.
    #[cfg(test)]
    pub fn for_testing() -> Self {
        Self {
            check_interval: Duration::from_millis(10),
            collect_timeout: Duration::from_millis(100),
            min_cltv_margin: 2,
        }
    }
}

// ============================================================================
// Timeout Manager
// ============================================================================

/// Manages timeout detection for JIT channel sessions.
///
/// The `TimeoutManager` runs as a background task, periodically checking
/// all active sessions for timeout conditions. When a timeout is detected,
/// it applies the appropriate `SessionInput` to trigger a state transition.
///
/// # Priority Order
///
/// Timeouts are checked in the following priority order:
/// 1. **Unsafe Hold** (most urgent - blocks can be mined any time)
/// 2. **Valid Until** (absolute deadline from opening_fee_params)
/// 3. **Collect Timeout** (90 seconds from first part)
///
/// Only the first detected timeout is applied per check cycle.
pub struct TimeoutManager<E, O, B>
where
    E: SessionEventEmitter + 'static,
    O: SessionOutputHandler + 'static,
    B: BlockheightProvider + 'static,
{
    session_manager: SessionManager<E, O>,
    blockheight_provider: Arc<B>,
    config: TimeoutConfig,
}

impl<E, O, B> TimeoutManager<E, O, B>
where
    E: SessionEventEmitter + 'static,
    O: SessionOutputHandler + 'static,
    B: BlockheightProvider + 'static,
{
    /// Create a new timeout manager.
    ///
    /// # Arguments
    ///
    /// * `session_manager` - The session manager to check and update
    /// * `blockheight_provider` - Provider for current block height
    /// * `config` - Timeout configuration
    pub fn new(
        session_manager: SessionManager<E, O>,
        blockheight_provider: Arc<B>,
        config: TimeoutConfig,
    ) -> Self {
        Self {
            session_manager,
            blockheight_provider,
            config,
        }
    }

    /// Start the timeout checker as a background task.
    ///
    /// Returns a `JoinHandle` that can be used to await completion or abort
    /// the task. The task runs indefinitely until aborted.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let handle = timeout_manager.spawn();
    ///
    /// // Later, to stop the background task:
    /// handle.abort();
    /// ```
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(async move {
            self.run().await;
        })
    }

    /// Run the timeout checker loop.
    ///
    /// This method runs indefinitely, checking for timeouts at the configured
    /// interval. It is typically called via `spawn()` rather than directly.
    async fn run(&self) {
        debug!(
            "Timeout manager started (interval={:?}, collect_timeout={:?}, cltv_margin={})",
            self.config.check_interval, self.config.collect_timeout, self.config.min_cltv_margin
        );

        let mut interval = tokio::time::interval(self.config.check_interval);

        loop {
            interval.tick().await;

            if let Err(e) = self.check_all_sessions().await {
                warn!("Timeout check failed: {}", e);
            }
        }
    }

    /// Check all active sessions for timeout conditions.
    ///
    /// This method iterates through all sessions and applies timeout inputs
    /// where appropriate. It's called periodically by the background task.
    pub async fn check_all_sessions(&self) -> Result<(), TimeoutError> {
        // Get current block height
        let current_height = self
            .blockheight_provider
            .get_blockheight()
            .await
            .map_err(|e| TimeoutError::BlockheightFetch(e.to_string()))?;

        let now = Instant::now();
        let utc_now = Utc::now();

        // Get snapshot of all session IDs
        let session_ids = self.session_manager.session_ids().await;

        trace!(
            "Checking {} sessions for timeouts (height={})",
            session_ids.len(),
            current_height
        );

        for session_id in session_ids {
            // Get session snapshot
            let Some(session) = self.session_manager.get_session(session_id).await else {
                continue; // Session removed between snapshot and check
            };

            // Skip terminal sessions
            if session.is_terminal() {
                continue;
            }

            // Determine which timeout input to apply (if any)
            if let Some(input) = self.check_session(&session, current_height, now, utc_now) {
                debug!(
                    "Timeout detected for session {:?}: {:?}",
                    session_id,
                    input_description(&input)
                );

                // Apply the timeout input
                if let Err(e) = self.session_manager.apply_input(session_id, input).await {
                    warn!("Failed to apply timeout to session {:?}: {}", session_id, e);
                }
            }
        }

        Ok(())
    }

    /// Check a single session for timeout conditions.
    ///
    /// Returns the first applicable timeout input, or `None` if no timeout
    /// condition is met. Checks are performed in priority order.
    fn check_session(
        &self,
        session: &Session,
        current_height: u32,
        now: Instant,
        utc_now: chrono::DateTime<Utc>,
    ) -> Option<SessionInput> {
        // 1. Check unsafe hold (most urgent - blocks may be mined any time)
        if let Some(input) = self.check_unsafe_hold(session, current_height) {
            return Some(input);
        }

        // 2. Check valid_until (absolute deadline)
        if session.config().valid_until() <= utc_now {
            return Some(SessionInput::ValidUntilPassed);
        }

        // 3. Check collect timeout (90 seconds from first part)
        if let Some(input) = self.check_collect_timeout(session, now) {
            return Some(input);
        }

        None
    }

    /// Check for collect timeout condition.
    ///
    /// Applies to `Collecting` and `AwaitingRetry` states.
    fn check_collect_timeout(&self, session: &Session, now: Instant) -> Option<SessionInput> {
        let first_part_at = match session.state() {
            SessionState::Collecting { first_part_at, .. } => Some(first_part_at),
            SessionState::AwaitingRetry { first_part_at, .. } => Some(first_part_at),
            _ => None,
        }?;

        if now.duration_since(*first_part_at) >= self.config.collect_timeout {
            Some(SessionInput::CollectTimeout)
        } else {
            None
        }
    }

    /// Check for unsafe hold condition.
    ///
    /// Triggers when the minimum CLTV expiry of held HTLCs is too close
    /// to the current block height.
    fn check_unsafe_hold(&self, session: &Session, current_height: u32) -> Option<SessionInput> {
        // Get the minimum CLTV expiry from all HTLC parts
        let min_cltv = session.state().min_cltv_expiry()?;

        // Calculate blocks remaining
        let blocks_remaining = min_cltv.saturating_sub(current_height);

        // If less than margin, trigger unsafe hold
        if blocks_remaining < self.config.min_cltv_margin {
            Some(SessionInput::UnsafeHold {
                min_cltv_expiry: min_cltv,
                current_height,
            })
        } else {
            None
        }
    }
}

// ============================================================================
// Error Types
// ============================================================================

/// Errors that can occur during timeout checking.
#[derive(Debug, Clone)]
pub enum TimeoutError {
    /// Failed to fetch current block height
    BlockheightFetch(String),
}

impl std::fmt::Display for TimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BlockheightFetch(e) => write!(f, "failed to fetch blockheight: {}", e),
        }
    }
}

impl std::error::Error for TimeoutError {}

// ============================================================================
// Helpers
// ============================================================================

/// Returns a short description of a timeout input for logging.
fn input_description(input: &SessionInput) -> &'static str {
    match input {
        SessionInput::CollectTimeout => "collect_timeout",
        SessionInput::ValidUntilPassed => "valid_until_passed",
        SessionInput::UnsafeHold { .. } => "unsafe_hold",
        _ => "unknown",
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::lsps2::session::{HtlcPart, SessionConfig, SessionId, SessionPhase};
    use crate::core::tlv::TlvStream;
    use crate::proto::lsps0::{Msat, Ppm, ShortChannelId};
    use crate::proto::lsps2::{OpeningFeeParams, Promise};
    use anyhow::Result;
    use async_trait::async_trait;
    use bitcoin::secp256k1::PublicKey;
    use chrono::TimeZone;
    use std::sync::atomic::{AtomicU32, Ordering};

    // ========================================================================
    // Mock BlockheightProvider
    // ========================================================================

    struct MockBlockheightProvider {
        height: AtomicU32,
    }

    impl MockBlockheightProvider {
        fn new(height: u32) -> Self {
            Self {
                height: AtomicU32::new(height),
            }
        }

        #[allow(dead_code)]
        fn set_height(&self, height: u32) {
            self.height.store(height, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl BlockheightProvider for MockBlockheightProvider {
        async fn get_blockheight(&self) -> Result<u32> {
            Ok(self.height.load(Ordering::SeqCst))
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

    fn expired_opening_fee_params() -> OpeningFeeParams {
        OpeningFeeParams {
            min_fee_msat: Msat::from_msat(1000),
            proportional: Ppm::from_ppm(1000),
            valid_until: Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap(), // Already expired
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

    fn expired_config() -> SessionConfig {
        SessionConfig::new(
            test_public_key(),
            expired_opening_fee_params(),
            Some(Msat::from_msat(100_000)),
        )
    }

    fn test_htlc_part(htlc_id: u64, amount_msat: u64, cltv_expiry: u32) -> HtlcPart {
        HtlcPart {
            htlc_id,
            amount_msat: Msat::from_msat(amount_msat),
            cltv_expiry,
            payment_hash: [1u8; 32],
            arrived_at: Instant::now(),
            onion_payload: TlvStream::default(),
            extra_tlvs: TlvStream::default(),
            in_channel: ShortChannelId::from(999u64),
        }
    }

    // ========================================================================
    // Configuration Tests
    // ========================================================================

    #[test]
    fn test_default_config() {
        let config = TimeoutConfig::default();
        assert_eq!(config.check_interval, Duration::from_secs(1));
        assert_eq!(config.collect_timeout, Duration::from_secs(90));
        assert_eq!(config.min_cltv_margin, 2);
    }

    #[test]
    fn test_custom_config() {
        let config = TimeoutConfig::new(Duration::from_millis(500), Duration::from_secs(60), 5);
        assert_eq!(config.check_interval, Duration::from_millis(500));
        assert_eq!(config.collect_timeout, Duration::from_secs(60));
        assert_eq!(config.min_cltv_margin, 5);
    }

    // ========================================================================
    // Collect Timeout Tests
    // ========================================================================

    #[tokio::test]
    async fn test_collect_timeout_not_triggered_before_deadline() {
        let session_manager = SessionManager::new_no_op();
        let blockheight_provider = Arc::new(MockBlockheightProvider::new(800_000));
        let config = TimeoutConfig::for_testing();

        let id = test_session_id();
        session_manager
            .create_session(id, test_config())
            .await
            .unwrap();

        // Add a part (starts the collect timer)
        session_manager
            .apply_input(
                id,
                SessionInput::PartArrived {
                    part: test_htlc_part(1, 50_000, 800_100),
                },
            )
            .await
            .unwrap();

        // Create timeout manager and check immediately (should not timeout)
        let timeout_manager =
            TimeoutManager::new(session_manager.clone(), blockheight_provider, config);

        timeout_manager.check_all_sessions().await.unwrap();

        // Session should still be in Collecting state
        let phase = session_manager.get_session_phase(id).await;
        assert_eq!(phase, Some(SessionPhase::Collecting));
    }

    #[tokio::test]
    async fn test_collect_timeout_triggers_after_deadline() {
        let session_manager = SessionManager::new_no_op();
        let blockheight_provider = Arc::new(MockBlockheightProvider::new(800_000));
        let config = TimeoutConfig {
            check_interval: Duration::from_millis(10),
            collect_timeout: Duration::from_millis(50), // Very short for testing
            min_cltv_margin: 2,
        };

        let id = test_session_id();
        session_manager
            .create_session(id, test_config())
            .await
            .unwrap();

        // Add a part (starts the collect timer)
        session_manager
            .apply_input(
                id,
                SessionInput::PartArrived {
                    part: test_htlc_part(1, 50_000, 800_100),
                },
            )
            .await
            .unwrap();

        // Wait for timeout to expire
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Create timeout manager and check
        let timeout_manager =
            TimeoutManager::new(session_manager.clone(), blockheight_provider, config);

        timeout_manager.check_all_sessions().await.unwrap();

        // Session should now be Failed
        let phase = session_manager.get_session_phase(id).await;
        assert_eq!(phase, Some(SessionPhase::Failed));
    }

    // ========================================================================
    // Valid Until Tests
    // ========================================================================

    #[tokio::test]
    async fn test_valid_until_not_triggered_when_future() {
        let session_manager = SessionManager::new_no_op();
        let blockheight_provider = Arc::new(MockBlockheightProvider::new(800_000));
        let config = TimeoutConfig::for_testing();

        let id = test_session_id();
        session_manager
            .create_session(id, test_config())
            .await
            .unwrap();

        // Add a part
        session_manager
            .apply_input(
                id,
                SessionInput::PartArrived {
                    part: test_htlc_part(1, 50_000, 800_100),
                },
            )
            .await
            .unwrap();

        let timeout_manager =
            TimeoutManager::new(session_manager.clone(), blockheight_provider, config);

        timeout_manager.check_all_sessions().await.unwrap();

        // Session should still be Collecting (valid_until is in 2100)
        let phase = session_manager.get_session_phase(id).await;
        assert_eq!(phase, Some(SessionPhase::Collecting));
    }

    #[tokio::test]
    async fn test_valid_until_triggers_when_expired() {
        let session_manager = SessionManager::new_no_op();
        let blockheight_provider = Arc::new(MockBlockheightProvider::new(800_000));
        let config = TimeoutConfig::for_testing();

        let id = test_session_id();
        // Use config with already-expired valid_until
        session_manager
            .create_session(id, expired_config())
            .await
            .unwrap();

        // Add a part
        session_manager
            .apply_input(
                id,
                SessionInput::PartArrived {
                    part: test_htlc_part(1, 50_000, 800_100),
                },
            )
            .await
            .unwrap();

        let timeout_manager =
            TimeoutManager::new(session_manager.clone(), blockheight_provider, config);

        timeout_manager.check_all_sessions().await.unwrap();

        // Session should be Failed due to valid_until
        let phase = session_manager.get_session_phase(id).await;
        assert_eq!(phase, Some(SessionPhase::Failed));
    }

    // ========================================================================
    // Unsafe Hold Tests
    // ========================================================================

    #[tokio::test]
    async fn test_unsafe_hold_not_triggered_with_sufficient_margin() {
        let session_manager = SessionManager::new_no_op();
        // Current height is 800_000, HTLC CLTV is 800_100 (100 blocks margin)
        let blockheight_provider = Arc::new(MockBlockheightProvider::new(800_000));
        let config = TimeoutConfig::for_testing();

        let id = test_session_id();
        session_manager
            .create_session(id, test_config())
            .await
            .unwrap();

        // Add a part with CLTV far in the future
        session_manager
            .apply_input(
                id,
                SessionInput::PartArrived {
                    part: test_htlc_part(1, 50_000, 800_100),
                },
            )
            .await
            .unwrap();

        let timeout_manager =
            TimeoutManager::new(session_manager.clone(), blockheight_provider, config);

        timeout_manager.check_all_sessions().await.unwrap();

        // Session should still be Collecting
        let phase = session_manager.get_session_phase(id).await;
        assert_eq!(phase, Some(SessionPhase::Collecting));
    }

    #[tokio::test]
    async fn test_unsafe_hold_triggers_when_cltv_too_close() {
        let session_manager = SessionManager::new_no_op();
        // Current height is 800_000, HTLC CLTV is 800_001 (only 1 block margin)
        let blockheight_provider = Arc::new(MockBlockheightProvider::new(800_000));
        let config = TimeoutConfig {
            check_interval: Duration::from_millis(10),
            collect_timeout: Duration::from_secs(90),
            min_cltv_margin: 2, // Need 2 blocks, only have 1
        };

        let id = test_session_id();
        session_manager
            .create_session(id, test_config())
            .await
            .unwrap();

        // Add a part with CLTV too close
        session_manager
            .apply_input(
                id,
                SessionInput::PartArrived {
                    part: test_htlc_part(1, 50_000, 800_001), // Only 1 block margin
                },
            )
            .await
            .unwrap();

        let timeout_manager =
            TimeoutManager::new(session_manager.clone(), blockheight_provider, config);

        timeout_manager.check_all_sessions().await.unwrap();

        // Session should be Failed due to unsafe hold
        let phase = session_manager.get_session_phase(id).await;
        assert_eq!(phase, Some(SessionPhase::Failed));
    }

    #[tokio::test]
    async fn test_unsafe_hold_triggers_when_cltv_passed() {
        let session_manager = SessionManager::new_no_op();
        // Current height is 800_005, HTLC CLTV was 800_000 (already passed!)
        let blockheight_provider = Arc::new(MockBlockheightProvider::new(800_005));
        let config = TimeoutConfig::for_testing();

        let id = test_session_id();
        session_manager
            .create_session(id, test_config())
            .await
            .unwrap();

        // Add a part with CLTV already in the past
        session_manager
            .apply_input(
                id,
                SessionInput::PartArrived {
                    part: test_htlc_part(1, 50_000, 800_000),
                },
            )
            .await
            .unwrap();

        let timeout_manager =
            TimeoutManager::new(session_manager.clone(), blockheight_provider, config);

        timeout_manager.check_all_sessions().await.unwrap();

        // Session should be Failed due to unsafe hold
        let phase = session_manager.get_session_phase(id).await;
        assert_eq!(phase, Some(SessionPhase::Failed));
    }

    // ========================================================================
    // Priority Tests
    // ========================================================================

    #[tokio::test]
    async fn test_unsafe_hold_priority_over_valid_until() {
        let session_manager = SessionManager::new_no_op();
        // CLTV is too close AND valid_until is expired
        let blockheight_provider = Arc::new(MockBlockheightProvider::new(800_000));
        let config = TimeoutConfig::for_testing();

        let id = test_session_id();
        session_manager
            .create_session(id, expired_config())
            .await
            .unwrap();

        // Add a part with CLTV too close
        session_manager
            .apply_input(
                id,
                SessionInput::PartArrived {
                    part: test_htlc_part(1, 50_000, 800_001),
                },
            )
            .await
            .unwrap();

        let timeout_manager =
            TimeoutManager::new(session_manager.clone(), blockheight_provider, config);

        // Get the session and check what input would be triggered
        let session = session_manager.get_session(id).await.unwrap();
        let input = timeout_manager.check_session(&session, 800_000, Instant::now(), Utc::now());

        // UnsafeHold should be triggered first (higher priority)
        assert!(matches!(input, Some(SessionInput::UnsafeHold { .. })));
    }

    // ========================================================================
    // Terminal State Tests
    // ========================================================================

    #[tokio::test]
    async fn test_terminal_sessions_are_skipped() {
        let session_manager = SessionManager::new_no_op();
        let blockheight_provider = Arc::new(MockBlockheightProvider::new(800_000));
        let config = TimeoutConfig {
            check_interval: Duration::from_millis(10),
            collect_timeout: Duration::from_millis(1), // Immediate timeout
            min_cltv_margin: 2,
        };

        let id = test_session_id();
        session_manager
            .create_session(id, test_config())
            .await
            .unwrap();

        // Add a part and trigger timeout to move to Failed state
        session_manager
            .apply_input(
                id,
                SessionInput::PartArrived {
                    part: test_htlc_part(1, 50_000, 800_100),
                },
            )
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(10)).await;

        let timeout_manager = TimeoutManager::new(
            session_manager.clone(),
            blockheight_provider.clone(),
            config.clone(),
        );

        // First check should move to Failed
        timeout_manager.check_all_sessions().await.unwrap();
        assert_eq!(
            session_manager.get_session_phase(id).await,
            Some(SessionPhase::Failed)
        );

        // Second check should not fail (terminal sessions are skipped)
        timeout_manager.check_all_sessions().await.unwrap();
        // Session should still be Failed (not an error)
        assert_eq!(
            session_manager.get_session_phase(id).await,
            Some(SessionPhase::Failed)
        );
    }

    // ========================================================================
    // Background Task Tests
    // ========================================================================

    #[tokio::test]
    async fn test_background_task_can_be_spawned_and_aborted() {
        let session_manager = SessionManager::new_no_op();
        let blockheight_provider = Arc::new(MockBlockheightProvider::new(800_000));
        let config = TimeoutConfig::for_testing();

        let timeout_manager = TimeoutManager::new(session_manager, blockheight_provider, config);

        let handle = timeout_manager.spawn();

        // Let it run for a bit
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Abort and check that it completes
        handle.abort();

        // Should complete (aborted)
        let result = handle.await;
        assert!(result.is_err()); // JoinError due to abort
    }

    #[tokio::test]
    async fn test_background_task_applies_timeouts() {
        let session_manager = SessionManager::new_no_op();
        let blockheight_provider = Arc::new(MockBlockheightProvider::new(800_000));
        let config = TimeoutConfig {
            check_interval: Duration::from_millis(10),
            collect_timeout: Duration::from_millis(30),
            min_cltv_margin: 2,
        };

        let id = test_session_id();
        session_manager
            .create_session(id, test_config())
            .await
            .unwrap();
        session_manager
            .apply_input(
                id,
                SessionInput::PartArrived {
                    part: test_htlc_part(1, 50_000, 800_100),
                },
            )
            .await
            .unwrap();

        let timeout_manager =
            TimeoutManager::new(session_manager.clone(), blockheight_provider, config);

        let handle = timeout_manager.spawn();

        // Wait for timeout to be detected and applied
        tokio::time::sleep(Duration::from_millis(100)).await;

        handle.abort();

        // Session should be Failed now
        let phase = session_manager.get_session_phase(id).await;
        assert_eq!(phase, Some(SessionPhase::Failed));
    }
}
