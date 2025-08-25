use crate::{
    jsonrpc::{server::RequestHandler, JsonRpcResponse as _, RequestObject, RpcError},
    lsps0::primitives::ShortChannelId,
    lsps2::{
        model::{
            DatastoreEntry, Lsps2BuyRequest, Lsps2BuyResponse, Lsps2GetInfoRequest,
            Lsps2GetInfoResponse, Lsps2PolicyGetInfoRequest, Lsps2PolicyGetInfoResponse,
            OpeningFeeParams, Promise,
        },
        DS_MAIN_KEY, DS_SUB_KEY,
    },
    util::unwrap_payload_with_peer_id,
};
use async_trait::async_trait;
use cln_rpc::{
    model::{
        requests::{self, DatastoreMode, DatastoreRequest, GetinfoRequest},
        responses::{DatastoreResponse, GetinfoResponse},
    },
    ClnRpc, RpcError as ClnRpcError,
};
use log::warn;
use rand::{rng, Rng};
use serde_json;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Default cltv delta for the last hop.
const DEFAULT_CLTV_EXPIRY_DELTA: u32 = 144;

#[async_trait]
pub trait Lsps2RpcCall: Send + Sync {
    async fn call_dev_lsps2_getinfo(
        &self,
        params: &Lsps2PolicyGetInfoRequest,
    ) -> Result<Lsps2PolicyGetInfoResponse, ClnRpcError>;

    async fn call_getinfo(&self, params: &GetinfoRequest) -> Result<GetinfoResponse, ClnRpcError>;

    async fn call_datastore(
        &self,
        params: &DatastoreRequest,
    ) -> Result<DatastoreResponse, ClnRpcError>;
}

pub struct ClnLsps2RpcCall {
    pub rpc: Arc<Mutex<ClnRpc>>,
}

impl ClnLsps2RpcCall {
    pub fn new(rpc: Arc<Mutex<ClnRpc>>) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl Lsps2RpcCall for ClnLsps2RpcCall {
    async fn call_dev_lsps2_getinfo(
        &self,
        params: &Lsps2PolicyGetInfoRequest,
    ) -> Result<Lsps2PolicyGetInfoResponse, ClnRpcError> {
        let mut rpc = self.rpc.lock().await;
        rpc.call_raw("dev-lsps2-getpolicy", params).await
    }

    async fn call_getinfo(&self, params: &GetinfoRequest) -> Result<GetinfoResponse, ClnRpcError> {
        let mut rpc = self.rpc.lock().await;
        rpc.call_typed(params).await
    }

    async fn call_datastore(
        &self,
        params: &DatastoreRequest,
    ) -> Result<DatastoreResponse, ClnRpcError> {
        let mut rpc = self.rpc.lock().await;
        rpc.call_typed(params).await
    }
}

/// Handler for the `lsps2.get_info` method.
pub struct Lsps2GetInfoHandler<T: Lsps2RpcCall> {
    pub rpc_call: T,
    pub promise_secret: [u8; 32],
}

impl Lsps2GetInfoHandler<ClnLsps2RpcCall> {
    pub fn new(rpc: Arc<Mutex<ClnRpc>>, promise_secret: [u8; 32]) -> Self {
        Self {
            rpc_call: ClnLsps2RpcCall::new(rpc),
            promise_secret,
        }
    }
}

/// The RequestHandler calls the internal rpc command `dev-lsps2-getinfo`. It
/// expects a plugin has registered this command and manages policies for the
/// LSPS2 service.
#[async_trait]
impl<T: Lsps2RpcCall + 'static> RequestHandler for Lsps2GetInfoHandler<T> {
    async fn handle(&self, payload: &[u8]) -> core::result::Result<Vec<u8>, RpcError> {
        let (payload, _) = unwrap_payload_with_peer_id(payload);

        let req: RequestObject<Lsps2GetInfoRequest> = serde_json::from_slice(&payload)
            .map_err(|e| RpcError::parse_error(format!("failed to parse request: {e}")))?;

        if req.id.is_none() {
            // Is a notification we can not reply so we just return
            return Ok(vec![]);
        }
        let params = req
            .params
            .ok_or(RpcError::invalid_params("expected params but was missing"))?;

        let policy_params: Lsps2PolicyGetInfoRequest = params.into();
        let res_data: Lsps2PolicyGetInfoResponse = self
            .rpc_call
            .call_dev_lsps2_getinfo(&policy_params)
            .await
            .map_err(|e| RpcError {
                code: 200,
                message: format!("failed to fetch policy {}", e),
                data: None,
            })?;

        let opening_fee_params_menu = res_data
            .policy_opening_fee_params_menu
            .iter()
            .map(|v| {
                let promise: Promise = v.get_hmac_hex(&self.promise_secret).try_into().unwrap();
                OpeningFeeParams {
                    min_fee_msat: v.min_fee_msat,
                    proportional: v.proportional,
                    valid_until: v.valid_until,
                    min_lifetime: v.min_lifetime,
                    max_client_to_self_delay: v.max_client_to_self_delay,
                    min_payment_size_msat: v.min_payment_size_msat,
                    max_payment_size_msat: v.max_payment_size_msat,
                    promise,
                }
            })
            .collect();

        let res = Lsps2GetInfoResponse {
            opening_fee_params_menu,
        }
        .into_response(req.id.unwrap()); // We checked that we got an id before.

        serde_json::to_vec(&res)
            .map_err(|e| RpcError::internal_error(format!("Failed to serialize response: {}", e)))
    }
}

