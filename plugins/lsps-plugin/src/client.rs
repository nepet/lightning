use anyhow::{anyhow, Context};
use chrono::{Duration, Utc};
use cln_lsps::jsonrpc::client::JsonRpcClient;
use cln_lsps::lsps0::primitives::Msat;
use cln_lsps::lsps0::{
    self,
    transport::{Bolt8Transport, CustomMessageHookManager, WithCustomMessageHookManager},
};
use cln_lsps::lsps2::model::{
    compute_opening_fee, Lsps2BuyRequest, Lsps2BuyResponse, Lsps2GetInfoRequest,
    Lsps2GetInfoResponse, OpeningFeeParams,
};
use cln_lsps::model::tlv::ToBytes;
use cln_lsps::model::{
    tlv, HtlcAcceptedContinueResponse, HtlcAcceptedRequest, Onion, TLV_OUTGOING_CLTV,
    TLV_PAYMENT_SECRET,
};
use cln_lsps::util::{self};
use cln_rpc::model::requests::ListpeersRequest;
use cln_rpc::primitives::{AmountOrAny, PublicKey};
use cln_rpc::ClnRpc;
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::str::FromStr;

const LSP_FEATURE_BIT: usize = 729;

#[derive(Clone)]
struct State {
    hook_manager: CustomMessageHookManager,
}

