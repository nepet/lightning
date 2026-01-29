use crate::{
    core::lsps2::persistence::PersistedSession,
    core::lsps2::provider::{
        Blockheight, BlockheightProvider, ChannelInfo, ChannelState as ProviderChannelState,
        DatastoreProvider, FundChannelCompleteResult, FundChannelStartResult, FundPsbtResult,
        LightningProvider, Lsps2OfferProvider, SendPsbtResult, SessionPersistenceProvider,
        SignPsbtResult,
    },
    core::lsps2::session::SessionId,
    proto::{
        lsps0::Msat,
        lsps2::{
            DatastoreEntry, Lsps2PolicyGetChannelCapacityRequest,
            Lsps2PolicyGetChannelCapacityResponse, Lsps2PolicyGetInfoRequest,
            Lsps2PolicyGetInfoResponse, OpeningFeeParams,
        },
    },
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Txid;
use cln_rpc::{
    model::{
        requests::{
            CloseRequest, DatastoreMode, DatastoreRequest, DeldatastoreRequest,
            FundchannelCompleteRequest, FundchannelRequest, FundchannelStartRequest,
            FundpsbtRequest, GetinfoRequest, ListdatastoreRequest, ListpeerchannelsRequest,
            SendpsbtRequest, SignpsbtRequest, UnreserveinputsRequest,
        },
        responses::ListdatastoreResponse,
    },
    primitives::{Amount, AmountOrAll, ChannelState, Feerate, Sha256, ShortChannelId},
    ClnRpc,
};
use core::fmt;
use serde::Serialize;
use std::path::PathBuf;
use std::str::FromStr;

pub const DS_MAIN_KEY: &str = "lsps";
pub const DS_SUB_KEY: &str = "lsps2";
pub const DS_SESSION_KEY: &str = "session";

#[derive(Clone)]
pub struct ClnApiRpc {
    rpc_path: PathBuf,
}

impl ClnApiRpc {
    pub fn new(rpc_path: PathBuf) -> Self {
        Self { rpc_path }
    }

    async fn create_rpc(&self) -> Result<ClnRpc> {
        ClnRpc::new(&self.rpc_path).await
    }
}

#[async_trait]
impl LightningProvider for ClnApiRpc {
    async fn fund_jit_channel(
        &self,
        peer_id: &PublicKey,
        amount: &Msat,
    ) -> Result<(Sha256, String)> {
        log::debug!(
            "fund_jit_channel: connecting to RPC socket at {:?}, peer={}, amount={}",
            self.rpc_path, peer_id, amount.msat()
        );
        let mut rpc = self.create_rpc().await?;
        log::debug!("fund_jit_channel: RPC connected, calling fundchannel");
        let res = rpc
            .call_typed(&FundchannelRequest {
                announce: Some(false),
                close_to: None,
                compact_lease: None,
                feerate: None,
                minconf: None,
                mindepth: Some(0),
                push_msat: None,
                request_amt: None,
                reserve: None,
                channel_type: Some(vec![12, 46, 50]),
                utxos: None,
                amount: AmountOrAll::Amount(Amount::from_msat(amount.msat())),
                id: peer_id.to_owned(),
            })
            .await
            .with_context(|| "calling fundchannel")?;
        log::debug!(
            "fund_jit_channel: fundchannel returned channel_id={}, txid={}",
            res.channel_id, res.txid
        );
        Ok((res.channel_id, res.txid))
    }

    async fn is_channel_ready(&self, peer_id: &PublicKey, channel_id: &Sha256) -> Result<bool> {
        let mut rpc = self.create_rpc().await?;
        let r = rpc
            .call_typed(&ListpeerchannelsRequest {
                id: Some(peer_id.to_owned()),
                short_channel_id: None,
            })
            .await
            .with_context(|| "calling listpeerchannels")?;

        let chs = r
            .channels
            .iter()
            .find(|&ch| ch.channel_id.is_some_and(|id| id == *channel_id));
        if let Some(ch) = chs {
            if ch.state == ChannelState::CHANNELD_NORMAL {
                return Ok(true);
            }
        }

        return Ok(false);
    }

    // -------------------------------------------------------------------------
    // New methods for MPP flow with withheld funding
    // -------------------------------------------------------------------------

    async fn fund_channel_start(
        &self,
        peer_id: &PublicKey,
        amount_sat: u64,
        announce: bool,
        mindepth: Option<u32>,
    ) -> Result<FundChannelStartResult> {
        let mut rpc = self.create_rpc().await?;
        let res = rpc
            .call_typed(&FundchannelStartRequest {
                id: peer_id.to_owned(),
                amount: Amount::from_sat(amount_sat),
                announce: Some(announce),
                mindepth,
                feerate: None,
                close_to: None,
                push_msat: None,
                reserve: None,
                // Request anchors + static_remotekey channel type
                channel_type: Some(vec![12, 22, 50]),
            })
            .await
            .with_context(|| "calling fundchannel_start")?;

        Ok(FundChannelStartResult {
            funding_address: res.funding_address,
            scriptpubkey: res.scriptpubkey,
            mindepth: res.mindepth,
        })
    }

    async fn fund_channel_complete_withheld(
        &self,
        peer_id: &PublicKey,
        psbt: &str,
    ) -> Result<FundChannelCompleteResult> {
        let mut rpc = self.create_rpc().await?;
        let res = rpc
            .call_typed(&FundchannelCompleteRequest {
                id: peer_id.to_owned(),
                psbt: psbt.to_string(),
                // CRITICAL: withhold=true means don't broadcast the funding tx
                withhold: Some(true),
            })
            .await
            .with_context(|| "calling fundchannel_complete with withhold=true")?;

        // Convert Sha256 to [u8; 32]
        let channel_id: [u8; 32] = *res.channel_id.as_byte_array();

        Ok(FundChannelCompleteResult {
            channel_id,
            commitments_secured: res.commitments_secured,
        })
    }

    async fn broadcast_funding(&self, psbt: &str) -> Result<SendPsbtResult> {
        let mut rpc = self.create_rpc().await?;
        let res = rpc
            .call_typed(&SendpsbtRequest {
                psbt: psbt.to_string(),
                reserve: None,
            })
            .await
            .with_context(|| "calling sendpsbt")?;

        // Parse txid string into bitcoin::Txid
        let txid = Txid::from_str(&res.txid)
            .with_context(|| format!("parsing txid '{}' from sendpsbt response", res.txid))?;

        Ok(SendPsbtResult { tx: res.tx, txid })
    }

    async fn get_channel_info(
        &self,
        peer_id: &PublicKey,
        channel_id: Option<&[u8; 32]>,
    ) -> Result<Option<ChannelInfo>> {
        let mut rpc = self.create_rpc().await?;
        let res = rpc
            .call_typed(&ListpeerchannelsRequest {
                id: Some(peer_id.to_owned()),
                short_channel_id: None,
            })
            .await
            .with_context(|| "calling listpeerchannels")?;

        // Find matching channel
        let channel = if let Some(cid) = channel_id {
            res.channels.iter().find(|ch| {
                ch.channel_id
                    .as_ref()
                    .map(|id| *id.as_byte_array() == *cid)
                    .unwrap_or(false)
            })
        } else {
            // Return first channel if no channel_id specified
            res.channels.first()
        };

        let Some(ch) = channel else {
            return Ok(None);
        };

        // Convert CLN ChannelState to our ChannelState
        let state = convert_channel_state(ch.state);

        // Extract channel_id as [u8; 32]
        let channel_id = ch.channel_id.as_ref().map(|id| *id.as_byte_array());

        // Extract alias SCID (local alias for forwarding before confirmation)
        let alias_scid = ch.alias.as_ref().and_then(|a| a.local);

        // Extract withheld flag from funding info
        let withheld = ch.funding.as_ref().and_then(|f| f.withheld);

        Ok(Some(ChannelInfo {
            state,
            peer_connected: ch.peer_connected,
            channel_id,
            short_channel_id: ch.short_channel_id,
            alias_scid,
            withheld,
        }))
    }

    async fn fund_psbt(
        &self,
        amount_sat: u64,
        feerate: &str,
        startweight: u32,
    ) -> Result<FundPsbtResult> {
        let feerate_parsed = Feerate::try_from(feerate)
            .with_context(|| format!("parsing feerate '{}'", feerate))?;

        let mut rpc = self.create_rpc().await?;
        let res = rpc
            .call_typed(&FundpsbtRequest {
                satoshi: AmountOrAll::Amount(Amount::from_sat(amount_sat)),
                feerate: feerate_parsed,
                startweight,
                excess_as_change: Some(true),
                opening_anchor_channel: Some(true),
                reserve: Some(2016),
                minconf: None,
                locktime: None,
                min_witness_weight: None,
                nonwrapped: None,
            })
            .await
            .with_context(|| "calling fundpsbt")?;

        Ok(FundPsbtResult {
            psbt: res.psbt,
            feerate_per_kw: res.feerate_per_kw,
            estimated_final_weight: res.estimated_final_weight,
        })
    }

    async fn sign_psbt(&self, psbt: &str) -> Result<SignPsbtResult> {
        let mut rpc = self.create_rpc().await?;
        let res = rpc
            .call_typed(&SignpsbtRequest {
                psbt: psbt.to_string(),
                signonly: None,
            })
            .await
            .with_context(|| "calling signpsbt")?;

        Ok(SignPsbtResult {
            signed_psbt: res.signed_psbt,
        })
    }

    async fn unreserve_inputs(&self, psbt: &str) -> Result<()> {
        let mut rpc = self.create_rpc().await?;
        rpc.call_typed(&UnreserveinputsRequest {
            psbt: psbt.to_string(),
            reserve: None,
        })
        .await
        .with_context(|| "calling unreserveinputs")?;

        Ok(())
    }

    async fn close_channel(&self, channel_id: &[u8; 32]) -> Result<()> {
        let channel_id_hex = hex::encode(channel_id);
        let mut rpc = self.create_rpc().await?;
        rpc.call_typed(&CloseRequest {
            id: channel_id_hex,
            unilateraltimeout: Some(1),
            destination: None,
            fee_negotiation_step: None,
            force_lease_closed: None,
            wrong_funding: None,
            feerange: None,
        })
        .await
        .with_context(|| "calling close on withheld channel")?;

        Ok(())
    }
}

/// Convert CLN's ChannelState enum to our provider ChannelState.
fn convert_channel_state(state: ChannelState) -> ProviderChannelState {
    match state {
        ChannelState::OPENINGD => ProviderChannelState::Openingd,
        ChannelState::CHANNELD_AWAITING_LOCKIN => ProviderChannelState::ChanneldAwaitingLockin,
        ChannelState::CHANNELD_NORMAL => ProviderChannelState::ChanneldNormal,
        ChannelState::CHANNELD_SHUTTING_DOWN => ProviderChannelState::ChanneldShuttingDown,
        ChannelState::CLOSINGD_SIGEXCHANGE => ProviderChannelState::ClosingdSigexchange,
        ChannelState::CLOSINGD_COMPLETE => ProviderChannelState::ClosingdComplete,
        ChannelState::AWAITING_UNILATERAL => ProviderChannelState::AwaitingUnilateral,
        ChannelState::FUNDING_SPEND_SEEN => ProviderChannelState::FundingSpendSeen,
        ChannelState::ONCHAIN => ProviderChannelState::Onchain,
        ChannelState::DUALOPEND_OPEN_INIT => ProviderChannelState::DualopendOpenInit,
        ChannelState::DUALOPEND_AWAITING_LOCKIN => ProviderChannelState::DualopendAwaitingLockin,
        ChannelState::DUALOPEND_OPEN_COMMITTED => ProviderChannelState::DualopendOpenCommitted,
        ChannelState::DUALOPEND_OPEN_COMMIT_READY => ProviderChannelState::DualopendOpenCommitReady,
        ChannelState::CHANNELD_AWAITING_SPLICE => ProviderChannelState::ChanneldAwaitingSplice,
    }
}

#[async_trait]
impl DatastoreProvider for ClnApiRpc {
    async fn store_buy_request(
        &self,
        scid: &ShortChannelId,
        peer_id: &PublicKey,
        opening_fee_params: &OpeningFeeParams,
        expected_payment_size: &Option<Msat>,
    ) -> Result<bool> {
        let mut rpc = self.create_rpc().await?;
        #[derive(Serialize)]
        struct BorrowedDatastoreEntry<'a> {
            peer_id: &'a PublicKey,
            opening_fee_params: &'a OpeningFeeParams,
            #[serde(borrow)]
            expected_payment_size: &'a Option<Msat>,
        }

        let ds = BorrowedDatastoreEntry {
            peer_id,
            opening_fee_params,
            expected_payment_size,
        };
        let json_str = serde_json::to_string(&ds)?;

        let ds = DatastoreRequest {
            generation: None,
            hex: None,
            mode: Some(DatastoreMode::MUST_CREATE),
            string: Some(json_str),
            key: vec![
                DS_MAIN_KEY.to_string(),
                DS_SUB_KEY.to_string(),
                scid.to_string(),
            ],
        };

        let _ = rpc
            .call_typed(&ds)
            .await
            .map_err(anyhow::Error::new)
            .with_context(|| "calling datastore")?;

        Ok(true)
    }

    async fn get_buy_request(&self, scid: &ShortChannelId) -> Result<DatastoreEntry> {
        let mut rpc = self.create_rpc().await?;
        let key = vec![
            DS_MAIN_KEY.to_string(),
            DS_SUB_KEY.to_string(),
            scid.to_string(),
        ];
        let res = rpc
            .call_typed(&ListdatastoreRequest {
                key: Some(key.clone()),
            })
            .await
            .with_context(|| "calling listdatastore")?;

        let (rec, _) = deserialize_by_key(&res, key)?;
        Ok(rec)
    }

    async fn del_buy_request(&self, scid: &ShortChannelId) -> Result<()> {
        let mut rpc = self.create_rpc().await?;
        let key = vec![
            DS_MAIN_KEY.to_string(),
            DS_SUB_KEY.to_string(),
            scid.to_string(),
        ];

        let _ = rpc
            .call_typed(&DeldatastoreRequest {
                generation: None,
                key,
            })
            .await;

        Ok(())
    }
}

