use bitcoin::opcodes::all;
use cln_plugin::Plugin;
use cln_rpc::primitives::{Amount, ShortChannelId};
use hex::FromHex;
use serde::{Deserialize, Deserializer};
use std::{collections::VecDeque, net::Incoming, sync::Arc, time::Duration};
use tokio::{
    sync::{oneshot, watch, Mutex},
    task::JoinHandle,
    time::sleep,
};

pub enum JitChannelRequestPhase {
    /// Collecting HTlCs until we have enough to justify opening a channel
    CollectingBuyIn {
        incoming_htlcs: Arc<Mutex<HtlcQueue>>,
        expected_payment_size_msat: Option<u64>,
    },

    /// Channel opening initiated, waiting for channel state "Ready"
    AwaitingChannelReady {
        waiting_htlcs: Arc<Mutex<HtlcQueue>>,
        channel_id: u64,
    },

    /// Channel is open, waiting for fee covering payment forward
    PendingFeePayment,

    /// Channel is open, collecting HTLCs to cover the fees
    CollectingFeePayment,

    /// JIT channel setup complete - operates as a normal channel
    Normal,

    TimedOut,
}

#[derive(Debug, Clone, PartialEq)]
enum JitHtlcAction {
    Wait,
    Continue,
    Resolve { payment_key: Vec<u8> },
    Fail { failure_message: Vec<u8> },
}

pub struct JitChannelCoordinator {
    jit_scid: u64,
    phase: Mutex<JitChannelRequestPhase>,
    timeout_handle: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for JitChannelCoordinator {
    fn drop(&mut self) {
        if let Some(handle) = self.timeout_handle.blocking_lock().take() {
            // Stops the timer once we dropped the coordinator.
            handle.abort(); // Is a no-op if the JoinHandle has already completed.
        }
    }
}

impl JitChannelCoordinator {
    pub fn new(
        jit_scid: u64,
        payment_size_msat: Option<u64>,
        timeout_duration: Duration,
    ) -> Arc<Self> {
        let coordinator = Arc::new(Self {
            jit_scid,
            phase: Mutex::new(JitChannelRequestPhase::CollectingBuyIn {
                incoming_htlcs: Arc::new(Mutex::new(HtlcQueue::new())),
                expected_payment_size_msat: payment_size_msat,
            }),
            timeout_handle: Mutex::new(None),
        });

        // Spawn background task to manage the timeout.
        let coordinator_clone = Arc::clone(&coordinator);
        let timeout_handle = tokio::spawn(async move {
            sleep(timeout_duration).await;
            let timed_out = coordinator_clone.handle_timeout().await;
            if timed_out {
                log::debug!(
                    "JIT channel coordinator for scid {} timed out",
                    coordinator_clone.jit_scid,
                );
            }
        });

        // Is save as we don't access from elsewhere during init.
        *coordinator.timeout_handle.try_lock().unwrap() = Some(timeout_handle);
        coordinator
    }

    pub async fn handle_timeout(&self) -> bool {
        let mut phase = self.phase.lock().await;

        // Only timeout if we are still collecting htlcs for the channel funding.
        let should_timeout = matches!(&*phase, JitChannelRequestPhase::CollectingBuyIn { .. });
        if should_timeout {
            let waiting_htlcs = match &*phase {
                JitChannelRequestPhase::CollectingBuyIn {
                    incoming_htlcs: waiting_htlcs,
                    ..
                } => Some(waiting_htlcs.clone()),
                _ => None,
            };
            // Atomic transition
            *phase = JitChannelRequestPhase::TimedOut;

            // Notify waiting htlcs
            if let Some(htlcs) = waiting_htlcs {
                // Check if this can lead to race conditions.
                let htlcs_in = htlcs.try_lock().unwrap().drain();
                htlcs_in.into_iter().for_each(|htlc| {
                    let _ = htlc.action_tx.send(JitHtlcAction::Fail {
                        failure_message: b"0x0700".to_vec(),
                    });
                });
            }
        }
        should_timeout
    }

    pub async fn htlc_in(&mut self, htlc: HtlcIn) -> Result<(), anyhow::Error> {
        let mut phase = self.phase.lock().await;

        match &mut *phase {
            JitChannelRequestPhase::CollectingBuyIn {
                incoming_htlcs,
                expected_payment_size_msat,
            } => {
                // MPP case:
                // 90s timer for all of this!
                //
                //

                // No-MPP case:
            }
            JitChannelRequestPhase::AwaitingChannelReady {
                waiting_htlcs,
                channel_id,
            } => todo!(),
            JitChannelRequestPhase::PendingFeePayment => {
                // append to queue and wait for signal
            }
            JitChannelRequestPhase::CollectingFeePayment => {
                // collect again and check that we get enough.
            }
            JitChannelRequestPhase::Normal => {
                // forward!
            }
            JitChannelRequestPhase::TimedOut => {
                // fail all incoming HTLCS
            }
        }

        todo!()
    }
}

#[derive(Debug)]
pub struct HtlcIn {
    id: u32,
    amount_msat: u64,
    action_tx: oneshot::Sender<JitHtlcAction>,
}

pub struct HtlcQueue {
    htlcs: VecDeque<HtlcIn>,
    total_amount_msat: u64,
}

impl HtlcQueue {
    pub fn new() -> Self {
        Self {
            htlcs: VecDeque::new(),
            total_amount_msat: 0,
        }
    }

    pub fn add_htlc(&mut self, htlc: HtlcIn) {
        self.total_amount_msat += htlc.amount_msat;
        self.htlcs.push_back(htlc);
    }

    pub fn total_amount_msat(&self) -> u64 {
        self.total_amount_msat
    }

    pub fn take_covering_amount(&mut self, required_msat: u64) -> Option<Vec<HtlcIn>> {
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

    pub fn drain(&mut self) -> Vec<HtlcIn> {
        self.total_amount_msat = 0;
        self.htlcs.drain(..).collect()
    }

    pub fn remove_htlcs(&mut self, htlc_ids: &[u32]) -> Vec<HtlcIn> {
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

/// Deserializes a lowercase hex string to a `Vec<u8>`.
pub fn from_hex<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;
    String::deserialize(deserializer)
        .and_then(|string| Vec::from_hex(string).map_err(|err| Error::custom(err.to_string())))
}

#[derive(Debug, Deserialize)]
#[allow(unused)]
struct Htlc {
    short_channel_id: ShortChannelId,
    id: u64,
    amount_msat: Amount,
    cltv_expiry: u32,
    cltv_expiry_relative: u16,
    #[serde(deserialize_with = "from_hex")]
    payment_hash: Vec<u8>,
    extra_tlvs: Vec<u8>,
}

#[derive(Debug, Deserialize)]
#[allow(unused)]
struct HtlcAcceptedRequest {
    htlc: Htlc,
    forward_to: String,
}

pub async fn jit_htlc_interceptor(
    htlc: Htlc,
    coord: Arc<JitChannelCoordinator>,
) -> Result<(), anyhow::Error> {
    todo!()
}

pub async fn on_hltc_accpted<S>(
    p: Plugin<S>,
    v: serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error>
where
    S: Clone + Send,
{
    let hook: HtlcAcceptedRequest = serde_json::from_value(v).unwrap();
    let htlc = hook.htlc;
    todo!();
}