impl WithCustomMessageHookManager for State {
    fn get_custommsg_hook_manager(&self) -> &CustomMessageHookManager {
        &self.hook_manager
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let hook_manager = CustomMessageHookManager::new();
    let state = State { hook_manager };

    if let Some(plugin) = cln_plugin::Builder::new(tokio::io::stdin(), tokio::io::stdout())
        .hook("custommsg", CustomMessageHookManager::on_custommsg::<State>)
        .hook("htlc_accepted", on_htlc_accepted)
        .rpcmethod(
            "lsps-listprotocols",
            "List protocols supported by LSP peer {peer}",
            on_lsps_listprotocols,
        )
        .rpcmethod(
            "lsps-buyjitchannel",
            "Command to request a JIT channel from an LSP",
            on_lsps_buy_jit_channel,
        )
        .rpcmethod(
            "lsps-lsps2-getinfo",
            "Low-level command to request the opening fee menu of an LSP",
            on_lsps_lsps2_getinfo,
        )
        .rpcmethod(
            "lsps-lsps2-buy",
            "Low-level command to return the lsps2.buy result from an ",
            on_lsps_lsps2_buy,
        )
        .start(state)
        .await?
    {
        plugin.join().await
    } else {
        Ok(())
    }
}

/// Rpc Method handler for `lsps-lsps2-getinfo`.
async fn on_lsps_lsps2_getinfo(
    p: cln_plugin::Plugin<State>,
    v: serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    let req: ClnRpcLsps2GetinfoRequest =
        serde_json::from_value(v).context("Failed to parse request JSON")?;
    debug!(
        "Requesting opening fee menu from lsp {} with token {:?}",
        req.lsp_id, req.token
    );

    let dir = p.configuration().lightning_dir;
    let rpc_path = Path::new(&dir).join(&p.configuration().rpc_file);
    let mut cln_client = cln_rpc::ClnRpc::new(rpc_path.clone()).await?;

    // Fail early: Check that we are connected to the peer and that it has the
    // LSP feature bit set.
    ensure_lsp_connected(&mut cln_client, &req.lsp_id).await?;

    // Create Transport and Client
    let transport = Bolt8Transport::new(
        &req.lsp_id,
        rpc_path.clone(), // Clone path for potential reuse
        p.state().hook_manager.clone(),
        None, // Use default timeout
    )
    .context("Failed to create Bolt8Transport")?;
    let client = JsonRpcClient::new(transport);

    // 1. Call lsps2.get_info.
    let info_req = Lsps2GetInfoRequest { token: req.token };
    let info_res: Lsps2GetInfoResponse = client
        .call_typed(info_req)
        .await
        .context("lsps2.get_info call failed")?;
    debug!("received lsps2.get_info response: {:?}", info_res);

    Ok(serde_json::to_value(info_res)?)
}

/// Rpc Method handler for `lsps-lsps2-buy`.
async fn on_lsps_lsps2_buy(
    p: cln_plugin::Plugin<State>,
    v: serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    let req: ClnRpcLsps2BuyRequest =
        serde_json::from_value(v).context("Failed to parse request JSON")?;
    debug!(
        "Asking for a channel from lsp {} with opening fee params {:?} and payment size {:?}",
        req.lsp_id, req.opening_fee_params, req.payment_size_msat
    );

    let dir = p.configuration().lightning_dir;
    let rpc_path = Path::new(&dir).join(&p.configuration().rpc_file);
    let mut cln_client = cln_rpc::ClnRpc::new(rpc_path.clone()).await?;

    // Fail early: Check that we are connected to the peer and that it has the
    // LSP feature bit set.
    ensure_lsp_connected(&mut cln_client, &req.lsp_id).await?;

    // Create Transport and Client
    let transport = Bolt8Transport::new(
        &req.lsp_id,
        rpc_path.clone(), // Clone path for potential reuse
        p.state().hook_manager.clone(),
        None, // Use default timeout
    )
    .context("Failed to create Bolt8Transport")?;
    let client = JsonRpcClient::new(transport);

    // Convert from AmountOrAny to Msat.
    let payment_size_msat = if let Some(payment_size) = req.payment_size_msat {
        match payment_size {
            AmountOrAny::Amount(amount) => Some(Msat::from_msat(amount.msat())),
            AmountOrAny::Any => None,
        }
    } else {
        None
    };

    let selected_params = req.opening_fee_params;

    if let Some(payment_size) = payment_size_msat {
        if payment_size < selected_params.min_payment_size_msat {
            return Err(anyhow!(
                "Requested payment size {}msat is below minimum {}msat required by LSP",
                payment_size,
                selected_params.min_payment_size_msat
            ));
        }
        if payment_size > selected_params.max_payment_size_msat {
            return Err(anyhow!(
                "Requested payment size {}msat is above maximum {}msat allowed by LSP",
                payment_size,
                selected_params.max_payment_size_msat
            ));
        }

        let opening_fee = compute_opening_fee(
            payment_size.msat(),
            selected_params.min_fee_msat.msat(),
            selected_params.proportional.ppm() as u64,
        )
        .ok_or_else(|| {
            warn!(
                "Opening fee calculation overflowed for payment size {}",
                payment_size
            );
            anyhow!("failed to calculate opening fee")
        })?;

        info!(
            "Calculated opening fee: {}msat for payment size {}msat",
            opening_fee, payment_size
        );
    } else {
        info!("No payment size specified, requesting JIT channel for a variable-amount invoice.");
        // Check if the selected params allow for variable amount (implicitly they do if max > min)
        if selected_params.min_payment_size_msat >= selected_params.max_payment_size_msat {
            // This shouldn't happen if LSP follows spec, but good to check.
            warn!("Selected fee params seem unsuitable for variable amount: min >= max");
        }
    }

    debug!("Calling lsps2.buy for peer {}", req.lsp_id);
    let buy_req = Lsps2BuyRequest {
        opening_fee_params: selected_params, // Pass the chosen params back
        payment_size_msat,
    };
    let buy_res: Lsps2BuyResponse = client
        .call_typed(buy_req)
        .await
        .context("lsps2.buy call failed")?;

    Ok(serde_json::to_value(buy_res)?)
}

/// RPC Method handler for `lsps-listprotocols`.
async fn on_lsps_listprotocols(
    p: cln_plugin::Plugin<State>,
    v: serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    #[derive(Deserialize)]
    struct Request {
        peer: String,
    }
    let dir = p.configuration().lightning_dir;
    let rpc_path = Path::new(&dir).join(&p.configuration().rpc_file);

    let req: Request = serde_json::from_value(v).context("Failed to parse request JSON")?;

    // Create the transport first and handle potential errors
    let transport = Bolt8Transport::new(
        &req.peer,
        rpc_path,
        p.state().hook_manager.clone(),
        None, // Use default timeout
    )
    .context("Failed to create Bolt8Transport")?;

    // Now create the client using the transport
    let client = JsonRpcClient::new(transport);

    info!("Requesting lsps0.list_protocols from peer {}", req.peer);
    let request = lsps0::model::Lsps0listProtocolsRequest {};
    let res: lsps0::model::Lsps0listProtocolsResponse = client
        .call_typed(request)
        .await
        .context("lsps0.list_protocols call failed")?;

    debug!("Received lsps0.list_protocols response: {:?}", res);
    Ok(serde_json::to_value(res)?)
}

/// RPC Method handler for `lsps-buy-jit-channel`.
/// Calls lsps2.get_info, selects parameters, calculates fee, calls lsps2.buy,
/// creates invoice.
async fn on_lsps_buy_jit_channel(
    p: cln_plugin::Plugin<State>,
    v: serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    #[derive(Deserialize)]
    struct Request {
        lsp_id: String,
        // Optional: for fixed-amount invoices
        payment_size_msat: Option<AmountOrAny>,
        // Optional: for discounts/API keys
        token: Option<String>,
    }

    let req: Request = serde_json::from_value(v).context("Failed to parse request JSON")?;
    debug!(
        "Handling lsps-buy-jit-channel request for peer {} with payment_size {:?} and token {:?}",
        req.lsp_id, req.payment_size_msat, req.token
    );

    let dir = p.configuration().lightning_dir;
    let rpc_path = Path::new(&dir).join(&p.configuration().rpc_file);
    let mut cln_client = cln_rpc::ClnRpc::new(rpc_path.clone()).await?;

    // 1. Get LSP's opening fee menu.
    let info_res: Lsps2GetInfoResponse = cln_client
        .call_raw(
            "lsps-lsps2-getinfo",
            &ClnRpcLsps2GetinfoRequest {
                lsp_id: req.lsp_id.clone(),
                token: req.token,
            },
        )
        .await?;

    // 2. Select Fee Parameters.
    // Simple strategy for now: choose the first valid option as LSPS2 requires
    // this to be the cheapest. Could be more sophisticated (e.g., user choice).
    let selected_params = info_res
        .opening_fee_params_menu
        .iter()
        .find(|params| {
            // Basic validation on client side: check expiry and promise length
            let fut_now = Utc::now() + Duration::minutes(1); // Add some extra time for network delay
            let expiry_valid = params.valid_until > fut_now;
            if !expiry_valid {
                warn!("Ignoring expired fee params from LSP {:?}", params);
            }
            expiry_valid
        })
        .cloned() // Clone the selected params
        .ok_or_else(|| {
            anyhow!(
                "No valid/unexpired fee parameters offered by LSP {}",
                req.lsp_id
            )
        })?;

    info!("Selected fee parameters: {:?}", selected_params);

    // 3. Request channel from LSP.
    let buy_res: Lsps2BuyResponse = cln_client
        .call_raw(
            "lsps-lsps2-buy",
            &ClnRpcLsps2BuyRequest {
                lsp_id: req.lsp_id.clone(),
                payment_size_msat: req.payment_size_msat,
                opening_fee_params: selected_params.clone(),
            },
        )
        .await?;

    debug!("Received lsps2.buy response: {:?}", buy_res);

    // We define the invoice expiry here to avoid cloning `selected_params`
    // as they are about to be moved to the `Lsps2BuyRequest`.
    let expiry = (selected_params.valid_until - Utc::now()).num_seconds();
    if expiry <= 10 {
        return Err(anyhow!(
            "Invoice lifetime is too short, options are valid until: {}",
            selected_params.valid_until,
        ));
    }

    // let op = ConfigOption::new_i64_with_default("cltv-final", 144, "Minimum final cltv delta");
    // let min_cltv = p.option(&op)?;

    // 4. Create and return invoice with a route hint pointing to the LSP, using
    // the scid we got from the LSP.
    let hint = RoutehintHopDev {
        id: req.lsp_id,
        short_channel_id: buy_res.jit_channel_scid.to_string(),
        fee_base_msat: Some(0),
        fee_proportional_millionths: 0,
        cltv_expiry_delta: u16::try_from(buy_res.lsp_cltv_expiry_delta)?,
    };

    let amount_msat = if let Some(payment_size) = req.payment_size_msat {
        payment_size
    } else {
        AmountOrAny::Any
    };

    let inv: cln_rpc::model::responses::InvoiceResponse = cln_client
        .call_raw(
            "invoice",
            &InvoiceRequest {
                amount_msat,
                dev_routes: Some(vec![vec![hint]]),
                description: String::from("TODO"), // TODO: Pass down description from rpc call
                label: gen_label(None),            // TODO: Pass down label from rpc call
                expiry: Some(expiry as u64),
                cltv: Some(u32::try_from(16 + 2)?), // TODO: FETCH REAL VALUE!
                deschashonly: None,
                preimage: None,
                exposeprivatechannels: None,
                fallbacks: None,
            },
        )
        .await?;

    // TODO: Add some more usefull information here.
    let out = LspsBuyJitChannelResponse { bolt11: inv.bolt11 };
    Ok(serde_json::to_value(out)?)
}

/// Checks that the node is connected to the peer and that it has the LSP
/// feature bit set.
async fn ensure_lsp_connected(cln_client: &mut ClnRpc, lsp_id: &str) -> Result<(), anyhow::Error> {
    let res = cln_client
        .call_typed(&ListpeersRequest {
            id: Some(PublicKey::from_str(lsp_id)?),
            level: None,
        })
        .await?;

    // unwrap in next line is safe as we checked that an item exists before.
    if res.peers.is_empty() || !res.peers.first().unwrap().connected {
        debug!("Node isn't connected to lsp {lsp_id}");
        return Err(anyhow!("not connected to lsp"));
    }

    res.peers
        .first()
        .filter(|peer| {
            // Check that feature bit is set
            peer.features.as_deref().map_or(false, |f_str| {
                if let Some(feature_bits) = hex::decode(f_str).ok() {
                    let mut fb = feature_bits.clone();
                    fb.reverse();
                    util::is_feature_bit_set(&fb, LSP_FEATURE_BIT)
                } else {
                    false
                }
            })
        })
        .ok_or_else(|| {
            debug!(
                "Peer {lsp_id} is not an lsp, feature bit {} is missing",
                LSP_FEATURE_BIT
            );
            anyhow!(
                "peer is not an lsp, feature bit {} is missing",
                LSP_FEATURE_BIT,
            )
        })?;

    Ok(())
}

/// Generates a unique label from an optional `String`. The given label is
/// appended by a timestamp (now).
fn gen_label(label: Option<&str>) -> String {
    let now = Utc::now();
    let millis = now.timestamp_millis();
    let l = label.unwrap_or_else(|| "lsps2.buy");
    format!("{}_{}", l, millis)
}

/// Checks if the accepted htlc is associated with a lsps request we issued.
async fn on_htlc_accepted(
    _p: cln_plugin::Plugin<State>,
    v: serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    let hook: HtlcAcceptedRequest = serde_json::from_value(v)?;
    let onion = hook.onion;
    let htlc = hook.htlc;
    log::debug!("GOT HTLC ACCEPTED HOOK REQUEST {:?}, {:?}", onion, htlc);

    let mut payload = onion.payload.clone();
    payload.set_tu64(TLV_OUTGOING_CLTV, 10);

    let payload = tlv::SerializedTlvStream::to_bytes(payload);
    log::debug!("Serialized payload: {}", hex::encode(&payload));

    let res = HtlcAcceptedContinueResponse::new(Some(payload), None, None);
    Ok(serde_json::to_value(&res)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClnRpcLsps2GetinfoRequest {
    lsp_id: String,
    token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClnRpcLsps2BuyRequest {
    lsp_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    payment_size_msat: Option<AmountOrAny>,
    opening_fee_params: OpeningFeeParams,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct LspsBuyJitChannelResponse {
    bolt11: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvoiceRequest {
    pub amount_msat: cln_rpc::primitives::AmountOrAny,
    pub description: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiry: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallbacks: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preimage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cltv: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deschashonly: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposeprivatechannels: Option<Vec<String>>,
    #[serde(rename = "dev-routes", skip_serializing_if = "Option::is_none")]
    pub dev_routes: Option<Vec<Vec<RoutehintHopDev>>>,
}

// This variant is used by dev-routes, using slightly different key names.
// TODO Remove once we have consolidated the routehint format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutehintHopDev {
    pub id: String,
    pub short_channel_id: String,
    pub fee_base_msat: Option<u64>,
    pub fee_proportional_millionths: u32,
    pub cltv_expiry_delta: u16,
}