#[async_trait]
impl Lsps2OfferProvider for ClnApiRpc {
    async fn get_offer(
        &self,
        request: &Lsps2PolicyGetInfoRequest,
    ) -> Result<Lsps2PolicyGetInfoResponse> {
        let mut rpc = self.create_rpc().await?;
        rpc.call_raw("lsps2-policy-getpolicy", request)
            .await
            .context("failed to call lsps2-policy-getpolicy")
    }

    async fn get_channel_capacity(
        &self,
        params: &Lsps2PolicyGetChannelCapacityRequest,
    ) -> Result<Lsps2PolicyGetChannelCapacityResponse> {
        let mut rpc = self.create_rpc().await?;
        rpc.call_raw("lsps2-policy-getchannelcapacity", params)
            .await
            .map_err(anyhow::Error::new)
            .with_context(|| "calling lsps2-policy-getchannelcapacity")
    }
}

#[async_trait]
impl BlockheightProvider for ClnApiRpc {
    async fn get_blockheight(&self) -> Result<Blockheight> {
        let mut rpc = self.create_rpc().await?;
        let info = rpc
            .call_typed(&GetinfoRequest {})
            .await
            .map_err(anyhow::Error::new)
            .with_context(|| "calling getinfo")?;
        Ok(info.blockheight)
    }
}

