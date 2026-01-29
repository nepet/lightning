use anyhow::bail;
use bitcoin::hashes::Hash;
use cln_lsps::{
    cln_adapters::{
        hooks::service_custommsg_hook,
        notifications::{
            map_channel_state_changed, map_forward_event, parse_forward_event, parse_payment_hash,
            ForwardStatus,
        },
        rpc::ClnApiRpc,
        sender::ClnSender,
        session_handler::ClnSessionOutputHandler,
        state::ServiceState,
        types::HtlcAcceptedRequest,
    },
    core::{
        lsps2::{
            htlc::{Htlc, HtlcAcceptedHookHandler, HtlcDecision, Onion, RejectReason},
            htlc_holder::{HtlcHolder, HtlcInfo, HtlcResponse},
            manager::SessionManager,
            provider::{DatastoreProvider, NoOpEventEmitter},
            service::Lsps2ServiceHandler,
            session::{HtlcPart, SessionConfig, SessionId, SessionInput},
        },
        server::LspsService,
    },
    proto::lsps0::{Msat, ShortChannelId},
};
use cln_plugin::{options, Plugin};
use cln_rpc::notifications::ChannelStateChangedNotification;
use log::{debug, error, trace, warn};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::oneshot;

pub const OPTION_ENABLED: options::FlagConfigOption = options::ConfigOption::new_flag(
    "experimental-lsps2-service",
    "Enables lsps2 for the LSP service",
);

pub const OPTION_PROMISE_SECRET: options::StringConfigOption =
    options::ConfigOption::new_str_no_default(
        "experimental-lsps2-promise-secret",
        "A 64-character hex string that is the secret for promises",
    );

/// Type alias for the SessionManager with CLN-specific handlers.
type ClnSessionManager = SessionManager<NoOpEventEmitter, ClnSessionOutputHandler>;

#[derive(Clone)]
struct State {
    lsps_service: Arc<LspsService>,
    sender: ClnSender,
    lsps2_enabled: bool,
    /// Session manager for MPP HTLC sessions
    session_manager: Arc<ClnSessionManager>,
    /// HTLC holder for deferred hook responses
    htlc_holder: Arc<HtlcHolder>,
}

impl State {
    pub fn new(rpc_path: PathBuf, promise_secret: &[u8; 32]) -> Self {
        let api = Arc::new(ClnApiRpc::new(rpc_path.clone()));
        let sender = ClnSender::new(rpc_path);
        let lsps2_handler = Arc::new(Lsps2ServiceHandler::new(api, promise_secret));
        let lsps_service = Arc::new(LspsService::builder().with_protocol(lsps2_handler).build());

        // Create HTLC holder for deferred hook responses
        let htlc_holder = Arc::new(HtlcHolder::new());

        // Create session output handler (uses HtlcHolder to release HTLCs)
        let output_handler = Arc::new(ClnSessionOutputHandler::new(htlc_holder.clone()));

        // Create session manager with no-op event emitter and CLN output handler
        let session_manager = Arc::new(SessionManager::new(
            Arc::new(NoOpEventEmitter),
            output_handler,
        ));

        Self {
            lsps_service,
            sender,
            lsps2_enabled: true,
            session_manager,
            htlc_holder,
        }
    }
}

impl ServiceState for State {
    fn service(&self) -> Arc<LspsService> {
        self.lsps_service.clone()
    }

