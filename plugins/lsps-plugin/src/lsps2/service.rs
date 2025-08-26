use anyhow::anyhow;
use async_trait::async_trait;
use bitcoin::hashes::Hash;
use chrono::Utc;
use cln_rpc::{
    model::{
        requests::{FundchannelRequest, ListpeerchannelsRequest},
        responses::{FundchannelResponse, ListpeerchannelsChannels, ListpeerchannelsResponse},
    },
    primitives::{Amount, AmountOrAll, ChannelState, PublicKey},
    ClnRpc, RpcError,
};
use std::{collections::VecDeque, fmt, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, oneshot, Mutex},
    task::JoinHandle,
    time::sleep,
};

use crate::{
    lsps0::primitives::Msat,
    lsps2::model::{compute_opening_fee, OpeningFeeParams},
};

pub mod failure_codes {
    pub const TEMPORARY_CHANNEL_FAILURE: &[u8] = &[0x07, 0x00];
    pub const UNKNOWN_NEXT_PEER: &[u8] = &[0x0a, 0x00];
}

#[async_trait]
pub trait LightningRpc: Send + Sync {
    async fn fund_channel(
        &self,
        request: FundchannelRequest,
    ) -> Result<FundchannelResponse, RpcError>;

    async fn list_peer_channels(
        &self,
        request: ListpeerchannelsRequest,
    ) -> Result<ListpeerchannelsResponse, RpcError>;
}

pub struct ClnRpcCall {
    pub rpc: Mutex<ClnRpc>,
}

impl ClnRpcCall {
    pub fn new(rpc: Mutex<ClnRpc>) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl LightningRpc for ClnRpcCall {
    async fn fund_channel(
        &self,
        request: FundchannelRequest,
    ) -> Result<FundchannelResponse, RpcError> {
        let mut rpc = self.rpc.lock().await;
        rpc.call_typed(&request).await
    }

    async fn list_peer_channels(
        &self,
        request: ListpeerchannelsRequest,
    ) -> Result<ListpeerchannelsResponse, RpcError> {
        let mut rpc = self.rpc.lock().await;
        rpc.call_typed(&request).await
    }
}

#[derive(Debug)]
enum JitChannelState {
    /// Collecting HTlCs until we have enough to justify opening a channel
    CollectingBuyIn {
        waiting_htlcs: Arc<Mutex<HtlcQueue>>,
    },

    /// Channel opening initiated, waiting for channel state "Ready"
    AwaitingChannelReady {
        waiting_htlcs: Arc<Mutex<HtlcQueue>>,
    },

    /// Channel is open, waiting for fee covering payment forward
    PendingFeePayment {
        waiting_htlcs: Arc<Mutex<HtlcQueue>>,
    },

    /// Channel is open, collecting HTLCs to cover the fees
    CollectingFeePayment {
        waiting_htlcs: Arc<Mutex<HtlcQueue>>,
    },

    /// JIT channel setup complete - operates as a normal channel
    NormalOperation,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JitHtlcAction {
    Wait,
    Continue {
        payload: Option<String>,
        forward_to: Option<Vec<u8>>,
        extra_tlvs: Option<String>,
        channel: Option<u64>,
    },
    Resolve {
        payment_key: Vec<u8>,
    },
    Fail {
        failure_message: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum JitChannelCoordinatorEvent {
    ChannelReady { channel_id: Vec<u8>, scid: u64 },
}

impl fmt::Display for JitChannelCoordinatorEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JitChannelCoordinatorEvent::ChannelReady { channel_id, .. } => {
                write!(f, "event_channel_ready {}", hex::encode(&channel_id))
            }
        }
    }
}

pub struct ChannelInfo {
    peer_id: PublicKey,
    opening_fee_params: OpeningFeeParams,
    htlc_minimum_msat: u64,
    funding_amount: Amount,
    expected_payment_size_msat: Option<Msat>,
}

impl ChannelInfo {
    pub fn new(
        peer_id: PublicKey,
        opening_fee_params: OpeningFeeParams,
        htlc_minimum_msat: u64,
        funding_amount: Amount,
        expected_payment_size_msat: Option<Msat>,
    ) -> Self {
        Self {
            peer_id,
            opening_fee_params,
            htlc_minimum_msat,
            funding_amount,
            expected_payment_size_msat,
        }
    }
}

pub struct JitChannelCoordinator<L: LightningRpc> {
    jit_scid: u64,
    channel_info: ChannelInfo,
    state: Mutex<JitChannelState>,
    rpc: Arc<L>,
    mpp_timeout: Duration,
    timer_handle: Mutex<Option<JoinHandle<()>>>,
    // timer_running: bool,
    event_tx: mpsc::UnboundedSender<JitChannelCoordinatorEvent>,
}

impl<L> Drop for JitChannelCoordinator<L>
where
    L: LightningRpc,
{
    fn drop(&mut self) {
        if let Ok(mut guard) = self.timer_handle.try_lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        } else {
            // This should never happen, but if it does, it's not too bad as the
            // timer will stop after mpp_timeout and only has a weak reference
            // to the parent struct. This means that it can't call any handlers
            // after the parent struct got dropped.
            log::warn!(
                "Jit channel coordinator for scid {} could not aquire timer lock on drop. Timer may be leaking",
                self.jit_scid,
            );
        }
    }
}