#[derive(Debug)]
pub enum DsError {
    /// No datastore entry with this exact key.
    NotFound { key: Vec<String> },
    /// Entry existed but had neither `string` nor `hex`.
    MissingValue { key: Vec<String> },
    /// JSON parse failed (from `string` or decoded `hex`).
    JsonParse {
        key: Vec<String>,
        source: serde_json::Error,
    },
    /// Hex decode failed.
    HexDecode {
        key: Vec<String>,
        source: hex::FromHexError,
    },
}

impl fmt::Display for DsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DsError::NotFound { key } => write!(f, "no datastore entry for key {:?}", key),
            DsError::MissingValue { key } => write!(
                f,
                "datastore entry had neither `string` nor `hex` for key {:?}",
                key
            ),
            DsError::JsonParse { key, source } => {
                write!(f, "failed to parse JSON at key {:?}: {}", key, source)
            }
            DsError::HexDecode { key, source } => {
                write!(f, "failed to decode hex at key {:?}: {}", key, source)
            }
        }
    }
}

impl std::error::Error for DsError {}

pub fn deserialize_by_key<K>(
    resp: &ListdatastoreResponse,
    key: K,
) -> std::result::Result<(DatastoreEntry, Option<u64>), DsError>
where
    K: AsRef<[String]>,
{
    let wanted: &[String] = key.as_ref();

    let ds = resp
        .datastore
        .iter()
        .find(|d| d.key.as_slice() == wanted)
        .ok_or_else(|| DsError::NotFound {
            key: wanted.to_vec(),
        })?;

    // Prefer `string`, fall back to `hex`
    if let Some(s) = &ds.string {
        let value = serde_json::from_str::<DatastoreEntry>(s).map_err(|e| DsError::JsonParse {
            key: ds.key.clone(),
            source: e,
        })?;
        return Ok((value, ds.generation));
    }

    if let Some(hx) = &ds.hex {
        let bytes = hex::decode(hx).map_err(|e| DsError::HexDecode {
            key: ds.key.clone(),
            source: e,
        })?;
        let value =
            serde_json::from_slice::<DatastoreEntry>(&bytes).map_err(|e| DsError::JsonParse {
                key: ds.key.clone(),
                source: e,
            })?;
        return Ok((value, ds.generation));
    }

    Err(DsError::MissingValue {
        key: ds.key.clone(),
    })
}

