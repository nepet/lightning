use anyhow::{anyhow, Context};
use async_trait::async_trait;
use cln_lsps::lsps0::model::Lsps0listProtocolsRequest;
use cln_lsps::lsps0::transport::{self, CustomMsg};
use cln_lsps::lsps2::handler::{Lsps2BuyHandler, Lsps2GetInfoHandler};
use cln_lsps::lsps2::model::{Lsps2BuyRequest, Lsps2GetInfoRequest};
use cln_lsps::util::wrap_payload_with_peer_id;
use cln_lsps::{
    jsonrpc::{
        server::{JsonRpcResponseWriter, JsonRpcServer},
        JsonRpcRequest,
    },
    lsps0::handler::Lsps0ListProtocolsHandler,
};
use cln_lsps::{lsps0, lsps2, util};
use cln_plugin::options::ConfigOption;
use cln_plugin::{options, Plugin};
use cln_rpc::notifications::CustomMsgNotification;
use cln_rpc::primitives::PublicKey;
use cln_rpc::ClnRpc;
use log::{debug, warn};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;

const LSP_FEATURE_BIT: usize = 729;

/// An option to enable this service.
const OPTION_ENABLED: options::FlagConfigOption = ConfigOption::new_flag(
    "lsps_service_enabled",
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
struct State {
    lsps_service: JsonRpcServer,
    rpc: Arc<Mutex<ClnRpc>>,
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
        if plugin.option(&lsps2::OPTION_ENABLED)? {
            let secret_hex = plugin.option(&lsps2::OPTION_PROMISE_SECRET)?;
            if let Some(secret_hex) = secret_hex {
                let secret: [u8; 32] = hex::decode(secret_hex.trim().to_lowercase())
                    .with_context(|| {
                        format!(
                            "Invalid hex string for promise secret: {}",
                            secret_hex.trim()
                        )
                    })?
                    .try_into()
                    .map_err(|v: Vec<u8>| {
                        anyhow!("Promise secret must be exactly 32 bytes, got {}", v.len())
                    })?;

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
        }

        let lsps_service = lsps_builder.build();
        let state = State {
            lsps_service,
            rpc: rpc_arc,
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
