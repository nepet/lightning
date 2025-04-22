use anyhow::anyhow;
use async_trait::async_trait;
use cln_lsps::jsonrpc::server::{JsonRpcResponseWriter, RequestHandler};
use cln_lsps::jsonrpc::{server::JsonRpcServer, JsonRpcRequest};
use cln_lsps::jsonrpc::{
    Error as JsonRpcError, JsonRpcResponse, RequestObject, ResponseObject, RpcError, TransportError,
};
use cln_lsps::lsps0;
use cln_lsps::lsps0::model::{Lsps0listProtocolsRequest, Lsps0listProtocolsResponse};
use cln_lsps::lsps0::transport::{self, CustomMsg};
use cln_lsps::lsps2::model::{
    compute_opening_fee, Lsps2BuyRequest, Lsps2BuyResponse, Lsps2GetInfoRequest,
    Lsps2GetInfoResponse, OpeningFeeParams,
};
use cln_plugin::options::ConfigOption;
use cln_plugin::Plugin;
use cln_plugin::{options, Plugin};
use cln_rpc::notifications::CustomMsgNotification;
use cln_rpc::primitives::PublicKey;
use cln_rpc::primitives::ShortChannelId;
use log::{debug, info, warn};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex}; // Using Mutex for simplicity, consider RwLock for performance
use time::{Duration, OffsetDateTime}; // For time-based validation

use cln_rpc::primitives::PublicKey; // Import PublicKey

// Represents the details of a pending JIT channel request stored by the LSP.
#[derive(Debug, Clone)]
struct PendingJitChannel {
    client_node_id: PublicKey,
    lsp_scid: ShortChannelId, // The SCID the LSP uses internally or for the alias
    opening_fee_params: OpeningFeeParams,
    payment_size_msat: Option<u64>, // None for variable amount
    expires_at: OffsetDateTime,
    // Add other relevant details like client_trusts_lsp if needed
}

/// An option to enable this service. It defaults to `false` as we don't want a
/// node to be an LSP per default.
/// If a user want's to run an LSP service on their node this has to explicitly
/// set to true. We keep this as a dev option for now until it actually does
/// something.
const OPTION_ENABLED: options::DefaultBooleanConfigOption = ConfigOption::new_bool_with_default(
    "dev-lsps-service",
    false,
    "Enables an LSPS service on the node.",
);

#[derive(Clone)]
struct State {
    lsps_service: JsonRpcServer,
    // Store pending JIT requests keyed by the jit_channel_scid (string format)
    // Using Mutex for simplicity; consider RwLock if contention is high.
    pending_jit_channels: Arc<Mutex<HashMap<String, PendingJitChannel>>>,
    // TODO: Add mechanism to generate unique SCIDs and manage LSP-side aliases
    // TODO: Add mechanism to validate promises (e.g., MAC key)
}

// Placeholder for promise validation logic/key
// In a real implementation, this would likely involve a secret key loaded at startup.
fn validate_promise(_params: &OpeningFeeParams) -> bool {
    // TODO: Implement real validation (e.g., MAC check)
    // For now, accept any promise that was included in the request.
    true
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // TODO: Load configuration for fee parameters, promise validation key, etc.

    let initial_pending_channels = Arc::new(Mutex::new(HashMap::new()));

    // Pass the pending_jit_channels Arc to the Lsps2BuyHandler instance
    let buy_handler = Arc::new(Lsps2BuyHandler {
        promise_validator: Arc::new(validate_promise), // Use the validation function
        pending_jit_channels: initial_pending_channels.clone(),
    });

    let lsps_service = JsonRpcServer::builder()
        .with_handler(
            Lsps0listProtocolsRequest::METHOD.to_string(),
            Arc::new(Lsps0ListProtocolsHandler),
        )
        .with_handler(
            Lsps2GetInfoRequest::METHOD.to_string(),
            Arc::new(Lsps2GetInfoHandler),
        )
        .with_handler(Lsps2BuyRequest::METHOD.to_string(), buy_handler)
        .build();

    let state = State {
        lsps_service,
        pending_jit_channels: initial_pending_channels, // Share the same Arc with the handler
    };

    // TODO: Start a background task to clean up expired pending_jit_channels

    if let Some(plugin) = cln_plugin::Builder::new(tokio::io::stdin(), tokio::io::stdout())
        .option(OPTION_ENABLED)
        .hook("custommsg", on_custommsg)
        .configure()
        .await?
    {
        if !plugin.option(&OPTION_ENABLED)? {
            return plugin
                .disable(&format!("`{}` not enabled", OPTION_ENABLED.name))
                .await;
        }

        let plugin = plugin.start(state).await?;
        plugin.join().await
    } else {
        Ok(())
    }
}