impl<L> JitChannelCoordinator<L>
where
    L: LightningRpc + 'static,
{
    pub fn new_arc(
        jit_scid: u64,
        channel_info: ChannelInfo,
        rpc: Arc<L>,
        mpp_timeout: Duration,
    ) -> Arc<Self> {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        let coordinator = Arc::new(Self {
            jit_scid,
            channel_info,
            state: Mutex::new(JitChannelState::CollectingBuyIn {
                waiting_htlcs: Arc::new(Mutex::new(HtlcQueue::new())),
            }),
            rpc,
            mpp_timeout,
            timer_handle: Mutex::new(None),
            event_tx,
        });

        // Spawn event-listener in background
        let coord_event_listener = Arc::downgrade(&coordinator);
        let scid_clone = jit_scid.clone();
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                log::debug!(
                    "JIT channel coordinator for {} received event {}",
                    &scid_clone,
                    event
                );
                if let Some(coord) = coord_event_listener.upgrade() {
                    coord.handle_event(event).await;
                }
            }
            log::debug!(
                "JIT channel coordinator for scid {} event channel closed",
                scid_clone,
            );
        });

        coordinator
    }

    pub async fn timer_is_running(&self) -> bool {
        let timer_lock = self.timer_handle.lock().await;
        if let Some(handle) = timer_lock.as_ref() {
            !handle.is_finished()
        } else {
            false
        }
    }

    async fn start_new_timer(self: &Arc<Self>, timeout: Duration) {
        let mut timer_lock = self.timer_handle.lock().await;
        if let Some(handle) = timer_lock.take() {
            handle.abort();
        }

        log::debug!(
            "JIT channel coordinator for scid {}, started a new timer",
            &self.jit_scid
        );

        let weak_coord = Arc::downgrade(&self);
        let jit_scid = self.jit_scid.clone();
        let timer_handle = tokio::spawn(async move {
            sleep(timeout).await;
            if let Some(coord) = weak_coord.upgrade() {
                let timed_out = coord.handle_timeout().await;
                if timed_out {
                    log::debug!("JIT channel coordinator for scid {} timed out", jit_scid,);
                }
            }
        });

        *timer_lock = Some(timer_handle);
    }

    #[allow(unused)] // In preparation of `MPP+fixed-invoice mode`.
    async fn maybe_start_new_mpp_timer(self: &Arc<Self>) {
        if !self.timer_is_running().await {
            self.start_new_timer(self.mpp_timeout).await
        }
    }

    async fn handle_event(&self, event: JitChannelCoordinatorEvent) {
        match event {
            JitChannelCoordinatorEvent::ChannelReady {
                channel_id,
                scid: channel,
            } => self.handle_channel_ready(channel_id, channel).await,
        }
    }

    async fn handle_timeout(&self) -> bool {
        let mut phase = self.state.lock().await;

        // Only timeout if we are still collecting htlcs for the channel funding.
        let should_timeout = matches!(&*phase, JitChannelState::CollectingBuyIn { .. });
        if should_timeout {
            let waiting_htlcs = match &*phase {
                JitChannelState::CollectingBuyIn { waiting_htlcs, .. } => {
                    Some(waiting_htlcs.clone())
                }
                _ => None,
            };
            // Atomic transition to same state with empty queue.
            *phase = JitChannelState::CollectingBuyIn {
                waiting_htlcs: Arc::new(Mutex::new(HtlcQueue::new())),
            };

            // Reject waiting htlcs.
            if let Some(htlcs) = waiting_htlcs {
                // Check if this can lead to race conditions.
                let htlcs_in = htlcs.try_lock().unwrap().drain();
                htlcs_in.into_iter().for_each(|htlc| {
                    let _ = htlc.action(JitHtlcAction::Fail {
                        failure_message: failure_codes::UNKNOWN_NEXT_PEER.to_vec(),
                    });
                });
            }
        }
        should_timeout
    }

    pub async fn new_htlc_in(
        self: &Arc<Self>,
        htlc: HtlcIn,
    ) -> Result<JitHtlcAction, anyhow::Error> {
        let mut phase = self.state.lock().await;

        match &mut *phase {
            JitChannelState::CollectingBuyIn { waiting_htlcs: _ } => {
                // FIXME: We skipp using the htlc queue in this state as we
                // initially only support `no-MPP+var-invoice` mode for testing.
                // `MPP+fixed-invoice` will be a quick follow-up.

                match self.channel_info.expected_payment_size_msat {
                    Some(_) => {
                        // MPP+fixed-invoice mode:
                        return Err(anyhow!("MPP payments are not supported yet"));
                    }
                    None => {
                        // no-MPP+var-invoice mode:
                        let now = Utc::now();
                        if now >= self.channel_info.opening_fee_params.valid_until {
                            // Opening fees not valid anymore, we need to fail the
                            // htlc with reason: temporary_channel_failure.
                            return Ok(JitHtlcAction::Fail {
                                failure_message: failure_codes::TEMPORARY_CHANNEL_FAILURE.to_vec(),
                            });
                        }

                        if htlc.amount_msat
                            < self
                                .channel_info
                                .opening_fee_params
                                .min_payment_size_msat
                                .msat()
                            || htlc.amount_msat
                                > self
                                    .channel_info
                                    .opening_fee_params
                                    .max_payment_size_msat
                                    .msat()
                        {
                            return Ok(JitHtlcAction::Fail {
                                failure_message: failure_codes::UNKNOWN_NEXT_PEER.to_vec(),
                            });
                        }

                        if let Some(opening_fee) = compute_opening_fee(
                            htlc.amount_msat,
                            self.channel_info
                                .opening_fee_params
                                .min_payment_size_msat
                                .msat(),
                            self.channel_info.opening_fee_params.proportional.ppm() as u64,
                        ) {
                            if htlc.amount_msat < opening_fee + self.channel_info.htlc_minimum_msat
                            {
                                // Payment amount is not enough to cover
                                // opening_fees + forward, need to fail with
                                // reason: unknown_next_peer.
                                return Ok(JitHtlcAction::Fail {
                                    failure_message: failure_codes::UNKNOWN_NEXT_PEER.to_vec(),
                                });
                            }

                            // We made it! Transition to channel opening!
                            let mut new_queue = HtlcQueue::new();
                            new_queue.add_htlc(htlc);
                            *phase = JitChannelState::AwaitingChannelReady {
                                waiting_htlcs: Arc::new(Mutex::new(new_queue)),
                            };

                            // Fund channel.
                            let funding_amount = self.channel_info.funding_amount.clone();
                            let peer_id = self.channel_info.peer_id.clone();
                            let event_tx = self.event_tx.clone();
                            let rpc = self.rpc.clone();
                            tokio::spawn(async move {
                                fund_channel_with_event(rpc, funding_amount, peer_id, event_tx)
                                    .await;
                            });

                            return Ok(JitHtlcAction::Wait);
                        } else {
                            // Computing opening fee overflowed, need to fail with
                            // reason: unknown_next_peer.
                            return Ok(JitHtlcAction::Fail {
                                failure_message: failure_codes::UNKNOWN_NEXT_PEER.to_vec(),
                            });
                        }
                    }
                }
            }
            JitChannelState::AwaitingChannelReady { waiting_htlcs } => {
                let mut waiting_htlcs = waiting_htlcs.lock().await;
                waiting_htlcs.add_htlc(htlc);
                return Ok(JitHtlcAction::Wait);
            }
            JitChannelState::PendingFeePayment { .. } => {
                return Err(anyhow!("not implemented yet"));
                // append to queue and wait for signal
            }
            JitChannelState::CollectingFeePayment { .. } => {
                return Err(anyhow!("not implemented yet"));
                // collect again and check that we get enough.
            }
            JitChannelState::NormalOperation => {
                return Ok(JitHtlcAction::Continue {
                    payload: None,
                    forward_to: None,
                    extra_tlvs: None,
                    channel: None,
                });
            }
        }
    }

    async fn handle_channel_ready(&self, channel_id: Vec<u8>, channel: u64) {
        let mut phase = self.state.lock().await;
        if let JitChannelState::AwaitingChannelReady { waiting_htlcs } = &*phase {
            let htlcs = waiting_htlcs.lock().await.drain();
            // FIXME: For MPP-Payments we want to send out the batch that covers
            // the opening_fee - ideally as one new htlc - while we hold the
            // remaining htlcs back in case the first batch gets rejected.

            // FIXME: Need to deduct Fee here!;
            htlcs.into_iter().for_each(|htlc| {
                _ = htlc.action(JitHtlcAction::Continue {
                    payload: None,
                    forward_to: Some(channel_id.clone()),
                    extra_tlvs: None,
                    channel: Some(channel),
                })
            });

            *phase = JitChannelState::PendingFeePayment {
                waiting_htlcs: Arc::new(Mutex::new(HtlcQueue::new())),
            };
        }
    }
}