/// Handler for the `lsps2.buy` method.
pub struct Lsps2BuyHandler<T> {
    pub rpc: T,
    pub promise_secret: [u8; 32],
}

impl Lsps2BuyHandler<ClnLsps2RpcCall> {
    pub fn new(rpc: Arc<Mutex<ClnRpc>>, promise_secret: [u8; 32]) -> Self {
        Self {
            rpc: ClnLsps2RpcCall::new(rpc),
            promise_secret,
        }
    }
}

#[async_trait]
impl<T: Lsps2RpcCall + 'static> RequestHandler for Lsps2BuyHandler<T> {
    async fn handle(&self, payload: &[u8]) -> core::result::Result<Vec<u8>, RpcError> {
        let (payload, peer_id) = unwrap_payload_with_peer_id(payload);

        let req: RequestObject<Lsps2BuyRequest> = serde_json::from_slice(&payload)
            .map_err(|e| RpcError::parse_error(format!("Failed to parse request: {}", e)))?;

        if req.id.is_none() {
            // Is a notification we can not reply so we just return
            return Ok(vec![]);
        }

        let req_params = req
            .params
            .ok_or_else(|| RpcError::invalid_request("Missing params field"))?;

        let fee_params = req_params.opening_fee_params;

        // FIXME: In the future we should replace the `None` with a meaningful
        // value that reflects the inbound capacity for this node from the
        // public network for a better pre-condition check on the payment_size.
        fee_params.validate(&self.promise_secret, req_params.payment_size_msat, None)?;

        // Generate a tmp scid to identify jit channel request in htlc.
        let get_info_req = GetinfoRequest {};
        let info = self.rpc.call_getinfo(&get_info_req).await.map_err(|e| {
            warn!("Failed to call getinfo via rpc {}", e);
            RpcError::internal_error("Internal error")
        })?;

        // FIXME: Future task: Check that we don't conflict with any jit scid we
        // already handed out -> Check datastore entries.
        let jit_scid_u64 = generate_jit_scid(info.blockheight);
        let jit_scid = ShortChannelId::from(jit_scid_u64);
        let ds_data = DatastoreEntry {
            peer_id,
            opening_fee_params: fee_params,
            expected_payment_size: req_params.payment_size_msat,
        };
        let ds_json = serde_json::to_string(&ds_data).map_err(|e| {
            warn!("Failed to serialize opening fee params to string {}", e);
            RpcError::internal_error("Internal error")
        })?;

        let ds_req = requests::DatastoreRequest {
            generation: None,
            hex: None,
            mode: Some(DatastoreMode::MUST_CREATE),
            string: Some(ds_json),
            key: vec![
                DS_MAIN_KEY.to_string(),
                DS_SUB_KEY.to_string(),
                jit_scid.to_string(),
            ],
        };

        let _ds_res = self.rpc.call_datastore(&ds_req).await.map_err(|e| {
            warn!("Failed to store jit request in ds via rpc {}", e);
            RpcError::internal_error("Internal error")
        })?;

        let res = Lsps2BuyResponse {
            jit_channel_scid: jit_scid,
            // We can make this configurable if necessary.
            lsp_cltv_expiry_delta: DEFAULT_CLTV_EXPIRY_DELTA,
            // We can implement the other mode later on as we might have to do
            // some additional work on core-lightning to enable this.
            client_trusts_lsp: false,
        }
        .into_response(req.id.unwrap()); // We checked that we got an id before.

        serde_json::to_vec(&res)
            .map_err(|e| RpcError::internal_error(format!("Failed to serialize response: {}", e)))
    }
}