// ============================================================================
// Session Persistence Implementation
// ============================================================================

impl ClnApiRpc {
    /// Helper to build the datastore key for a session.
    fn session_key(session_id: SessionId) -> Vec<String> {
        vec![
            DS_MAIN_KEY.to_string(),
            DS_SUB_KEY.to_string(),
            DS_SESSION_KEY.to_string(),
            session_id.0.to_string(),
        ]
    }

    /// Helper to build the prefix key for listing all sessions.
    fn session_prefix_key() -> Vec<String> {
        vec![
            DS_MAIN_KEY.to_string(),
            DS_SUB_KEY.to_string(),
            DS_SESSION_KEY.to_string(),
        ]
    }
}

#[async_trait]
impl SessionPersistenceProvider for ClnApiRpc {
    async fn save_session(&self, session: &PersistedSession) -> Result<()> {
        let mut rpc = self.create_rpc().await?;

        let json_str = serde_json::to_string(session)
            .with_context(|| "serializing session for persistence")?;

        let key = Self::session_key(session.id());

        // Use MUST_REPLACE to create or update
        // Note: CLN datastore doesn't have a direct "upsert" mode,
        // so we try CREATE_OR_REPLACE which handles both cases
        let request = DatastoreRequest {
            key: key.clone(),
            string: Some(json_str),
            hex: None,
            mode: Some(DatastoreMode::CREATE_OR_REPLACE),
            generation: None,
        };

        rpc.call_typed(&request)
            .await
            .with_context(|| format!("saving session to datastore: {:?}", key))?;

        Ok(())
    }

