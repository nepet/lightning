use anyhow::{anyhow, Context};
use async_trait::async_trait;
use cln_lsps::lsps0::model::Lsps0listProtocolsRequest;
use cln_lsps::lsps0::transport::{self, CustomMsg};
use cln_lsps::lsps2::handler::{Lsps2BuyHandler, Lsps2GetInfoHandler};
use cln_lsps::lsps2::model::{DatastoreEntry, Lsps2BuyRequest, Lsps2GetInfoRequest};
use cln_lsps::lsps2::service::{
    ChannelInfo, ClnRpcCall, HtlcIn, JitChannelCoordinator, JitHtlcAction,
};
use cln_lsps::model::tlv::ToBytes;
use cln_lsps::model::{
    tlv, HtlcAcceptedContinueResponse, HtlcAcceptedRequest, HtlcAcceptedResponse,
    TLV_SHORT_CHANNEL_ID,
};
use cln_lsps::util::wrap_payload_with_peer_id;
use cln_lsps::{
    jsonrpc::{
        server::{JsonRpcResponseWriter, JsonRpcServer},
        JsonRpcRequest,
    },
    lsps0::handler::Lsps0ListProtocolsHandler,
};
use cln_lsps::{lsps0, lsps2, model, util};
use cln_plugin::options::ConfigOption;
use cln_plugin::{options, Plugin};
use cln_rpc::model::requests::ListdatastoreRequest;
use cln_rpc::notifications::CustomMsgNotification;
use cln_rpc::primitives::{Amount, PublicKey, ShortChannelId};
use cln_rpc::ClnRpc;
use hex::FromHex;
use log::{debug, trace, warn};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::{oneshot, Mutex};

const LSP_FEATURE_BIT: usize = 729;

/// An option to enable this service.
const OPTION_ENABLED: options::FlagConfigOption = ConfigOption::new_flag(
    "dev-lsps-service-enabled",
    "Enables an LSPS service on the node.",
);

// trait BuilderExt {
//     fn register_lsps0(self) -> Self;
// }

// impl<S, I, O> BuilderExt for cln_plugin::Builder<S, I, O>
// where
//     O: Send + AsyncWrite + Unpin + 'static,
//     S: Clone + Sync + Send + 'static,
//     I: AsyncRead + Send + Unpin + 'static,
// {
//     fn register_lsps0(self) -> Self {
//         self.option(lsps2::OPTION_ENABLED)
//     }
// }

#[derive(Clone)]
pub struct State {
    rpc_path: PathBuf,
    lsps_service: JsonRpcServer,
    rpc: Arc<Mutex<ClnRpc>>,
    lsps2_enabled: bool,
    lsps2_coordinators: Arc<Mutex<HashMap<u64, Weak<JitChannelCoordinator<ClnRpcCall>>>>>,
}

