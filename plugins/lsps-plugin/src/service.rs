use anyhow::anyhow;
use async_trait::async_trait;
use chrono::{Duration, Utc};
use cln_lsps::jsonrpc::server::{JsonRpcResponseWriter, RequestHandler};
use cln_lsps::jsonrpc::{server::JsonRpcServer, JsonRpcRequest};
use cln_lsps::jsonrpc::{JsonRpcResponse, RequestObject, RpcError, TransportError};
use cln_lsps::lsps0::model::{Lsps0listProtocolsRequest, Lsps0listProtocolsResponse};
use cln_lsps::lsps0::primitives::{Msat, Ppm};
use cln_lsps::lsps0::transport::{self, CustomMsg};
use cln_lsps::lsps2::model::{
    compute_opening_fee, Lsps2BuyRequest, Lsps2BuyResponse, Lsps2GetInfoRequest,
    Lsps2GetInfoResponse, OpeningFeeParams,
};
use cln_lsps::{lsps0, util};
use cln_plugin::options::ConfigOption;
use cln_plugin::{options, Plugin};
use cln_rpc::notifications::CustomMsgNotification;
use cln_rpc::primitives::PublicKey;
use log::{debug, info, warn};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

const LSP_FEATURE_BIT: usize = 729;

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
    // TODO: Add a storage (Arc/Mutex/HashMap with SCID as key) to store pending
    // jit channel requests (with weak or cleanup mech)
    // TODO: Add mechanism to generate unique SCIDs and manage LSP-side aliases
    // TODO: Add mechanism to validate promises (e.g., MAC key)
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // TODO: Add options for LSP fee policies.
    let lsps_service = JsonRpcServer::builder()
        .with_handler(
            Lsps0listProtocolsRequest::METHOD.to_string(),
            Arc::new(Lsps0ListProtocolsHandler),
        )
        .with_handler(
            Lsps2GetInfoRequest::METHOD.to_string(),
            Arc::new(Lsps2GetInfoHandler),
        )
        .with_handler(
            Lsps2BuyRequest::METHOD.to_string(),
            Arc::new(Lsps2BuyHandler),
        )
        .build();

    let state = State { lsps_service };

    // TODO: Start a background task to clean up expired pending_jit_channels

    if let Some(plugin) = cln_plugin::Builder::new(tokio::io::stdin(), tokio::io::stdout())
        .option(OPTION_ENABLED)
        .featurebits(
            cln_plugin::FeatureBitsKind::Node,
            util::feature_bit_to_hex(LSP_FEATURE_BIT),
        )
        .featurebits(
            cln_plugin::FeatureBitsKind::Init,
            util::feature_bit_to_hex(LSP_FEATURE_BIT),
        )
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

/// Handler for the `lsps2.get_info` method.
pub struct Lsps2GetInfoHandler;