    async fn load_session(&self, session_id: SessionId) -> Result<Option<PersistedSession>> {
        let mut rpc = self.create_rpc().await?;

        let key = Self::session_key(session_id);

        let res = rpc
            .call_typed(&ListdatastoreRequest {
                key: Some(key.clone()),
            })
            .await
            .with_context(|| "listing datastore for session")?;

        // Check if the session exists
        if res.datastore.is_empty() {
            return Ok(None);
        }

        // Find the exact key match
        let ds = res.datastore.iter().find(|d| d.key == key);
        let Some(ds) = ds else {
            return Ok(None);
        };

        // Parse the session from JSON
        let session = if let Some(s) = &ds.string {
            serde_json::from_str::<PersistedSession>(s)
                .with_context(|| "parsing persisted session JSON")?
        } else if let Some(hx) = &ds.hex {
            let bytes = hex::decode(hx).with_context(|| "decoding persisted session hex")?;
            serde_json::from_slice::<PersistedSession>(&bytes)
                .with_context(|| "parsing persisted session from hex")?
        } else {
            return Ok(None);
        };

        Ok(Some(session))
    }

    async fn delete_session(&self, session_id: SessionId) -> Result<()> {
        let mut rpc = self.create_rpc().await?;

        let key = Self::session_key(session_id);

        // Delete the session - ignore if not found
        let _ = rpc
            .call_typed(&DeldatastoreRequest {
                key,
                generation: None,
            })
            .await;

        Ok(())
    }

    async fn load_all_sessions(&self) -> Result<Vec<PersistedSession>> {
        let mut rpc = self.create_rpc().await?;

        let prefix = Self::session_prefix_key();

        let res = rpc
            .call_typed(&ListdatastoreRequest {
                key: Some(prefix.clone()),
            })
            .await
            .with_context(|| "listing all sessions from datastore")?;

        let mut sessions = Vec::new();

        for ds in &res.datastore {
            // Only process entries that match our prefix (4 components: lsps/lsps2/session/scid)
            if ds.key.len() != 4 {
                continue;
            }

            // Parse the session
            let session_result = if let Some(s) = &ds.string {
                serde_json::from_str::<PersistedSession>(s)
            } else if let Some(hx) = &ds.hex {
                match hex::decode(hx) {
                    Ok(bytes) => serde_json::from_slice::<PersistedSession>(&bytes),
                    Err(_) => continue, // Skip invalid entries
                }
            } else {
                continue; // Skip entries without data
            };

            match session_result {
                Ok(session) => sessions.push(session),
                Err(e) => {
                    // Log warning but continue - don't fail recovery due to one bad entry
                    log::warn!(
                        "Failed to parse persisted session at key {:?}: {}",
                        ds.key,
                        e
                    );
                }
            }
        }

        Ok(sessions)
    }
}