impl State {
    async fn get_or_create_lsps2_coordinator(
        &self,
        jit_scid: u64,
        channel_info: ChannelInfo,
        mpp_timeout: Duration,
    ) -> Result<Arc<JitChannelCoordinator<ClnRpcCall>>, anyhow::Error> {
        let mut coord_map = self.lsps2_coordinators.lock().await;
        if let Some(weak_coord) = coord_map.get(&jit_scid) {
            if let Some(strong_coord) = weak_coord.upgrade() {
                return Ok(strong_coord);
            }
        }

        log::debug!(
            "Creating a new JIT channel coordinator for scid {}",
            jit_scid
        );

        let rpc = ClnRpc::new(&self.rpc_path)
            .await
            .with_context(|| "failed to connect to rpc socket")?;
        let rpc = Arc::new(ClnRpcCall::new(Mutex::new(rpc)));
        let new_coord = JitChannelCoordinator::new_arc(jit_scid, channel_info, rpc, mpp_timeout);
        coord_map.insert(jit_scid, Arc::downgrade(&new_coord));
        Ok(new_coord)
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    if let Some(plugin) = cln_plugin::Builder::new(tokio::io::stdin(), tokio::io::stdout())
        .option(OPTION_ENABLED)
        .option(lsps2::OPTION_ENABLED)
        .option(lsps2::OPTION_PROMISE_SECRET)
        .featurebits(
            cln_plugin::FeatureBitsKind::Node,
            util::feature_bit_to_hex(LSP_FEATURE_BIT),
        )
        .featurebits(
            cln_plugin::FeatureBitsKind::Init,
            util::feature_bit_to_hex(LSP_FEATURE_BIT),
        )
        .hook("custommsg", on_custommsg)
        .hook("htlc_accepted", on_hltc_accpted)
        .configure()
        .await?
    {
        if !plugin.option(&OPTION_ENABLED)? {
            return plugin
                .disable(&format!("`{}` not enabled", OPTION_ENABLED.name))
                .await;
        }

        let dir = plugin.configuration().lightning_dir;
        let rpc_path = Path::new(&dir).join(&plugin.configuration().rpc_file);
        let rpc_client = ClnRpc::new(&rpc_path).await?;
        let rpc_arc = Arc::new(Mutex::new(rpc_client));

        let mut lsps_builder = JsonRpcServer::builder().with_handler(
            Lsps0listProtocolsRequest::METHOD.to_string(),
            Arc::new(Lsps0ListProtocolsHandler {
                lsps2_enabled: plugin.option(&lsps2::OPTION_ENABLED)?,
            }),
        );

        // FIXME: Once this get more cluttered, replace with a match to make it
        // cleaner.
        let lsps2_enabled = if plugin.option(&lsps2::OPTION_ENABLED)? {
            log::debug!("lsps2 enabled");
            let secret_hex = plugin.option(&lsps2::OPTION_PROMISE_SECRET)?;
            if let Some(secret_hex) = secret_hex {
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

                lsps_builder = lsps_builder
                    .with_handler(
                        Lsps2GetInfoRequest::METHOD.to_string(),
                        Arc::new(Lsps2GetInfoHandler::new(rpc_arc.clone(), secret)),
                    )
                    .with_handler(
                        Lsps2BuyRequest::METHOD.to_string(),
                        Arc::new(Lsps2BuyHandler::new(rpc_arc.clone(), secret)),
                    );
            }
            true
        } else {
            false
        };

        let lsps_service = lsps_builder.build();
        debug!("STARTING LSP WITH SERVICE: {:?}", lsps_service);
        let state = State {
            lsps_service,
            rpc: rpc_arc,
            rpc_path,
            lsps2_enabled: lsps2_enabled,
            lsps2_coordinators: Arc::new(Mutex::new(HashMap::new())),
        };
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
            let mut writer = LspsResponseWriter {
                peer_id: msg.peer_id,
                rpc: p.state().rpc.clone(),
            };

            // The payload inside CustomMsg is the actual JSON-RPC
            // request/notification, we wrap it to attach the peer_id as well.
            let payload = wrap_payload_with_peer_id(&req.payload, msg.peer_id);
            let service = p.state().lsps_service.clone();

            match service.handle_message(&payload, &mut writer).await {
                Ok(_) => {
                    debug!("Successfully handled LSPS message from {}", msg.peer_id);
                    continue_response
                }
                Err(e) => {
                    warn!(
                        "Failed to handle LSPS message {} from {}: {}",
                        hex::encode(&req.payload),
                        msg.peer_id,
                        e
                    );
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
            let mut writer = LspsResponseWriter {
                peer_id: msg.peer_id,
                rpc: p.state().rpc.clone(),
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
    rpc: Arc<Mutex<ClnRpc>>,
}

#[async_trait]
impl JsonRpcResponseWriter for LspsResponseWriter {
    async fn write(&mut self, payload: &[u8]) -> cln_lsps::jsonrpc::Result<()> {
        let mut rpc = self.rpc.lock().await;
        transport::send_custommsg(&mut rpc, payload.to_vec(), self.peer_id).await
    }
}

pub async fn on_hltc_accpted(
    p: Plugin<State>,
    v: serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    if !p.state().lsps2_enabled {
        return Ok(serde_json::to_value(HtlcAcceptedResponse {
            result: String::from("continue"),
            payment_key: None,
        })?);
    };

    debug!("calling htlc_accepted_hook handler for LSP payments");

    let hook: HtlcAcceptedRequest = serde_json::from_value(v)?;
    let onion = hook.onion;
    let htlc = hook.htlc;
    let jit_scid = match onion.short_channel_id {
        Some(scid) => scid,
        None => {
            debug!("is not a forward, continue.");
            return Ok(serde_json::to_value(HtlcAcceptedResponse {
                result: String::from("continue"),
                payment_key: None,
            })?);
        }
    };

    debug!(
        "got request with onion {:?} scid {} check datastore for requests",
        onion, jit_scid
    );

    let ds_req = ListdatastoreRequest {
        key: Some(vec![
            lsps2::DS_MAIN_KEY.to_string(),
            lsps2::DS_SUB_KEY.to_string(),
            jit_scid.to_string(),
        ]),
    };

    debug!(
        "got request for scid {} check datastore for requests {:?}",
        jit_scid, &ds_req
    );

    let res = p.state().rpc.lock().await.call_typed(&ds_req).await?;

    debug!("returned from datastore {:?}", &res);

    if let Some(ds) = res.datastore.into_iter().next() {
        if let Some(ds_data_raw) = ds.string {
            let ds_data: DatastoreEntry = serde_json::from_str(&ds_data_raw)?;
            let channel_info = ChannelInfo::new(
                ds_data.peer_id,
                ds_data.opening_fee_params.clone(),
                0,
                Amount::from_msat(ds_data.opening_fee_params.max_payment_size_msat.msat()),
                ds_data.expected_payment_size,
            );
            let coord = match p
                .state()
                .get_or_create_lsps2_coordinator(
                    jit_scid.to_u64(),
                    channel_info,
                    Duration::from_secs(90), // FIXME: make this an option, must be >=90s.
                )
                .await
            {
                Ok(c) => c,
                Err(e) => return Err(anyhow!("Could not process jit-htlc {}", e)),
            };
            let (action_tx, action_rx) = oneshot::channel();
            let res = coord
                .new_htlc_in(HtlcIn::new(htlc.id, htlc.amount_msat.msat(), action_tx))
                .await;
            match res {
                Ok(action) => match action {
                    JitHtlcAction::Wait => {
                        debug!("htlcs are being processed, waiting...");
                        let action = action_rx.await?;
                        return process_action(jit_scid, action, onion.payload);
                    }
                    JitHtlcAction::Resolve { payment_key } => {
                        return create_response("resolve", Some(hex::encode(payment_key)))
                    }
                    JitHtlcAction::Fail { failure_message: _ } => {
                        return create_response("fail", None)
                    }
                    JitHtlcAction::Continue {
                        payload: _,
                        forward_to,
                        extra_tlvs: _,
                        channel: _,
                    } => {
                        debug!(
                            "JIT channel coordinator for {} returned, continue.",
                            &jit_scid.to_string()
                        );
                        let res = HtlcAcceptedContinueResponse::new(None, forward_to, None);
                        return Ok(serde_json::to_value(&res)?);
                    }
                },
                Err(e) => return Err(anyhow!("Could not process new htlc  {}", e)),
            }
        } else {
            return Err(anyhow!("Datastore entry is empty"));
        }
    }
    // Is not a jit_payment, continue
    let r = HtlcAcceptedResponse {
        result: String::from("continue"),
        payment_key: None,
    };
    Ok(serde_json::to_value(&r)?)
}

fn create_response(
    result: &str,
    payment_key: Option<String>,
) -> Result<serde_json::Value, anyhow::Error> {
    let response = HtlcAcceptedResponse {
        result: result.to_string(),
        payment_key,
    };
    serde_json::to_value(&response).map_err(|e| anyhow!("Failed to serialize response: {}", e))
}

fn process_action(
    jit_scid: ShortChannelId,
    action: JitHtlcAction,
    pl: tlv::SerializedTlvStream,
) -> Result<serde_json::Value, anyhow::Error> {
    match action {
        JitHtlcAction::Wait => Err(anyhow!("Unexpected wait action")),
        JitHtlcAction::Continue {
            payload: _,
            forward_to,
            extra_tlvs: _,
            channel: scid,
        } => {
            debug!(
                "JIT channel coordinator for {} returned, continue.",
                &jit_scid.to_string()
            );

            let scid = scid.unwrap();
            let mut payload = pl.clone();
            payload.set_tu64(TLV_SHORT_CHANNEL_ID, scid);
            let payload = tlv::SerializedTlvStream::to_bytes(payload);
            debug!("Replace payload with {}", hex::encode(&payload));

            let res = HtlcAcceptedContinueResponse::new(None, forward_to, None);
            return Ok(serde_json::to_value(&res)?);
        }
        JitHtlcAction::Resolve { payment_key } => {
            create_response("resolve", Some(hex::encode(payment_key)))
        }
        JitHtlcAction::Fail { failure_message: _ } => create_response("fail", None),
    }
}