#[async_trait]
impl RequestHandler for Lsps2GetInfoHandler {
    async fn handle(&self, payload: &[u8]) -> core::result::Result<Vec<u8>, RpcError> {
        let req: RequestObject<Lsps2GetInfoRequest> = serde_json::from_slice(payload)
            .map_err(|e| RpcError::parse_error(format!("failed to parse request: {e}")))?;

        let params = req
            .params
            .ok_or(RpcError::invalid_params("expected params but was missing"))?;

        // TODO: Implement token validation if tokens are used.
        if let Some(token) = params.token {
            warn!(
                "Token validation is not implemented. Ignoring token: {}",
                token
            );
        }

        // TODO: Get data from options. We just return some dummy data for now.
        let now = Utc::now();
        let valid_for = Duration::minutes(30); // How long the promise is valid
        let valid_until = now + valid_for;

        // FIXME:
        // Example promise generation (HMAC-SHA256 of params with a secret key)
        // This needs a proper implementation with a persistent key.
        let generate_promise = |params: &OpeningFeeParams| -> String {
            // Placeholder: In reality, serialize params deterministically and
            // MAC them. Ensure the generated promise is <= 512 bytes.
            let base_promise = format!(
                "promise_for_{}_{}_{}",
                params.min_fee_msat, params.proportional, params.valid_until
            );
            // Simple truncation for example purposes. A real implementation
            // might use hashing.
            base_promise.chars().take(512).collect()
        };

        let mut params_out = OpeningFeeParams {
            min_fee_msat: Msat::from_msat(500_000), // 500 sat
            proportional: Ppm::from_ppm(1000),      // 0.1%
            valid_until,
            min_lifetime: 1008,                                     // ~1 week
            max_client_to_self_delay: 2016,                         // ~2 weeks
            min_payment_size_msat: Msat::from_msat(10_000),         // 10 sat
            max_payment_size_msat: Msat::from_msat(10_000_000_000), // 0.1 BTC
            promise: "".to_string(),                                // Will be generated
        };
        params_out.promise = generate_promise(&params_out);

        let fee_menu = vec![params_out]; // Can add more options here

        // TODO(Optional): validate generated params before sending. Depends on
        // the way we generate them from the options.

        let res_data = Lsps2GetInfoResponse {
            opening_fee_params_menu: fee_menu,
        };

        if let Some(id) = req.id {
            let res = res_data.into_response(id);
            serde_json::to_vec(&res).map_err(|e| {
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
pub struct Lsps2BuyHandler;

#[async_trait]
impl RequestHandler for Lsps2BuyHandler {
    async fn handle(&self, payload: &[u8]) -> core::result::Result<Vec<u8>, RpcError> {
        // Note: We may want to have access to a client's PublicKey here to
        // reflect it into the SCID or to store it along a pending jit channel
        // for a sanity check on the HTLC's destination.
        // As the RequestHandler trait doesn't provide it directly, one option
        // is to add it to the payload and strip it off before we deserialize
        // it.
        let req: RequestObject<Lsps2BuyRequest> = serde_json::from_slice(payload)
            .map_err(|e| RpcError::parse_error(format!("Failed to parse request: {}", e)))?;

        let req_params = req
            .params
            .ok_or_else(|| RpcError::invalid_request("Missing params field"))?;
        let fee_params = req_params.opening_fee_params;

        // --- Validation ---
        // 0. Validate the structure of the fee_params received from client
        // if !fee_params.validate() {
        //     warn!(
        //         "Received invalid opening_fee_params structure in lsps2.buy: {:?}",
        //         fee_params
        //     );
        //     return Err(RpcError::invalid_params(
        //         "Invalid structure for opening_fee_params",
        //     ));
        // }

        // 1. Validate promise
        // if !(self.promise_validator)(&fee_params) {
        //     warn!("Promise validation failed for params: {:?}", fee_params);
        //     return Err(RpcError::custom_error(201, "Invalid promise"));
        // }

        // 2. Validate valid_until
        let now = Utc::now();
        if now >= fee_params.valid_until {
            warn!(
                "Offer expired: now ({}) >= valid_until ({})",
                now, fee_params.valid_until
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
            let opening_fee = compute_opening_fee(
                payment_size.msat(),
                fee_params.min_fee_msat.msat(),
                fee_params.proportional.ppm() as u64,
            )
            .ok_or_else(|| {
                warn!(
                    "Opening fee calculation overflowed for payment size {}",
                    payment_size
                );
                RpcError::custom_error(203, "failed to calculate opening fee")
            })?;

            // TODO: This is a dummy for now, get the real value from the
            // options.
            const HTLC_MINIMUM_MSAT: u64 = 1000;
            if opening_fee.saturating_add(HTLC_MINIMUM_MSAT) > payment_size.msat() {
                warn!(
                    "payment_size_msat ({}) <= opening_fee ({}) + htlc_minimum ({})",
                    payment_size, opening_fee, HTLC_MINIMUM_MSAT
                );
                return Err(RpcError::custom_error(
                    202,
                    "payment_size_msat is too small to cover the opening fee",
                ));
            }

            // TODO(Optional): Check the incoming liquidity if payment_size is
            // specified (via RPC call). May want to print a logling if the
            // liquidity is insufficient.
        } else {
            // Variable amount invoice - no fee check possible here.
            // Fee check happens when the actual payment arrives (in htlc_accepted hook).
            info!("Handling lsps2.buy for variable amount invoice");
        }

        // TODO: Implement a robus way to generate unique SCIDs for JIT channels.
        // Must be unpredictable to third parties but in best case reproducible
        // by the LSP and bound to the public key of the client (possible
        // discounts).
        // PLACEHOLDER: Timestamp based approach.
        let ts_nanos = now.timestamp_nanos_opt().ok_or_else(|| {
            debug!("Failed to get timestamp with nanos.");
            RpcError::internal_error("failed to generate scid")
        })?;
        let block = (ts_nanos / 1_000_000_000) as u32 & 0xFFFFFF;
        let tx = (ts_nanos / 1_000) as u32 & 0xFFFFFF;
        let outnum = (ts_nanos) as u16;

        let jit_scid =
            cln_rpc::primitives::ShortChannelId::from_str(&format!("{}x{}x{}", block, tx, outnum))
                .map_err(|e| {
                    debug!("Failed to construct scid from string {e}");
                    RpcError::internal_error("failed to generate scid")
                })?;

        // TODO: Maybe we need to add an alias SCID so that the HTLC does not
        // get rejected, check on that later!

        // TODO: Store pending request:
        // - client_node_id (maybe)
        // - jit_scid
        // - opening_fee_params
        // - payment_size_msat (optional)
        // - client_trusts_lsp (maybe)
        // - some_key_for_scid (maybe)

        // TODO: Get actual CLTV delta from options
        let lsp_cltv_expiry_delta = 144;
        // TODO: Determine client_trusts_lsp based on LSP policy/options
        let client_trusts_lsp = false; // Defaulting to LSP trusts client model

        let response_data = Lsps2BuyResponse {
            jit_channel_scid: jit_scid,
            lsp_cltv_expiry_delta,
            client_trusts_lsp,
        };

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