    fn sender(&self) -> cln_lsps::cln_adapters::sender::ClnSender {
        self.sender.clone()
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    if let Some(plugin) = cln_plugin::Builder::new(tokio::io::stdin(), tokio::io::stdout())
        .option(OPTION_ENABLED)
        .option(OPTION_PROMISE_SECRET)
        // FIXME: Temporarily disabled lsp feature to please test cases, this is
        // ok as the feature is optional per spec.
        // We need to ensure that `connectd` only starts after all plugins have
        // been initialized.
        // .featurebits(
        //     cln_plugin::FeatureBitsKind::Node,
        //     util::feature_bit_to_hex(LSP_FEATURE_BIT),
        // )
        // .featurebits(
        //     cln_plugin::FeatureBitsKind::Init,
        //     util::feature_bit_to_hex(LSP_FEATURE_BIT),
        // )
        .hook("custommsg", service_custommsg_hook)
        .hook("htlc_accepted", on_htlc_accepted)
        // Subscribe to notifications for MPP session state transitions
        .subscribe("channel_state_changed", on_channel_state_changed)
        .subscribe("forward_event", on_forward_event)
        .subscribe("disconnect", on_disconnect)
        .configure()
        .await?
    {
        let rpc_path =
            Path::new(&plugin.configuration().lightning_dir).join(&plugin.configuration().rpc_file);

        if plugin.option(&OPTION_ENABLED)? {
            log::debug!("lsps2-service enabled");
            if let Some(secret_hex) = plugin.option(&OPTION_PROMISE_SECRET)? {
                let secret_hex = secret_hex.trim().to_lowercase();

                let decoded_bytes = match hex::decode(&secret_hex) {
                    Ok(bytes) => bytes,
                    Err(_) => {
                        return plugin
                            .disable(&format!(
                                "Invalid hex string for promise secret: {}",
                                secret_hex
                            ))
                            .await;
                    }
                };

                let secret: [u8; 32] = match decoded_bytes.try_into() {
                    Ok(array) => array,
                    Err(vec) => {
                        return plugin
                            .disable(&format!(
                                "Promise secret must be exactly 32 bytes, got {}",
                                vec.len()
                            ))
                            .await;
                    }
                };

                let state = State::new(rpc_path, &secret);
                let plugin = plugin.start(state).await?;
                plugin.join().await
            } else {
                bail!("lsps2 enabled but no promise-secret set.");
            }
        } else {
            return plugin
                .disable(&format!("`{}` not enabled", &OPTION_ENABLED.name))
                .await;
        }
    } else {
        Ok(())
    }
}

async fn on_htlc_accepted(
    p: Plugin<State>,
    v: serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    Ok(handle_htlc_safe(&p, v).await)
}

async fn handle_htlc_safe(p: &Plugin<State>, v: serde_json::Value) -> serde_json::Value {
    match handle_htlc_inner(p, v).await {
        Ok(response) => response,
        Err(e) => {
            error!("HTLC hook error (continuing): {:#}", e);
            json_continue()
        }
    }
}

async fn handle_htlc_inner(
    p: &Plugin<State>,
    v: serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    if !p.state().lsps2_enabled {
        return Ok(json_continue());
    }

    let req: HtlcAcceptedRequest = serde_json::from_value(v)?;

    let short_channel_id = match req.onion.short_channel_id {
        Some(scid) => scid,
        None => {
            trace!("We are the destination of the HTLC, continue.");
            return Ok(json_continue());
        }
    };

    let rpc_path = Path::new(&p.configuration().lightning_dir).join(&p.configuration().rpc_file);
    let api = ClnApiRpc::new(rpc_path.clone());

    // Check if this SCID is a JIT session and whether it's MPP
    let ds_entry = match api.get_buy_request(&short_channel_id).await {
        Ok(entry) => entry,
        Err(_) => {
            // Not a JIT session SCID, continue
            trace!("SCID {} is not a JIT session, continue.", short_channel_id);
            return Ok(json_continue());
        }
    };

    // Check if this is an MPP payment
    if ds_entry.expected_payment_size.is_some() {
        // MPP payment - use the new session flow
        debug!(
            "MPP HTLC for JIT session {}: amount={}",
            short_channel_id,
            req.htlc.amount_msat.msat()
        );
        return handle_mpp_htlc(p, &req, short_channel_id, ds_entry).await;
    }

    // No-MPP payment - use the existing blocking flow
    // Fixme: Use real htlc_minimum_amount.
    let handler = HtlcAcceptedHookHandler::new(api, 1000);

    let onion = Onion {
        short_channel_id,
        payload: req.onion.payload,
    };

    let htlc = Htlc {
        amount_msat: Msat::from_msat(req.htlc.amount_msat.msat()),
        extra_tlvs: req.htlc.extra_tlvs.unwrap_or_default(),
    };

    debug!("Handle no-MPP JIT session HTLC.");
    let response = match handler.handle(&htlc, &onion).await {
        Ok(dec) => {
            log_decision(&dec);
            decision_to_response(dec)?
        }
        Err(e) => {
            // Fixme: Should we log **BROKEN** here?
            debug!("Htlc handler failed (continuing): {:#}", e);
            return Ok(json_continue());
        }
    };

    Ok(serde_json::to_value(&response)?)
}

/// Handle an MPP HTLC using the session state machine.
///
/// This function:
/// 1. Creates or gets the session from SessionManager
/// 2. Holds the HTLC using HtlcHolder with a oneshot channel
/// 3. Submits the HTLC part to the session
/// 4. Awaits the response on the oneshot channel
/// 5. Converts the HtlcResponse to JSON
async fn handle_mpp_htlc(
    p: &Plugin<State>,
    req: &HtlcAcceptedRequest,
    jit_scid: ShortChannelId,
    ds_entry: cln_lsps::proto::lsps2::DatastoreEntry,
) -> Result<serde_json::Value, anyhow::Error> {
    let state = p.state();
    let session_id = SessionId::from(jit_scid);

    // Create session config from datastore entry
    let config = SessionConfig::new(
        ds_entry.peer_id,
        ds_entry.opening_fee_params.clone(),
        ds_entry.expected_payment_size,
    );

    // Get or create the session
    let created = state
        .session_manager
        .get_or_create_session(session_id, || config)
        .await;

    match created {
        Ok(true) => debug!("Created new MPP session: {}", session_id),
        Ok(false) => debug!("Using existing MPP session: {}", session_id),
        Err(e) => {
            warn!("Failed to get/create session {}: {}", session_id, e);
            return Ok(json_continue());
        }
    }

    // Create the HtlcInfo for the holder
    let htlc_info = htlc_info_from_request(req);

    // Create the oneshot channel for the response
    let (tx, rx) = oneshot::channel();

    // Hold the HTLC (this stores the sender)
    state.htlc_holder.hold(session_id, htlc_info, tx).await;

    // Create the HtlcPart for the session
    let part = htlc_part_from_request(req, jit_scid);

    // Submit the part to the session
    let input = SessionInput::PartArrived { part };
    match state.session_manager.apply_input(session_id, input).await {
        Ok(phase) => {
            debug!("Session {} transitioned to phase: {:?}", session_id, phase);
        }
        Err(e) => {
            warn!("Failed to apply input to session {}: {}", session_id, e);
            // Try to release the HTLC we just held
            state
                .htlc_holder
                .release_fail(
                    session_id,
                    cln_lsps::core::lsps2::session::FailureCode::TemporaryChannelFailure,
                )
                .await;
            return Ok(json_continue());
        }
    }

    // Await the response from the HTLC holder
    // This will be sent when the session decides what to do (forward or fail)
    match rx.await {
        Ok(response) => {
            debug!(
                "Got HTLC response for session {}: {:?}",
                session_id,
                match &response {
                    HtlcResponse::Continue { forward_to, .. } =>
                        format!("Continue to {}", forward_to),
                    HtlcResponse::Fail { failure_code } => format!("Fail with {:?}", failure_code),
                }
            );
            htlc_response_to_json(response)
        }
        Err(_) => {
            // The sender was dropped without sending - this shouldn't happen
            // in normal operation but could happen if the session was removed
            warn!(
                "HTLC response channel dropped for session {}, failing HTLC",
                session_id
            );
            Ok(json_fail(
                cln_lsps::proto::lsps2::failure_codes::TEMPORARY_CHANNEL_FAILURE,
            ))
        }
    }
}

fn decision_to_response(decision: HtlcDecision) -> Result<serde_json::Value, anyhow::Error> {
    Ok(match decision {
        HtlcDecision::NotOurs => json_continue(),

        HtlcDecision::Forward {
            mut payload,
            forward_to,
            mut extra_tlvs,
        } => json_continue_forward(
            payload.to_bytes()?,
            forward_to.as_byte_array().to_vec(),
            extra_tlvs.to_bytes()?,
        ),

        // Fixme: once we implement MPP-Support we need to remove this.
        HtlcDecision::Reject {
            reason: RejectReason::MppNotSupported,
        } => json_continue(),
        HtlcDecision::Reject { reason } => json_fail(reason.failure_code()),
    })
}

fn json_continue() -> serde_json::Value {
    serde_json::json!({"result": "continue"})
}

fn json_continue_forward(
    payload: Vec<u8>,
    forward_to: Vec<u8>,
    extra_tlvs: Vec<u8>,
) -> serde_json::Value {
    serde_json::json!({
        "result": "continue",
        "payload": hex::encode(payload),
        "forward_to": hex::encode(forward_to),
        "extra_tlvs": hex::encode(extra_tlvs)
    })
}

fn json_fail(failure_code: &str) -> serde_json::Value {
    serde_json::json!({
        "result": "fail",
        "failure_message": failure_code
    })
}

fn log_decision(decision: &HtlcDecision) {
    match decision {
        HtlcDecision::NotOurs => {
            trace!("SCID not ours, continue");
        }
        HtlcDecision::Forward { forward_to, .. } => {
            debug!(
                "Forwarding via JIT channel {}",
                hex::encode(forward_to.as_byte_array())
            );
        }
        HtlcDecision::Reject { reason } => {
            debug!("Rejecting HTLC: {:?}", reason);
        }
    }
}

// ============================================================================
// MPP Session Helper Functions
// ============================================================================

/// Convert HtlcAcceptedRequest to HtlcPart for the session state machine.
///
/// # Arguments
/// * `req` - The HTLC accepted request from the hook
/// * `jit_scid` - The JIT SCID that this HTLC targets
fn htlc_part_from_request(req: &HtlcAcceptedRequest, jit_scid: ShortChannelId) -> HtlcPart {
    // Convert payment_hash from Vec<u8> to [u8; 32]
    let mut payment_hash = [0u8; 32];
    if req.htlc.payment_hash.len() == 32 {
        payment_hash.copy_from_slice(&req.htlc.payment_hash);
    }

    HtlcPart::new(
        req.htlc.id,
        Msat::from_msat(req.htlc.amount_msat.msat()),
        req.htlc.cltv_expiry,
        payment_hash,
        req.onion.payload.clone(),
        req.htlc.extra_tlvs.clone().unwrap_or_default(),
        jit_scid,
    )
}

/// Convert HtlcAcceptedRequest to HtlcInfo for the HTLC holder.
///
/// # Arguments
/// * `req` - The HTLC accepted request from the hook
fn htlc_info_from_request(req: &HtlcAcceptedRequest) -> HtlcInfo {
    // Convert payment_hash from Vec<u8> to [u8; 32]
    let mut payment_hash = [0u8; 32];
    if req.htlc.payment_hash.len() == 32 {
        payment_hash.copy_from_slice(&req.htlc.payment_hash);
    }

    HtlcInfo {
        htlc_id: req.htlc.id,
        amount_msat: Msat::from_msat(req.htlc.amount_msat.msat()),
        cltv_expiry: req.htlc.cltv_expiry,
        payment_hash,
        payload: req.onion.payload.clone(),
    }
}

/// Convert HtlcResponse to JSON for the hook response.
fn htlc_response_to_json(response: HtlcResponse) -> Result<serde_json::Value, anyhow::Error> {
    use cln_lsps::core::lsps2::session::FailureCode;

    match response {
        HtlcResponse::Continue {
            forward_to,
            mut payload,
            mut extra_tlvs,
        } => {
            // CLN expects forward_to as the SCID string
            Ok(serde_json::json!({
                "result": "continue",
                "payload": hex::encode(payload.to_bytes()?),
                "forward_to": forward_to.to_string(),
                "extra_tlvs": hex::encode(extra_tlvs.to_bytes()?)
            }))
        }
        HtlcResponse::Fail { failure_code } => {
            let failure_message = match failure_code {
                FailureCode::TemporaryChannelFailure => "1007", // temporary_channel_failure
                FailureCode::UnknownNextPeer => "400a",         // unknown_next_peer
            };
            Ok(serde_json::json!({
                "result": "fail",
                "failure_message": failure_message
            }))
        }
    }
}

// ============================================================================
// Notification Handlers
// ============================================================================

/// Handle channel_state_changed notification.
///
/// This notification is sent when a channel's state changes. We use it to detect:
/// - `CHANNELD_NORMAL`: Channel is ready for forwarding (triggers `ClientChannelReady`)
/// - Closing states: Channel is closing (triggers `ClientDisconnected`)
async fn on_channel_state_changed(
    p: Plugin<State>,
    v: serde_json::Value,
) -> Result<(), anyhow::Error> {
    // Parse the notification
    let notif: ChannelStateChangedNotification = match serde_json::from_value(v) {
        Ok(n) => n,
        Err(e) => {
            debug!("Failed to parse channel_state_changed notification: {}", e);
            return Ok(());
        }
    };

    let state = p.state();
    if !state.lsps2_enabled {
        return Ok(());
    }

    // Convert channel_id to [u8; 32]
    let channel_id: [u8; 32] = *notif.channel_id.as_byte_array();

    // Find session by channel_id
    let session_id = match state.session_manager.find_by_channel_id(&channel_id).await {
        Some(id) => id,
        None => {
            trace!(
                "No JIT session found for channel_id {}",
                hex::encode(channel_id)
            );
            return Ok(());
        }
    };

    debug!(
        "channel_state_changed for session {:?}: {:?} -> {:?}",
        session_id, notif.old_state, notif.new_state
    );

    // Get alias SCID from the notification's short_channel_id field
    // For zero-conf channels, this should be the alias SCID
    let alias_scid = notif.short_channel_id;

    // Map to SessionInput
    let input = match map_channel_state_changed(notif.new_state, alias_scid) {
        Some(input) => input,
        None => {
            trace!(
                "Channel state {:?} does not trigger session transition",
                notif.new_state
            );
            return Ok(());
        }
    };

    // Apply input to session
    match state.session_manager.apply_input(session_id, input).await {
        Ok(phase) => {
            debug!(
                "Session {:?} transitioned to phase {:?} after channel_state_changed",
                session_id, phase
            );
        }
        Err(e) => {
            warn!(
                "Failed to apply channel_state_changed to session {:?}: {}",
                session_id, e
            );
        }
    }

    Ok(())
}

/// Handle forward_event notification.
///
/// This notification is sent when a forward's status changes. We use it to detect:
/// - `settled`: Preimage received (triggers `PreimageReceived`)
/// - `failed`/`local_failed`: Client rejected (triggers `ClientRejectedPayment`)
async fn on_forward_event(p: Plugin<State>, v: serde_json::Value) -> Result<(), anyhow::Error> {
    // Parse the notification using our custom parser (not in cln_rpc)
    let notif = match parse_forward_event(&v) {
        Some(n) => n,
        None => {
            debug!("Failed to parse forward_event notification");
            return Ok(());
        }
    };

    let state = p.state();
    if !state.lsps2_enabled {
        return Ok(());
    }

    // Parse payment_hash
    let payment_hash = match parse_payment_hash(&notif.payment_hash) {
        Some(h) => h,
        None => {
            debug!(
                "Invalid payment_hash in forward_event: {}",
                notif.payment_hash
            );
            return Ok(());
        }
    };

    // Find session by payment_hash
    let session_id = match state
        .session_manager
        .find_by_payment_hash(&payment_hash)
        .await
    {
        Some(id) => id,
        None => {
            trace!(
                "No JIT session found for payment_hash {}",
                notif.payment_hash
            );
            return Ok(());
        }
    };

    debug!(
        "forward_event for session {:?}: status={:?}",
        session_id, notif.status
    );

    // Map to SessionInput
    let input = match map_forward_event(&notif) {
        Some(input) => input,
        None => {
            trace!(
                "Forward status {:?} does not trigger session transition",
                notif.status
            );
            return Ok(());
        }
    };

    // Log preimage if available
    if notif.status == ForwardStatus::Settled {
        if let Some(ref preimage_hex) = notif.preimage {
            debug!(
                "Session {:?} received preimage: {}",
                session_id, preimage_hex
            );
        }
    }

    // Apply input to session
    match state.session_manager.apply_input(session_id, input).await {
        Ok(phase) => {
            debug!(
                "Session {:?} transitioned to phase {:?} after forward_event",
                session_id, phase
            );
        }
        Err(e) => {
            warn!(
                "Failed to apply forward_event to session {:?}: {}",
                session_id, e
            );
        }
    }

    Ok(())
}

/// Handle disconnect notification.
///
/// This notification is sent when a peer disconnects. We use it to detect
/// client disconnection during channel opening, which should fail the session.
async fn on_disconnect(p: Plugin<State>, v: serde_json::Value) -> Result<(), anyhow::Error> {
    // Parse the peer_id from the notification
    // The disconnect notification has format: {"id": "pubkey_hex"}
    let peer_id: bitcoin::secp256k1::PublicKey = match v.get("id") {
        Some(serde_json::Value::String(s)) => match s.parse() {
            Ok(pk) => pk,
            Err(e) => {
                debug!("Failed to parse peer_id in disconnect notification: {}", e);
                return Ok(());
            }
        },
        _ => {
            debug!("Missing or invalid 'id' in disconnect notification");
            return Ok(());
        }
    };

    let state = p.state();
    if !state.lsps2_enabled {
        return Ok(());
    }

    // Find all sessions for this peer
    let session_ids = state.session_manager.find_by_peer_id(&peer_id).await;

    if session_ids.is_empty() {
        trace!("No JIT sessions found for disconnected peer {}", peer_id);
        return Ok(());
    }

    debug!(
        "Peer {} disconnected, found {} JIT session(s)",
        peer_id,
        session_ids.len()
    );

    // Apply ClientDisconnected to each session
    let input = SessionInput::ClientDisconnected;
    for session_id in session_ids {
        match state
            .session_manager
            .apply_input(session_id, input.clone())
            .await
        {
            Ok(phase) => {
                debug!(
                    "Session {:?} transitioned to phase {:?} after disconnect",
                    session_id, phase
                );
            }
            Err(e) => {
                // Session may already be in a terminal state, which is fine
                trace!(
                    "Failed to apply disconnect to session {:?}: {}",
                    session_id,
                    e
                );
            }
        }
    }

    Ok(())
}