fn generate_jit_scid(best_blockheigt: u32) -> u64 {
    let mut rng = rng();
    let block = best_blockheigt + 6; // Approx 1 hour in the future and should avoid collision with confirmed channels
    let tx_idx: u32 = rng.random_range(0..5000);
    let output_idx: u16 = rng.random_range(0..10);

    ((block as u64) << 40) | ((tx_idx as u64) << 16) | (output_idx as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        jsonrpc::{JsonRpcRequest, ResponseObject},
        lsps0::primitives::{Msat, Ppm},
        lsps2::model::PolicyOpeningFeeParams,
        util::wrap_payload_with_peer_id,
    };
    use chrono::{TimeZone, Utc};
    use cln_rpc::primitives::{Amount, PublicKey};

    const PUBKEY: [u8; 33] = [
        0x02, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87,
        0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16,
        0xf8, 0x17, 0x98,
    ];

    fn create_peer_id() -> PublicKey {
        PublicKey::from_slice(&PUBKEY).expect("Valid pubkey")
    }

    fn create_wrapped_request(request: &RequestObject<Lsps2GetInfoRequest>) -> Vec<u8> {
        let payload = serde_json::to_vec(request).expect("Failed to serialize request");
        wrap_payload_with_peer_id(&payload, create_peer_id())
    }

    #[derive(Clone)]
    pub struct MockLsps2RpcCall {
        pub dev_lsps2_getinfo_response: Option<Result<Lsps2PolicyGetInfoResponse, ClnRpcError>>,
        pub getinfo_response: Option<Result<GetinfoResponse, ClnRpcError>>,
        pub datastore_response: Option<Result<DatastoreResponse, ClnRpcError>>,

        pub datastore_calls: Arc<Mutex<Vec<DatastoreRequest>>>,
    }

    impl MockLsps2RpcCall {
        pub fn new() -> Self {
            Self {
                dev_lsps2_getinfo_response: None,
                getinfo_response: None,
                datastore_response: None,
                datastore_calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub fn with_dev_lsps2_getinfo_success(
            mut self,
            response: Lsps2PolicyGetInfoResponse,
        ) -> Self {
            self.dev_lsps2_getinfo_response = Some(Ok(response));
            self
        }

        pub fn with_dev_lsps2_getinfo_error(mut self, error: ClnRpcError) -> Self {
            self.dev_lsps2_getinfo_response = Some(Err(error));
            self
        }

        pub fn with_getinfo_success(mut self, response: GetinfoResponse) -> Self {
            self.getinfo_response = Some(Ok(response));
            self
        }

        pub fn with_getinfo_error(mut self, error: ClnRpcError) -> Self {
            self.getinfo_response = Some(Err(error));
            self
        }

        pub fn with_datastore_success(mut self, response: DatastoreResponse) -> Self {
            self.datastore_response = Some(Ok(response));
            self
        }

        pub fn with_datastore_error(mut self, error: ClnRpcError) -> Self {
            self.datastore_response = Some(Err(error));
            self
        }

        pub async fn datastore_calls(&self) -> Vec<DatastoreRequest> {
            let ds_vec = self.datastore_calls.lock().await;
            ds_vec.clone()
        }
    }

    #[async_trait]
    impl Lsps2RpcCall for MockLsps2RpcCall {
        async fn call_dev_lsps2_getinfo(
            &self,
            _params: &Lsps2PolicyGetInfoRequest,
        ) -> Result<Lsps2PolicyGetInfoResponse, ClnRpcError> {
            self.dev_lsps2_getinfo_response.clone().unwrap()
        }

        async fn call_getinfo(
            &self,
            _params: &GetinfoRequest,
        ) -> Result<GetinfoResponse, ClnRpcError> {
            self.getinfo_response.clone().unwrap()
        }

        async fn call_datastore(
            &self,
            params: &DatastoreRequest,
        ) -> Result<DatastoreResponse, ClnRpcError> {
            let mut ds_vec = self.datastore_calls.lock().await;
            ds_vec.push(params.clone());
            self.datastore_response.clone().unwrap()
        }
    }

    fn create_buy_request_payload(
        opening_fee_params: OpeningFeeParams,
        payment_size_msat: Option<Msat>,
        id: Option<String>,
    ) -> Vec<u8> {
        let buy_request = Lsps2BuyRequest {
            opening_fee_params,
            payment_size_msat,
        };

        let request = buy_request.into_request(id);
        let payload = serde_json::to_vec(&request).expect("Failed to serialize request");
        wrap_payload_with_peer_id(&payload, create_peer_id())
    }

    fn create_valid_opening_fee_params(promise_secret: &[u8; 32]) -> OpeningFeeParams {
        let policy_params = PolicyOpeningFeeParams {
            min_fee_msat: Msat(2000),
            proportional: Ppm(10000),
            valid_until: Utc::now() + chrono::Duration::hours(1), // Valid for 1 hour
            min_lifetime: 1000,
            max_client_to_self_delay: 42,
            min_payment_size_msat: Msat(1000000),
            max_payment_size_msat: Msat(100000000),
        };

        let promise = policy_params
            .get_hmac_hex(promise_secret)
            .try_into()
            .unwrap();

        OpeningFeeParams {
            min_fee_msat: policy_params.min_fee_msat,
            proportional: policy_params.proportional,
            valid_until: policy_params.valid_until,
            min_lifetime: policy_params.min_lifetime,
            max_client_to_self_delay: policy_params.max_client_to_self_delay,
            min_payment_size_msat: policy_params.min_payment_size_msat,
            max_payment_size_msat: policy_params.max_payment_size_msat,
            promise,
        }
    }

    fn successful_getinfo_response() -> GetinfoResponse {
        GetinfoResponse {
            lightning_dir: String::default(),
            alias: None,
            our_features: None,
            warning_bitcoind_sync: None,
            warning_lightningd_sync: None,
            address: None,
            binding: None,
            blockheight: 999999,
            color: String::default(),
            fees_collected_msat: Amount::from_btc(0),
            id: PublicKey::from_slice(&PUBKEY).unwrap(),
            network: String::default(),
            num_active_channels: 0,
            num_inactive_channels: 0,
            num_peers: 0,
            num_pending_channels: 0,
            version: String::default(),
        }
    }

    #[tokio::test]
    async fn test_successful_get_info() {
        let mock_response = Lsps2PolicyGetInfoResponse {
            policy_opening_fee_params_menu: vec![PolicyOpeningFeeParams {
                min_fee_msat: Msat(2000),
                proportional: Ppm(10000),
                valid_until: Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap(),
                min_lifetime: 1000,
                max_client_to_self_delay: 42,
                min_payment_size_msat: Msat(1000000),
                max_payment_size_msat: Msat(100000000),
            }],
        };

        let promise_secret = [0u8; 32];
        let promise = mock_response.policy_opening_fee_params_menu[0].get_hmac_hex(&promise_secret);

        let mock_rpc = MockLsps2RpcCall::new().with_dev_lsps2_getinfo_success(mock_response);
        let handler = Lsps2GetInfoHandler {
            rpc_call: mock_rpc,
            promise_secret,
        };

        let request = Lsps2GetInfoRequest { token: None }.into_request(Some("test-id".to_string()));
        let payload = create_wrapped_request(&request);

        let result = handler.handle(&payload).await.unwrap();
        let response: ResponseObject<Lsps2GetInfoResponse> =
            serde_json::from_slice(&result).unwrap();
        let response = response.into_inner().unwrap();

        assert_eq!(
            response.opening_fee_params_menu[0].min_payment_size_msat,
            Msat(1000000)
        );
        assert_eq!(
            response.opening_fee_params_menu[0].max_payment_size_msat,
            Msat(100000000)
        );
        assert_eq!(
            response.opening_fee_params_menu[0].promise,
            promise.try_into().unwrap()
        );
    }

    #[tokio::test]
    async fn test_get_info_rpc_error_handling() {
        let mock_rpc = MockLsps2RpcCall::new().with_dev_lsps2_getinfo_error(ClnRpcError {
            code: Some(-1),
            message: "not found".to_string(),
            data: None,
        });
        let promise_secret = [0u8; 32];
        let handler = Lsps2GetInfoHandler {
            rpc_call: mock_rpc,
            promise_secret,
        };

        let request = Lsps2GetInfoRequest { token: None }.into_request(Some("test-id".to_string()));
        let payload = create_wrapped_request(&request);

        let result = handler.handle(&payload).await;

        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.code, 200);
        assert!(error.message.contains("failed to fetch policy"));
    }

    #[tokio::test]
    async fn test_successful_buy_flow_mpp() {
        let promise_secret = [42u8; 32];
        let opening_fee_params = create_valid_opening_fee_params(&promise_secret);
        let payment_size = Msat(5000000); // 5M msat, within valid range

        let mock_rpc = MockLsps2RpcCall::new()
            .with_getinfo_success(successful_getinfo_response())
            .with_datastore_success(DatastoreResponse {
                generation: None,
                hex: None,
                string: None,
                key: vec![],
            });

        let handler = Lsps2BuyHandler {
            rpc: mock_rpc.clone(),
            promise_secret,
        };

        // Setting a payment_size as we want to test the variant
        // "MPP+fixed-invoice".
        let payload = create_buy_request_payload(
            opening_fee_params,
            Some(payment_size),
            Some("test-buy-id".to_string()),
        );

        let result = handler.handle(&payload).await.unwrap();
        let response: ResponseObject<Lsps2BuyResponse> = serde_json::from_slice(&result).unwrap();
        let buy_response = response.into_inner().unwrap();

        // Verify response structure
        assert_eq!(
            buy_response.lsp_cltv_expiry_delta,
            DEFAULT_CLTV_EXPIRY_DELTA
        );
        assert_eq!(buy_response.client_trusts_lsp, false);

        // Verify JIT SCID is reasonable (based on block height + 6)
        let expected_block = 999999 + 6;
        let jit_scid_u64: u64 = buy_response.jit_channel_scid.to_u64();
        let extracted_block = (jit_scid_u64 >> 40) as u32;
        assert_eq!(extracted_block, expected_block);

        // Check that we set the correct flow variant in the datastore
        let ds_calls = mock_rpc.datastore_calls().await;
        let ds_json = ds_calls.first().unwrap().string.clone().unwrap();
        let ds_entry: DatastoreEntry = serde_json::from_str(&ds_json).unwrap();
        assert_eq!(ds_entry.expected_payment_size, Some(payment_size));
    }

    #[tokio::test]
    async fn test_successful_buy_flow_no_mpp() {
        let promise_secret = [42u8; 32];
        let opening_fee_params = create_valid_opening_fee_params(&promise_secret);

        let mock_rpc = MockLsps2RpcCall::new()
            .with_getinfo_success(successful_getinfo_response())
            .with_datastore_success(DatastoreResponse {
                generation: None,
                hex: None,
                string: None,
                key: vec![],
            });

        let handler = Lsps2BuyHandler {
            rpc: mock_rpc.clone(),
            promise_secret,
        };

        // Not setting a payment_size as we want to test the variant
        // "no-MPP+var-invoice".
        let payload =
            create_buy_request_payload(opening_fee_params, None, Some("test-buy-id".to_string()));

        let result = handler.handle(&payload).await.unwrap();
        let response: ResponseObject<Lsps2BuyResponse> = serde_json::from_slice(&result).unwrap();
        let buy_response = response.into_inner().unwrap();

        // Verify response structure
        assert_eq!(
            buy_response.lsp_cltv_expiry_delta,
            DEFAULT_CLTV_EXPIRY_DELTA
        );
        assert_eq!(buy_response.client_trusts_lsp, false);

        // Verify JIT SCID is reasonable (based on block height + 6)
        let expected_block = 999999 + 6;
        let jit_scid_u64: u64 = buy_response.jit_channel_scid.to_u64();
        let extracted_block = (jit_scid_u64 >> 40) as u32;
        assert_eq!(extracted_block, expected_block);

        // Check that we set the correct flow variant in the datastore
        let ds_calls = mock_rpc.datastore_calls().await;
        let ds_json = ds_calls.first().unwrap().string.clone().unwrap();
        let ds_entry: DatastoreEntry = serde_json::from_str(&ds_json).unwrap();
        assert!(ds_entry.expected_payment_size.is_none())
    }

    #[tokio::test]
    async fn test_buy_getinfo_error() {
        let promise_secret = [42u8; 32];
        let opening_fee_params = create_valid_opening_fee_params(&promise_secret);

        let mock_rpc = MockLsps2RpcCall::new().with_getinfo_error(ClnRpcError {
            code: Some(201),
            message: "call failed".to_string(),
            data: None,
        });

        let handler = Lsps2BuyHandler {
            rpc: mock_rpc.clone(),
            promise_secret,
        };

        let payload =
            create_buy_request_payload(opening_fee_params, None, Some("test-buy-id".to_string()));

        let result = handler.handle(&payload).await;
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.code, -32603);
        assert!(error.message.contains("Internal error"));
    }

    #[tokio::test]
    async fn test_buy_datastore_error() {
        let promise_secret = [42u8; 32];
        let opening_fee_params = create_valid_opening_fee_params(&promise_secret);

        let mock_rpc = MockLsps2RpcCall::new()
            .with_getinfo_success(successful_getinfo_response())
            .with_datastore_error(ClnRpcError {
                code: Some(-1),
                message: "already exists".to_string(),
                data: None,
            });
        let handler = Lsps2BuyHandler {
            rpc: mock_rpc.clone(),
            promise_secret,
        };

        let payload =
            create_buy_request_payload(opening_fee_params, None, Some("test-buy-id".to_string()));

        let result = handler.handle(&payload).await;
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.code, -32603);
        assert!(error.message.contains("Internal error"));
    }

    #[tokio::test]
    async fn test_missing_params() {
        let handler = Lsps2BuyHandler {
            rpc: MockLsps2RpcCall::new(),
            promise_secret: [42u8; 32],
        };

        // Create request with no params
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "lsps2.buy",
            "id": "test-id"
            // Missing "params"
        });

        let payload =
            wrap_payload_with_peer_id(&serde_json::to_vec(&request).unwrap(), create_peer_id());

        let result = handler.handle(&payload).await;
        assert!(result.is_err());

        let error = result.unwrap_err();
        assert_eq!(error.code, -32600); // Invalid request
        assert!(error.message.contains("Missing params field"));
    }
}