async fn on_custommsg(
    p: Plugin<State>,
    v: serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    // All of this could be done async if needed.
    let continue_response = Ok(serde_json::json!({
      "result": "continue"
    }));
    let msg: CustomMsgNotification =
        serde_json::from_value(v).map_err(|e| anyhow!("invalid custommsg: {e}"))?;

    // Attempt to parse as LSPS0 CustomMsg format first
    match CustomMsg::from_str(&msg.payload) {
        Ok(req) if req.message_type == lsps0::transport::LSPS0_MESSAGE_TYPE => {
            debug!("Received LSPS0 message from peer {}", msg.peer_id);
            let dir = p.configuration().lightning_dir;
            let rpc_path = Path::new(&dir).join(&p.configuration().rpc_file);
            let mut writer = LspsResponseWriter {
                peer_id: msg.peer_id,
                rpc_path: rpc_path.try_into()?,
                // Store the original request ID if needed for context, though handle_message doesn't use it directly
            };

            let service = p.state().lsps_service.clone();
            // The payload inside CustomMsg is the actual JSON-RPC request/notification
            match service.handle_message(&req.payload, &mut writer).await {
                Ok(_) => {
                    debug!("Successfully handled LSPS message from {}", msg.peer_id);
                    continue_response
                }
                Err(e) => {
                    warn!("Failed to handle LSPS message from {}: {}", msg.peer_id, e);
                    // Decide if an error response should be sent back via writer?
                    // handle_message already tries to send JSON-RPC errors.
                    // If handle_message itself fails (e.g., transport error in writer), log it.
                    continue_response // Allow CLN to continue processing other hooks
                }
            }
        }
        Ok(req) => {
            // Unknown message type within CustomMsg format
            debug!(
                "Ignoring CustomMsg with unknown type {} from peer {}",
                req.message_type, msg.peer_id
            );
            continue_response
        }
        Err(_) => {
            // Payload doesn't match CustomMsg hex format, might be a raw JSON-RPC message?
            // This part is ambiguous in LSPS0. Assuming direct JSON for now if CustomMsg fails.
            debug!(
                "Payload from {} not in CustomMsg format, attempting direct JSON-RPC parse",
                msg.peer_id
            );
            let dir = p.configuration().lightning_dir;
            let rpc_path = Path::new(&dir).join(&p.configuration().rpc_file);
            let mut writer = LspsResponseWriter {
                peer_id: msg.peer_id,
                rpc_path: rpc_path.try_into()?,
            };
            let service = p.state().lsps_service.clone();
            let payload_bytes = match hex::decode(&msg.payload) {
                Ok(bytes) => bytes,
                Err(_) => msg.payload.as_bytes().to_vec(), // Assume raw string if not hex
            };

            match service.handle_message(&payload_bytes, &mut writer).await {
                Ok(_) => {
                    debug!(
                        "Successfully handled direct JSON-RPC message from {}",
                        msg.peer_id
                    );
                    continue_response
                }
                Err(e) => {
                    warn!(
                        "Failed to handle direct JSON-RPC message from {}: {}",
                        msg.peer_id, e
                    );
                    continue_response
                }
            }
        }
    }
}

pub struct LspsResponseWriter {
    peer_id: PublicKey,
    rpc_path: PathBuf,
}

#[async_trait]
impl JsonRpcResponseWriter for LspsResponseWriter {
    async fn write(&mut self, payload: &[u8]) -> cln_lsps::jsonrpc::Result<()> {
        let mut client = cln_rpc::ClnRpc::new(&self.rpc_path).await.map_err(|e| {
            cln_lsps::jsonrpc::Error::Transport(TransportError::Other(e.to_string()))
        })?;
        transport::send_custommsg(&mut client, payload.to_vec(), self.peer_id).await
    }
}

pub struct Lsps0ListProtocolsHandler;

