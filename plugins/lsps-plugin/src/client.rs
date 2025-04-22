use anyhow::{anyhow, Context};
use cln_lsps::jsonrpc::client::JsonRpcClient;
use cln_lsps::lsps0::{
    self,
    transport::{Bolt8Transport, CustomMessageHookManager, WithCustomMessageHookManager},
};
// Assuming lsps0::parameter_validation is made public in lsps0/mod.rs
// If not, the model file needs adjustment as noted there.
use cln_lsps::lsps2::model::{
    compute_opening_fee, Lsps2BuyRequest, Lsps2BuyResponse, Lsps2GetInfoRequest,
    Lsps2GetInfoResponse, OpeningFeeParams,
};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use time::OffsetDateTime;

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
        .rpcmethod(
            "lsps-listprotocols",
            "List protocols supported by LSP peer {peer}",
            on_lsps_listprotocols,
        )
        .rpcmethod(
            "lsps-buy-jit-channel",
            "Request a JIT channel from LSP peer {peer} for optional {payment_size_msat} with optional {token}",
            on_lsps_buy_jit_channel,
        )
        .start(state)
        .await?
    {
        plugin.join().await
    } else {
        Ok(())
    }
}

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
    ).context("Failed to create Bolt8Transport")?;

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
/// Calls lsps2.get_info, selects parameters, calculates fee, calls lsps2.buy.
async fn on_lsps_buy_jit_channel(
    p: cln_plugin::Plugin<State>,
    v: serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    #[derive(Deserialize)]
    struct Request {
        peer: String,
        #[serde(default, with = "cln_rpc::serde_opt_milli_sat")]
        payment_size_msat: Option<cln_rpc::primitives::Amount>, // Optional: for fixed-amount invoices
        token: Option<String>, // Optional: for discounts/API keys
    }

    let req: Request = serde_json::from_value(v).context("Failed to parse request JSON")?;
    info!(
        "Handling lsps-buy-jit-channel request for peer {} with payment_size {:?} and token {:?}",
        req.peer, req.payment_size_msat, req.token
    );

    let dir = p.configuration().lightning_dir;
    let rpc_path = Path::new(&dir).join(&p.configuration().rpc_file);

    // --- Create Transport and Client ---
    let transport = Bolt8Transport::new(
        &req.peer,
        rpc_path.clone(), // Clone path for potential reuse
        p.state().hook_manager.clone(),
        None, // Use default timeout
    ).context("Failed to create Bolt8Transport")?;
    let client = JsonRpcClient::new(transport);

    // --- 1. Call lsps2.get_info ---
    info!("Calling lsps2.get_info for peer {}", req.peer);
    let get_info_request = Lsps2GetInfoRequest { token: req.token };
    let get_info_response: Lsps2GetInfoResponse = client
        .call_typed(get_info_request)
        .await
        .context("lsps2.get_info call failed")?;
    debug!("Received lsps2.get_info response: {:?}", get_info_response);

    if get_info_response.opening_fee_params_menu.is_empty() {
        return Err(anyhow!("LSP {} does not offer any JIT channel options", req.peer));
    }

    // --- 2. Select Fee Parameters ---
    // Simple strategy: choose the first valid option. Could be more sophisticated (e.g., cheapest).
    let selected_params = get_info_response
        .opening_fee_params_menu
        .iter()
        .find(|params| {
            // Basic validation on client side: check expiry and promise length
            let expiry_valid = OffsetDateTime::parse(&params.valid_until, &time::format_description::well_known::Iso8601::DEFAULT)
                .map(|expiry| expiry > OffsetDateTime::now_utc())
                .unwrap_or_else(|e| {
                    warn!("Failed to parse valid_until '{}' from LSP: {}", params.valid_until, e);
                    false // Treat unparseable dates as invalid
                });
            let promise_valid = params.promise.len() <= 512;

             if !expiry_valid { warn!("Ignoring expired fee params from LSP: {:?}", params); }
             if !promise_valid { warn!("Ignoring fee params with oversized promise from LSP: {:?}", params); }

             expiry_valid && promise_valid && params.validate() // Also run the model's validation
        })
        .cloned() // Clone the selected params
        .ok_or_else(|| anyhow!("No valid/unexpired fee parameters offered by LSP {}", req.peer))?;

    info!("Selected fee parameters: {:?}", selected_params);

    // --- 3. Validate Payment Size and Calculate Fee ---
    let payment_size_msat_u64 = req.payment_size_msat.map(|a| a.msat());

    if let Some(payment_size) = payment_size_msat_u64 {
         if payment_size < selected_params.min_payment_size_msat {
            return Err(anyhow!(
                "Requested payment size {}msat is below minimum {}msat required by LSP",
                payment_size, selected_params.min_payment_size_msat
            ));
        }
         if payment_size > selected_params.max_payment_size_msat {
            return Err(anyhow!(
                "Requested payment size {}msat is above maximum {}msat allowed by LSP",
                payment_size, selected_params.max_payment_size_msat
            ));
        }

        let opening_fee = compute_opening_fee(payment_size, &selected_params)
            .ok_or_else(|| anyhow!("Opening fee calculation overflowed for payment size {}", payment_size))?;

        info!(
            "Calculated opening fee: {}msat for payment size {}msat",
            opening_fee, payment_size
        );

        // Check if fee is payable (using placeholder HTLC min)
        const HTLC_MINIMUM_MSAT: u64 = 1000; // Should match LSP's expectation or be configurable
        if opening_fee.saturating_add(HTLC_MINIMUM_MSAT) > payment_size {
             return Err(anyhow!(
                "Calculated opening fee {}msat is too high for payment size {}msat (cannot forward minimum HTLC)",
                opening_fee, payment_size
            ));
        }
        // TODO: Optionally prompt user to confirm the fee before proceeding
        // info!("Proposed opening fee is {}msat. Proceed? (Y/N)", opening_fee);
        // ... wait for user input ...
    } else {
        info!("No payment size specified, requesting JIT channel for a variable-amount invoice.");
        // Check if the selected params allow for variable amount (implicitly they do if max > min)
        if selected_params.min_payment_size_msat >= selected_params.max_payment_size_msat {
             // This shouldn't happen if LSP follows spec, but good to check.
             warn!("Selected fee params seem unsuitable for variable amount: min >= max");
        }
    }


    // --- 4. Call lsps2.buy ---
    info!("Calling lsps2.buy for peer {}", req.peer);
    let buy_request = Lsps2BuyRequest {
        opening_fee_params: selected_params, // Pass the chosen params back
        payment_size_msat: payment_size_msat_u64,
    };
    let buy_response: Lsps2BuyResponse = client
        .call_typed(buy_request)
        .await
        .context("lsps2.buy call failed")?;

    debug!("Received lsps2.buy response: {:?}", buy_response);

    // Client-side validation of the response
    if !buy_response.validate() {
        warn!("Received invalid lsps2.buy response structure: {:?}", buy_response);
        return Err(anyhow!("Received invalid response from LSP for lsps2.buy"));
    }

    // --- 5. Return Result ---
    // The response contains the SCID and CLTV delta needed to create the invoice.
    // We should format this nicely for the user/caller.
    #[derive(Serialize)]
    struct BuyResult {
        jit_channel_scid: String,
        lsp_cltv_expiry_delta: u16,
        client_trusts_lsp: bool, // Provide a default if None
        // Potentially add calculated fee here for user info?
    }

    let result = BuyResult {
        jit_channel_scid: buy_response.jit_channel_scid,
        lsp_cltv_expiry_delta: buy_response.lsp_cltv_expiry_delta,
        client_trusts_lsp: buy_response.client_trusts_lsp.unwrap_or(false), // Default to false if omitted
    };

    info!("Successfully obtained JIT channel details: SCID={}, CLTV Delta={}", result.jit_channel_scid, result.lsp_cltv_expiry_delta);

    Ok(serde_json::to_value(result)?)
}