async fn fund_channel_with_event<L: LightningRpc>(
    rpc: Arc<L>,
    funding_amount: Amount,
    peer_id: PublicKey,
    event_tx: mpsc::UnboundedSender<JitChannelCoordinatorEvent>,
) {
    log::debug!("funding new jit-channel for {}", &peer_id.to_string());
    let req = FundchannelRequest {
        announce: Some(false),
        close_to: None,
        compact_lease: None,
        feerate: None,
        minconf: None,
        mindepth: Some(0),
        push_msat: None,
        request_amt: None,
        reserve: None,
        channel_type: None, // FIXME: Set channel type to scid-alias
        utxos: None,
        amount: AmountOrAll::Amount(funding_amount),
        id: peer_id,
    };

    // FIXME: Dispatch event on error and add a handler that retries after
    // a couple of seconds.
    let res = rpc.fund_channel(req).await.unwrap();

    // FIXME: Persist tx-id to jump into state machine after restart,
    // (no htlcs as they will be replayed by core-lightning).
    let channel_id = res.channel_id.as_byte_array();
    let mut chan_is_ready = false;
    let mut cur_state = ChannelState::OPENINGD;
    while !chan_is_ready {
        let list_chan_req = ListpeerchannelsRequest {
            id: Some(peer_id),
            short_channel_id: None,
        };
        let chans = rpc.list_peer_channels(list_chan_req).await.unwrap();
        if let Some(chan) = chans
            .channels
            .into_iter()
            .filter(|c| match c.channel_id {
                Some(id) => id.as_byte_array() == channel_id,
                None => false,
            })
            .collect::<Vec<ListpeerchannelsChannels>>()
            .first()
        {
            cur_state = chan.state;
            if chan.state == ChannelState::CHANNELD_NORMAL {
                _ = event_tx.send(JitChannelCoordinatorEvent::ChannelReady {
                    channel_id: channel_id.clone().to_vec(),
                    scid: chan.short_channel_id.unwrap().to_u64(),
                });
                return;
            };
        };
        log::debug!(
            "JIT channel {} not ready yet, state is {:?}, try again in 10s...",
            hex::encode(channel_id),
            cur_state,
        );
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

#[derive(Debug)]
pub struct HtlcIn {
    id: u64,
    amount_msat: u64,
    action_tx: oneshot::Sender<JitHtlcAction>,
}

impl HtlcIn {
    pub fn new(id: u64, amount_msat: u64, action_tx: oneshot::Sender<JitHtlcAction>) -> Self {
        Self {
            id,
            amount_msat,
            action_tx,
        }
    }

    pub fn action(self, action: JitHtlcAction) {
        let _ = self.action_tx.send(action);
    }
}

#[derive(Debug)]
struct HtlcQueue {
    htlcs: VecDeque<HtlcIn>,
    total_amount_msat: u64,
}

impl HtlcQueue {
    fn new() -> Self {
        Self {
            htlcs: VecDeque::new(),
            total_amount_msat: 0,
        }
    }

    fn len(&self) -> usize {
        self.htlcs.len()
    }

    fn is_empty(&self) -> bool {
        self.htlcs.is_empty()
    }

    fn pop_back(&mut self) -> Option<HtlcIn> {
        match self.htlcs.pop_back() {
            Some(htlc) => {
                self.total_amount_msat -= htlc.amount_msat;
                Some(htlc)
            }
            None => None,
        }
    }

    fn add_htlc(&mut self, htlc: HtlcIn) {
        self.total_amount_msat += htlc.amount_msat;
        self.htlcs.push_back(htlc);
    }

    fn total_amount_msat(&self) -> u64 {
        self.total_amount_msat
    }

    fn take_covering_amount(&mut self, required_msat: u64) -> Option<Vec<HtlcIn>> {
        if self.total_amount_msat < required_msat {
            return None;
        }

        let mut taken = Vec::new();
        let mut accumulated = 0;

        while accumulated < required_msat && !self.htlcs.is_empty() {
            let htlc = self.htlcs.pop_front().unwrap();
            accumulated += htlc.amount_msat;
            self.total_amount_msat -= htlc.amount_msat;
            taken.push(htlc);
        }

        Some(taken)
    }

    fn drain(&mut self) -> Vec<HtlcIn> {
        self.total_amount_msat = 0;
        self.htlcs.drain(..).collect()
    }

    fn remove_htlcs(&mut self, htlc_ids: &[u64]) -> Vec<HtlcIn> {
        if htlc_ids.is_empty() {
            return Vec::new();
        }
        // Potential performance optimization but it might only make sense for
        // large slices. I'll leave it here for a future decission.
        // let ids_to_remove: HashSet<_> = htlc_ids.iter().copied().collect();
        let all_htlcs = self.drain();
        let mut removed = Vec::with_capacity(htlc_ids.len());
        let mut kept = VecDeque::new();
        self.total_amount_msat = 0;

        for htlc in all_htlcs {
            if htlc_ids.contains(&htlc.id) {
                removed.push(htlc);
            } else {
                self.total_amount_msat += htlc.amount_msat;
                kept.push_back(htlc);
            }
        }

        self.htlcs = kept;

        removed
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ops::Deref,
        sync::atomic::{AtomicU32, Ordering},
    };

    use crate::{
        lsps0::primitives::{Msat, Ppm},
        lsps2::model::Promise,
    };

    use super::*;
    use tokio::{
        sync::oneshot,
        time::{self, timeout},
    };

    const PUBKEY: [u8; 33] = [
        0x02, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87,
        0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16,
        0xf8, 0x17, 0x98,
    ];

    fn create_peer_id() -> PublicKey {
        PublicKey::from_slice(&PUBKEY).expect("Valid pubkey")
    }

    #[derive(Clone)]
    struct MockChannelRpc {
        fund_channel_response: Option<Result<FundchannelResponse, RpcError>>,
        fund_channel_call_count: Arc<AtomicU32>,
    }

    impl MockChannelRpc {
        fn new() -> Self {
            Self {
                fund_channel_response: None,
                fund_channel_call_count: Arc::new(AtomicU32::new(0)),
            }
        }

        fn with_fund_channel_response(
            mut self,
            response: Result<FundchannelResponse, RpcError>,
        ) -> Self {
            self.fund_channel_response = Some(response);
            self
        }

        fn fund_channel_call_count(&self) -> u32 {
            self.fund_channel_call_count.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl LightningRpc for MockChannelRpc {
        async fn fund_channel(
            &self,
            _request: FundchannelRequest,
        ) -> Result<FundchannelResponse, RpcError> {
            self.fund_channel_call_count.fetch_add(1, Ordering::Relaxed);
            self.fund_channel_response.clone().unwrap()
        }
        async fn list_peer_channels(
            &self,
            request: ListpeerchannelsRequest,
        ) -> Result<ListpeerchannelsResponse, RpcError> {
            todo!()
        }
    }

    fn create_test_channel_info() -> ChannelInfo {
        ChannelInfo {
            peer_id: create_peer_id(),
            opening_fee_params: OpeningFeeParams {
                min_payment_size_msat: Msat(1000),
                max_payment_size_msat: Msat(1000000),
                proportional: Ppm(1000), // 1000 ppm = 0.1%
                valid_until: Utc::now() + Duration::from_secs(3600),
                min_fee_msat: Msat(1000),
                min_lifetime: 100,
                max_client_to_self_delay: 144,
                promise: Promise::try_from("").unwrap(),
            },
            htlc_minimum_msat: 1000,
            funding_amount: Amount::from_sat(100000),
            expected_payment_size_msat: None, // "no-MPP+var-invoice"
        }
    }

    // Helper function to create test HTLCs
    fn create_test_htlc(id: u64, amount_msat: u64) -> (HtlcIn, oneshot::Receiver<JitHtlcAction>) {
        let (action_tx, action_rx) = oneshot::channel();
        (HtlcIn::new(id, amount_msat, action_tx), action_rx)
    }

    // Helper function to create multiple test HTLCs
    fn create_test_htlcs(amounts: &[(u64, u64)]) -> Vec<HtlcIn> {
        amounts
            .iter()
            .map(|(id, amount)| {
                let (htlc, _) = create_test_htlc(*id, *amount);
                htlc
            })
            .collect()
    }

    #[tokio::test]
    async fn test_htlc_amount_below_minimum() {
        let channel_info = create_test_channel_info();
        let mock_rpc = Arc::new(MockChannelRpc::new());

        let coordinator = JitChannelCoordinator::new_arc(
            12345,
            channel_info,
            mock_rpc.clone(),
            Duration::from_secs(10),
        );

        coordinator.start_new_timer(coordinator.mpp_timeout).await;
        assert!(coordinator.timer_is_running().await);

        let (htlc, _) = create_test_htlc(1, 500); // Below min_payment_size_msat

        let result = coordinator.new_htlc_in(htlc).await;
        assert!(result.is_ok());
        let action = result.unwrap();

        assert_eq!(
            action,
            JitHtlcAction::Fail {
                failure_message: failure_codes::UNKNOWN_NEXT_PEER.to_vec(),
            }
        );

        // Should not have called fund_channel RPC
        assert_eq!(mock_rpc.fund_channel_call_count(), 0);
    }

    #[tokio::test]
    async fn test_htlc_amount_above_maximum() {
        let channel_info = create_test_channel_info();
        let mock_rpc = Arc::new(MockChannelRpc::new());

        let coordinator = JitChannelCoordinator::new_arc(
            12345,
            channel_info,
            mock_rpc.clone(),
            Duration::from_secs(10),
        );

        let (htlc, _) = create_test_htlc(1, 1500000); // Above max_payment_size_msat

        let result = coordinator.new_htlc_in(htlc).await;
        assert!(result.is_ok());
        let action = result.unwrap();

        assert_eq!(
            action,
            JitHtlcAction::Fail {
                failure_message: failure_codes::UNKNOWN_NEXT_PEER.to_vec(),
            }
        );

        // Should not have called RPC
        assert_eq!(mock_rpc.fund_channel_call_count(), 0);
    }

    #[tokio::test]
    async fn test_expired_opening_fees() {
        let mut channel_info = create_test_channel_info();
        channel_info.opening_fee_params.valid_until = Utc::now() - Duration::from_secs(60);

        let mock_rpc = Arc::new(MockChannelRpc::new());

        let coordinator = JitChannelCoordinator::new_arc(
            12345,
            channel_info,
            mock_rpc.clone(),
            Duration::from_secs(10),
        );

        let (htlc, _) = create_test_htlc(1, 10000);

        let result = coordinator.new_htlc_in(htlc).await;
        assert!(result.is_ok());
        let action = result.unwrap();

        assert_eq!(
            action,
            JitHtlcAction::Fail {
                failure_message: failure_codes::TEMPORARY_CHANNEL_FAILURE.to_vec(),
            }
        );

        // Should not have called RPC
        assert_eq!(mock_rpc.fund_channel_call_count(), 0);
    }

    #[tokio::test]
    async fn test_insufficient_amount_for_opening_fee() {
        let channel_info = create_test_channel_info();
        let mock_rpc = Arc::new(MockChannelRpc::new());

        let coordinator = JitChannelCoordinator::new_arc(
            12345,
            channel_info,
            mock_rpc.clone(),
            Duration::from_secs(10),
        );

        // Amount that's in range but insufficient to cover opening fee + htlc_minimum
        let (htlc, _) = create_test_htlc(1, 1500);

        let result = coordinator.new_htlc_in(htlc).await;
        assert!(result.is_ok());
        let action = result.unwrap();

        assert_eq!(
            action,
            JitHtlcAction::Fail {
                failure_message: failure_codes::UNKNOWN_NEXT_PEER.to_vec(),
            }
        );

        // Should not have called RPC
        assert_eq!(mock_rpc.fund_channel_call_count(), 0);
    }

    #[tokio::test]
    async fn test_successful_channel_opening() {
        let channel_info = create_test_channel_info();
        let mock_rpc = Arc::new(MockChannelRpc::new().with_fund_channel_response(Ok(
            FundchannelResponse {
                channel_type: None,
                close_to: None,
                mindepth: None,
                channel_id: Hash::all_zeros(),
                outnum: 0,
                tx: String::new(),
                txid: String::new(),
            },
        )));

        let coordinator = JitChannelCoordinator::new_arc(
            12345,
            channel_info,
            mock_rpc.clone(),
            Duration::from_secs(10),
        );

        let (htlc, _) = create_test_htlc(1, 50000); // Sufficient amount

        let result = coordinator.new_htlc_in(htlc).await;
        assert!(result.is_ok());
        let action = result.unwrap();
        assert_eq!(action, JitHtlcAction::Wait);

        // Wait for funding to complete and channel to become ready
        time::sleep(Duration::from_millis(200)).await;

        // Should have called RPC exactly once
        assert_eq!(mock_rpc.fund_channel_call_count(), 1);

        // Check state transition
        let state = coordinator.state.lock().await;
        println!("{:?}", state);
        assert!(matches!(
            state.deref(),
            JitChannelState::PendingFeePayment { .. }
        ));
    }

    #[test]
    fn test_queue_add_htlcs() {
        let mut queue = HtlcQueue::new();
        let htlcs = create_test_htlcs(&[(1, 1000), (2, 2000), (3, 1500)]);

        for htlc in htlcs {
            queue.add_htlc(htlc);
        }

        assert_eq!(queue.len(), 3);
        assert_eq!(queue.total_amount_msat(), 4500);
    }

    #[test]
    fn test_queue_drain_all_htlcs() {
        let mut queue = HtlcQueue::new();
        let htlcs = create_test_htlcs(&[(1, 1000), (2, 2000), (3, 1500)]);

        for htlc in htlcs {
            queue.add_htlc(htlc);
        }

        let drained = queue.drain();

        // Queue should be empty after drain
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.total_amount_msat(), 0);
        assert!(queue.is_empty());

        // Drained HTLCs should match what we added
        assert_eq!(drained.len(), 3);
        assert_eq!(drained[0].id, 1);
        assert_eq!(drained[0].amount_msat, 1000);
        assert_eq!(drained[1].id, 2);
        assert_eq!(drained[1].amount_msat, 2000);
        assert_eq!(drained[2].id, 3);
        assert_eq!(drained[2].amount_msat, 1500);
    }

    #[test]
    fn test_queue_drain_empty_queue() {
        let mut queue = HtlcQueue::new();
        let drained = queue.drain();

        assert_eq!(drained.len(), 0);
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.total_amount_msat(), 0);
    }

    #[test]
    fn test_queue_take_covering_amount_exact() {
        let mut queue = HtlcQueue::new();
        let htlcs = create_test_htlcs(&[(1, 1000), (2, 2000), (3, 1500)]);

        for htlc in htlcs {
            queue.add_htlc(htlc);
        }

        // Take exactly 3000 msat (first two HTLCs)
        let taken = queue.take_covering_amount(3000).unwrap();

        assert_eq!(taken.len(), 2);
        assert_eq!(taken[0].id, 1);
        assert_eq!(taken[1].id, 2);

        // Queue should have remaining HTLC
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.total_amount_msat(), 1500);
    }

    #[test]
    fn test_queue_take_covering_amount_overpay() {
        let mut queue = HtlcQueue::new();
        let htlcs = create_test_htlcs(&[(1, 1000), (2, 2000)]);

        for htlc in htlcs {
            queue.add_htlc(htlc);
        }

        // Take 2500 msat (need both HTLCs, total 3000)
        let taken = queue.take_covering_amount(2500).unwrap();

        assert_eq!(taken.len(), 2);
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.total_amount_msat(), 0);
    }

    #[test]
    fn test_queue_take_covering_amount_insufficient() {
        let mut queue = HtlcQueue::new();
        let htlcs = create_test_htlcs(&[(1, 1000), (2, 2000)]);

        for htlc in htlcs {
            queue.add_htlc(htlc);
        }

        // Try to take 5000 msat (more than available 3000)
        let taken = queue.take_covering_amount(5000);

        assert!(taken.is_none());

        // Queue should be unchanged
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.total_amount_msat(), 3000);
    }

    #[test]
    fn test_queue_take_covering_amount_zero() {
        let mut queue = HtlcQueue::new();
        let htlcs = create_test_htlcs(&[(1, 1000), (2, 2000)]);

        for htlc in htlcs {
            queue.add_htlc(htlc);
        }

        // Take 0 msat
        let taken = queue.take_covering_amount(0).unwrap();

        assert_eq!(taken.len(), 0);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.total_amount_msat(), 3000);
    }

    #[test]
    fn test_remove_htlcs() {
        let mut queue = HtlcQueue::new();
        let htlcs = create_test_htlcs(&[(1, 1000), (2, 2000), (3, 1500), (4, 3000)]);

        for htlc in htlcs {
            queue.add_htlc(htlc);
        }

        // Remove HTLCs with ids 1 and 3
        let removed = queue.remove_htlcs(&[1, 3]);

        assert_eq!(removed.len(), 2);

        // Check removed HTLCs (order might vary)
        let removed_ids: std::collections::HashSet<u64> = removed.iter().map(|h| h.id).collect();
        assert!(removed_ids.contains(&1));
        assert!(removed_ids.contains(&3));

        // Queue should have remaining HTLCs
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.total_amount_msat(), 5000); // 2000 + 3000
    }

    #[test]
    fn test_remove_htlcs_nonexistent() {
        let mut queue = HtlcQueue::new();
        let htlcs = create_test_htlcs(&[(1, 1000), (2, 2000)]);

        for htlc in htlcs {
            queue.add_htlc(htlc);
        }

        // Try to remove non-existent HTLC
        let removed = queue.remove_htlcs(&[99]);

        assert_eq!(removed.len(), 0);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.total_amount_msat(), 3000);
    }

    #[test]
    fn test_remove_htlcs_empty_list() {
        let mut queue = HtlcQueue::new();
        let htlcs = create_test_htlcs(&[(1, 1000), (2, 2000)]);

        for htlc in htlcs {
            queue.add_htlc(htlc);
        }

        // Remove empty list
        let removed = queue.remove_htlcs(&[]);

        assert_eq!(removed.len(), 0);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.total_amount_msat(), 3000);
    }

    #[test]
    fn test_remove_all_htlcs() {
        let mut queue = HtlcQueue::new();
        let htlcs = create_test_htlcs(&[(1, 1000), (2, 2000), (3, 1500)]);

        for htlc in htlcs {
            queue.add_htlc(htlc);
        }

        // Remove all HTLCs
        let removed = queue.remove_htlcs(&[1, 2, 3]);

        assert_eq!(removed.len(), 3);
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.total_amount_msat(), 0);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_fifo_ordering() {
        let mut queue = HtlcQueue::new();

        // Add HTLCs in specific order
        let (htlc1, _) = create_test_htlc(10, 1000);
        let (htlc2, _) = create_test_htlc(20, 2000);
        let (htlc3, _) = create_test_htlc(30, 3000);
        queue.add_htlc(htlc1);
        queue.add_htlc(htlc2);
        queue.add_htlc(htlc3);

        // Take first HTLC
        let taken = queue.take_covering_amount(500).unwrap();

        // Should get first HTLC (FIFO order)
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].id, 10);

        // Drain remaining and check order
        let remaining = queue.drain();
        assert_eq!(remaining[0].id, 20);
        assert_eq!(remaining[1].id, 30);
    }

    #[tokio::test]
    async fn test_htlc_action_tx_can_be_used() {
        let mut queue = HtlcQueue::new();
        let (action_tx, action_rx) = oneshot::channel();

        let htlc = HtlcIn {
            id: 1,
            amount_msat: 1000,
            action_tx,
        };

        queue.add_htlc(htlc);

        // Drain the HTLC and send action
        let drained = queue.drain();
        assert_eq!(drained.len(), 1);

        let htlc = drained.into_iter().next().unwrap();
        htlc.action(JitHtlcAction::Wait);

        // Receiver should get the action
        let received_action = action_rx.await.unwrap();
        assert_eq!(received_action, JitHtlcAction::Wait);
    }

    #[test]
    fn test_queue_consistency_after_operations() {
        let mut queue = HtlcQueue::new();

        // Add some HTLCs
        for i in 1..=5 {
            let (htlc, _) = create_test_htlc(i, i as u64 * 1000);
            queue.add_htlc(htlc);
        }

        assert_eq!(queue.len(), 5);
        assert_eq!(queue.total_amount_msat(), 15000); // 1+2+3+4+5 = 15

        // Remove some HTLCs
        let removed = queue.remove_htlcs(&[2, 4]);
        assert_eq!(removed.len(), 2);
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.total_amount_msat(), 9000); // 15000 - 2000 - 4000

        // Take covering amount
        let taken = queue.take_covering_amount(4000);
        assert!(taken.is_some());
        let taken = taken.unwrap();

        // Should take HTLC 1 (1000) and HTLC 3 (3000) = 4000 total
        assert_eq!(taken.len(), 2);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.total_amount_msat(), 5000); // Only HTLC 5 left

        // Drain remaining
        let remaining = queue.drain();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, 5);
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.total_amount_msat(), 0);
    }

    #[test]
    fn test_remove_htlcs_with_duplicates() {
        let mut queue = HtlcQueue::new();

        // Add HTLCs
        for i in 1..=10 {
            let (htlc, _) = create_test_htlc(i, i as u64 * 100);
            queue.add_htlc(htlc);
        }

        // Remove with duplicate IDs in the list (should handle gracefully)
        let removed = queue.remove_htlcs(&[2, 4, 2, 6, 4]);

        // Should only remove unique HTLCs
        assert_eq!(removed.len(), 3); // HTLCs 2, 4, 6
        assert_eq!(queue.len(), 7);

        let removed_ids: std::collections::HashSet<u64> = removed.iter().map(|h| h.id).collect();
        assert!(removed_ids.contains(&2));
        assert!(removed_ids.contains(&4));
        assert!(removed_ids.contains(&6));
    }
}