#[async_trait]
impl RequestHandler for Lsps0ListProtocolsHandler {
    async fn handle(&self, payload: &[u8]) -> core::result::Result<Vec<u8>, RpcError> {
        let req: RequestObject<Lsps0listProtocolsRequest> =
            serde_json::from_slice(payload).unwrap();
        if let Some(id) = req.id {
            let res = Lsps0listProtocolsResponse { protocols: vec![] }.into_response(id);
            let res_vec = serde_json::to_vec(&res).unwrap();
            return Ok(res_vec);
        }
        // If request has no ID (notification), return empty Ok result.
        Ok(vec![])
    }
}

// --- LSPS2 Handlers ---

/// Handler for the `lsps2.get_info` method.
pub struct Lsps2GetInfoHandler;

#[async_trait]
impl RequestHandler for Lsps2GetInfoHandler {
    async fn handle(&self, payload: &[u8]) -> core::result::Result<Vec<u8>, RpcError> {
        let req: RequestObject<Lsps2GetInfoRequest> = serde_json::from_slice(payload)
            .map_err(|e| RpcError::parse_error(format!("Failed to parse request: {}", e)))?;

        // TODO: Implement token validation if used
        if let Some(token) = &req.params.as_ref().and_then(|p| p.token.as_ref()) {
            warn!(
                "Token validation not implemented. Ignoring token: {}",
                token
            );
            // Example: return Err(RpcError::custom_error(200, "unrecognized_or_stale_token"));
        }

        // --- Generate Fee Parameters ---
        // TODO: Load these from configuration or calculate dynamically based on chain fees.
        // Using hardcoded example values for now.
        let now = OffsetDateTime::now_utc();
        let valid_for = Duration::minutes(30); // How long the promise is valid
        let valid_until = now + valid_for;

        // Example promise generation (HMAC-SHA256 of params with a secret key)
        // This needs a proper implementation with a persistent key.
        let generate_promise = |params: &OpeningFeeParams| -> String {
            // Placeholder: In reality, serialize params deterministically and MAC them.
            // Ensure the generated promise is <= 512 bytes.
            let base_promise = format!(
                "promise_for_{}_{}_{}",
                params.min_fee_msat, params.proportional, params.valid_until
            );
            // Simple truncation for example purposes. A real implementation might use hashing.
            base_promise.chars().take(512).collect()
        };

        let mut params1 = OpeningFeeParams {
            min_fee_msat: 500_000, // 500 sat
            proportional: 1000,    // 0.1%
            valid_until: valid_until
                .format(&time::format_description::well_known::Iso8601::DEFAULT)
                .map_err(|e| {
                    RpcError::internal_error(format!("Failed to format datetime: {}", e))
                })?,
            min_lifetime: 1008,                    // ~1 week
            max_client_to_self_delay: 2016,        // ~2 weeks
            min_payment_size_msat: 10_000,         // 10 sat
            max_payment_size_msat: 10_000_000_000, // 0.1 BTC
            promise: "".to_string(),               // Will be generated
        };
        params1.promise = generate_promise(&params1);

        let fee_menu = vec![params1]; // Can add more options here

        // Validate generated params before sending (optional sanity check)
        for p in &fee_menu {
            if !p.validate() {
                warn!("Generated invalid OpeningFeeParams: {:?}", p);
                // Decide how to handle this - skip, error, etc.
                return Err(RpcError::internal_error(
                    "Failed to generate valid fee parameters",
                ));
            }
        }

        let response_data = Lsps2GetInfoResponse {
            opening_fee_params_menu: fee_menu,
        };

        // --- Create and Serialize Response ---
        if let Some(id) = req.id {
            let response = response_data.into_response(id);
            serde_json::to_vec(&response).map_err(|e| {
                RpcError::internal_error(format!("Failed to serialize response: {}", e))
            })
        } else {
            // Notification, should not happen for get_info, but handle defensively
            warn!("Received lsps2.get_info request without ID (notification)");
            Ok(vec![])
        }
    }
}

/// Handler for the `lsps2.buy` method.
pub struct Lsps2BuyHandler {
    // Function/closure to validate the promise string against the params.
    // Takes OpeningFeeParams, returns true if valid. Needs access to LSP's secret key.
    promise_validator: Arc<dyn Fn(&OpeningFeeParams) -> bool + Send + Sync>,
    pending_jit_channels: Arc<Mutex<HashMap<String, PendingJitChannel>>>,
}

#[async_trait]
impl RequestHandler for Lsps2BuyHandler {
    async fn handle(&self, payload: &[u8]) -> core::result::Result<Vec<u8>, RpcError> {
        // Note: We need the client's PublicKey (node_id) here.
        // The RequestHandler trait doesn't provide it directly.
        // It must be implicitly available via the transport context (e.g., Bolt8 connection)
        // or passed explicitly if the server architecture changes.
        // For now, we'll assume it's obtainable, but leave it as a TODO.
        // Using a placeholder public key. Replace with actual mechanism.
        let client_node_id = PublicKey::from_str(
            "020000000000000000000000000000000000000000000000000000000000000000",
        )
        .map_err(|e| {
            RpcError::internal_error(format!("Failed to create placeholder PublicKey: {}", e))
        })?; // TODO: Get actual client node ID from transport context

        let req: RequestObject<Lsps2BuyRequest> = serde_json::from_slice(payload)
            .map_err(|e| RpcError::parse_error(format!("Failed to parse request: {}", e)))?;

        let req_params = req
            .params
            .ok_or_else(|| RpcError::invalid_request("Missing params field"))?;
        let fee_params = req_params.opening_fee_params;

        // --- Validation ---
        // 0. Validate the structure of the fee_params received from client
        if !fee_params.validate() {
            warn!(
                "Received invalid opening_fee_params structure in lsps2.buy: {:?}",
                fee_params
            );
            return Err(RpcError::invalid_params(
                "Invalid structure for opening_fee_params",
            ));
        }

        // 1. Validate promise
        if !(self.promise_validator)(&fee_params) {
            warn!("Promise validation failed for params: {:?}", fee_params);
            return Err(RpcError::custom_error(201, "Invalid promise"));
        }

        // 2. Validate valid_until
        let now = OffsetDateTime::now_utc();
        let valid_until = OffsetDateTime::parse(
            &fee_params.valid_until,
            &time::format_description::well_known::Iso8601::DEFAULT,
        )
        .map_err(|e| {
            warn!(
                "Failed to parse valid_until '{}': {}",
                fee_params.valid_until, e
            );
            RpcError::custom_error(201, "Invalid format for valid_until")
        })?;
        if now >= valid_until {
            warn!(
                "Offer expired: now ({}) >= valid_until ({})",
                now, valid_until
            );
            return Err(RpcError::custom_error(
                201,
                "Offer expired (valid_until has passed)",
            ));
        }

        // 3. Validate payment_size_msat (if provided)
        if let Some(payment_size) = req_params.payment_size_msat {
            if payment_size < fee_params.min_payment_size_msat {
                warn!(
                    "payment_size_msat ({}) < min_payment_size_msat ({})",
                    payment_size, fee_params.min_payment_size_msat
                );
                return Err(RpcError::invalid_params(format!(
                    "payment_size_msat ({}) is less than min_payment_size_msat ({})",
                    payment_size, fee_params.min_payment_size_msat
                )));
            }
            if payment_size > fee_params.max_payment_size_msat {
                warn!(
                    "payment_size_msat ({}) > max_payment_size_msat ({})",
                    payment_size, fee_params.max_payment_size_msat
                );
                return Err(RpcError::invalid_params(format!(
                    "payment_size_msat ({}) is greater than max_payment_size_msat ({})",
                    payment_size, fee_params.max_payment_size_msat
                )));
            }

            // 4. Calculate opening_fee and check if payable
            let opening_fee = compute_opening_fee(payment_size, &fee_params).ok_or_else(|| {
                warn!(
                    "Opening fee calculation overflowed for payment size {}",
                    payment_size
                );
                RpcError::custom_error(203, "Calculation overflow for payment_size_msat")
                // payment_size_too_large (overflow)
            })?;

            // Use a reasonable minimum HTLC value (e.g., 1 sat) for the check
            // This should ideally come from LN implementation config.
            const HTLC_MINIMUM_MSAT: u64 = 1000;
            if opening_fee.saturating_add(HTLC_MINIMUM_MSAT) > payment_size {
                warn!(
                    "payment_size_msat ({}) <= opening_fee ({}) + htlc_minimum ({})",
                    payment_size, opening_fee, HTLC_MINIMUM_MSAT
                );
                return Err(RpcError::custom_error(
                    202,
                    "payment_size_msat is too small to cover the opening fee",
                )); // payment_size_too_small
            }

            // TODO: Check LSP incoming liquidity if payment_size is specified (requires RPC call to self)
            // if !check_liquidity(payment_size) {
            //     warn!("Insufficient incoming liquidity for payment size {}", payment_size);
            //     return Err(RpcError::custom_error(203, "Insufficient incoming liquidity")); // payment_size_too_large (liquidity)
            // }
        } else {
            // Variable amount invoice - no fee check possible here.
            // Fee check happens when the actual payment arrives (in htlc_accepted hook).
            info!("Handling lsps2.buy for variable amount invoice");
        }

        // --- Generate SCID and Store Pending Request ---
        // TODO: Implement a robust way to generate unique SCIDs for JIT channels.
        // Using a placeholder based on timestamp + client pubkey hash for now.
        // This MUST be unpredictable to third parties but reproducible/retrievable by the LSP.
        let timestamp_nanos = now.unix_timestamp_nanos();
        // Simple hash - replace with something better if needed (e.g., HMAC with a secret)
        let client_hash = client_node_id
            .serialize()
            .iter()
            .fold(0u64, |acc, &b| acc.wrapping_mul(31).wrapping_add(b as u64));
        let block = (timestamp_nanos / 1_000_000_000) as u32 & 0xFFFFFF; // Use lower 24 bits of timestamp seconds
        let tx = (timestamp_nanos / 1_000) as u32 & 0xFFFFFF; // Use lower 24 bits of timestamp micros
        let outnum = (client_hash & 0xFFFF) as u16; // Use lower 16 bits of hash

        let jit_scid = ShortChannelId::new(block, tx, outnum);
        let jit_scid_str = jit_scid.to_string();

        // TODO: Generate LSP-side alias SCID if different from jit_scid
        let lsp_scid = jit_scid; // Assuming they are the same for now

        let pending_request = PendingJitChannel {
            client_node_id,
            lsp_scid,
            opening_fee_params: fee_params.clone(), // Store the agreed params
            payment_size_msat: req_params.payment_size_msat,
            expires_at: valid_until,
        };

        // Store the pending request
        {
            // Lock scope
            let mut pending_map = self.pending_jit_channels.lock().unwrap();
            // Check for collisions? Unlikely with timestamp+hash but possible. Handle gracefully.
            if pending_map.contains_key(&jit_scid_str) {
                // This indicates a potential issue with SCID generation or rapid requests.
                // Option 1: Reject the request.
                // Option 2: Overwrite (if the previous one likely expired or failed).
                // Option 3: Generate a new SCID (requires loop/retry logic).
                warn!(
                    "JIT SCID collision detected for {}. Rejecting request.",
                    jit_scid_str
                );
                return Err(RpcError::internal_error(
                    "Failed to generate unique JIT channel ID, please try again",
                ));
            }
            info!(
                "Storing pending JIT channel request for client {} with SCID {}",
                pending_request.client_node_id, jit_scid_str
            );
            pending_map.insert(jit_scid_str.clone(), pending_request);
        } // Lock released

        // --- Prepare Response ---
        // TODO: Get actual CLTV delta from configuration or LN node state
        let lsp_cltv_expiry_delta = 144;
        // TODO: Determine client_trusts_lsp based on LSP policy/configuration
        let client_trusts_lsp = Some(false); // Defaulting to LSP trusts client model

        let response_data = Lsps2BuyResponse {
            jit_channel_scid: jit_scid_str,
            lsp_cltv_expiry_delta,
            client_trusts_lsp,
        };

        // Validate response before sending (optional sanity check)
        if !response_data.validate() {
            warn!("Generated invalid Lsps2BuyResponse: {:?}", response_data);
            // Attempt to clean up stored pending request? Difficult without ID.
            return Err(RpcError::internal_error(
                "Failed to generate valid buy response",
            ));
        }

        // --- Create and Serialize Response ---
        if let Some(id) = req.id {
            let response = response_data.into_response(id);
            serde_json::to_vec(&response).map_err(|e| {
                RpcError::internal_error(format!("Failed to serialize response: {}", e))
            })
        } else {
            // Notification, should not happen for buy, but handle defensively
            warn!("Received lsps2.buy request without ID (notification)");
            Ok(vec![])
        }
    }
}
