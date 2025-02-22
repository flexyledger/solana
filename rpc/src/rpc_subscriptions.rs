//! The `pubsub` module implements a threaded subscription service on client RPC request

use {
    crate::{
        optimistically_confirmed_bank_tracker::OptimisticallyConfirmedBank,
        parsed_token_accounts::{get_parsed_token_account, get_parsed_token_accounts},
        rpc_pubsub_service::PubSubConfig,
        rpc_subscription_tracker::{
            AccountSubscriptionParams, LogsSubscriptionKind, LogsSubscriptionParams,
            ProgramSubscriptionParams, SignatureSubscriptionParams, SubscriptionControl,
            SubscriptionId, SubscriptionInfo, SubscriptionParams, SubscriptionsTracker,
        },
    },
    crossbeam_channel::{Receiver, RecvTimeoutError, SendError, Sender},
    serde::Serialize,
    solana_account_decoder::{parse_token::spl_token_id_v2_0, UiAccount, UiAccountEncoding},
    solana_client::{
        rpc_filter::RpcFilterType,
        rpc_response::{
            ProcessedSignatureResult, ReceivedSignatureResult, Response, RpcKeyedAccount,
            RpcLogsResponse, RpcResponseContext, RpcSignatureResult, SlotInfo, SlotUpdate,
        },
    },
    solana_measure::measure::Measure,
    solana_runtime::{
        bank::{Bank, TransactionLogInfo},
        bank_forks::BankForks,
        commitment::{BlockCommitmentCache, CommitmentSlots},
    },
    solana_sdk::{
        account::{AccountSharedData, ReadableAccount},
        clock::{Slot, UnixTimestamp},
        pubkey::Pubkey,
        signature::Signature,
        timing::timestamp,
        transaction,
    },
    solana_vote_program::vote_state::Vote,
    std::{
        collections::{HashMap, VecDeque},
        io::Cursor,
        iter, str,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock, Weak,
        },
        thread::{Builder, JoinHandle},
        time::Duration,
    },
    tokio::sync::broadcast,
};

const RECEIVE_DELAY_MILLIS: u64 = 100;

fn get_transaction_logs(
    bank: &Bank,
    params: &LogsSubscriptionParams,
) -> Option<Vec<TransactionLogInfo>> {
    let pubkey = match &params.kind {
        LogsSubscriptionKind::All | LogsSubscriptionKind::AllWithVotes => None,
        LogsSubscriptionKind::Single(pubkey) => Some(pubkey),
    };
    let mut logs = bank.get_transaction_logs(pubkey);
    if matches!(params.kind, LogsSubscriptionKind::All) {
        // Filter out votes if the subscriber doesn't want them
        if let Some(logs) = &mut logs {
            logs.retain(|log| !log.is_vote);
        }
    }
    logs
}

// A more human-friendly version of Vote, with the bank state signature base58 encoded.
#[derive(Serialize, Deserialize, Debug)]
pub struct RpcVote {
    pub slots: Vec<Slot>,
    pub hash: String,
    pub timestamp: Option<UnixTimestamp>,
}

pub enum NotificationEntry {
    Slot(SlotInfo),
    SlotUpdate(SlotUpdate),
    Vote(Vote),
    Root(Slot),
    Bank(CommitmentSlots),
    Gossip(Slot),
    SignaturesReceived((Slot, Vec<Signature>)),
    Subscribed(SubscriptionParams, SubscriptionId),
    Unsubscribed(SubscriptionParams, SubscriptionId),
}

impl std::fmt::Debug for NotificationEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            NotificationEntry::Root(root) => write!(f, "Root({})", root),
            NotificationEntry::Vote(vote) => write!(f, "Vote({:?})", vote),
            NotificationEntry::Slot(slot_info) => write!(f, "Slot({:?})", slot_info),
            NotificationEntry::SlotUpdate(slot_update) => {
                write!(f, "SlotUpdate({:?})", slot_update)
            }
            NotificationEntry::Bank(commitment_slots) => {
                write!(f, "Bank({{slot: {:?}}})", commitment_slots.slot)
            }
            NotificationEntry::SignaturesReceived(slot_signatures) => {
                write!(f, "SignaturesReceived({:?})", slot_signatures)
            }
            NotificationEntry::Gossip(slot) => write!(f, "Gossip({:?})", slot),
            NotificationEntry::Subscribed(params, id) => {
                write!(f, "Subscribed({:?}, {:?})", params, id)
            }
            NotificationEntry::Unsubscribed(params, id) => {
                write!(f, "Unsubscribed({:?}, {:?})", params, id)
            }
        }
    }
}

#[allow(clippy::type_complexity)]
fn check_commitment_and_notify<P, S, B, F, X>(
    params: &P,
    subscription: &SubscriptionInfo,
    bank_forks: &Arc<RwLock<BankForks>>,
    commitment_slots: &CommitmentSlots,
    bank_method: B,
    filter_results: F,
    notifier: &mut RpcNotifier,
    is_final: bool,
) -> bool
where
    S: Clone + Serialize,
    B: Fn(&Bank, &P) -> X,
    F: Fn(X, &P, Slot, Arc<Bank>) -> (Box<dyn Iterator<Item = S>>, Slot),
    X: Clone + Default,
{
    let commitment = if let Some(commitment) = subscription.commitment() {
        commitment
    } else {
        error!("missing commitment in check_commitment_and_notify");
        return false;
    };
    let slot = if commitment.is_finalized() {
        commitment_slots.highest_confirmed_root
    } else if commitment.is_confirmed() {
        commitment_slots.highest_confirmed_slot
    } else {
        commitment_slots.slot
    };

    let mut notified = false;
    if let Some(bank) = bank_forks.read().unwrap().get(slot).cloned() {
        let results = bank_method(&bank, params);
        let mut w_last_notified_slot = subscription.last_notified_slot.write().unwrap();
        let (filter_results, result_slot) =
            filter_results(results, params, *w_last_notified_slot, bank);
        for result in filter_results {
            notifier.notify(
                Response {
                    context: RpcResponseContext { slot },
                    value: result,
                },
                subscription,
                is_final,
            );
            *w_last_notified_slot = result_slot;
            notified = true;
        }
    }
    notified
}

#[derive(Debug, Clone)]
pub struct RpcNotification {
    pub subscription_id: SubscriptionId,
    pub is_final: bool,
    pub json: Weak<String>,
}

struct RecentItems {
    queue: VecDeque<Arc<String>>,
    total_bytes: usize,
    max_len: usize,
    max_total_bytes: usize,
}

impl RecentItems {
    fn new(max_len: usize, max_total_bytes: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            total_bytes: 0,
            max_len,
            max_total_bytes,
        }
    }

    fn push(&mut self, item: Arc<String>) {
        self.total_bytes = self
            .total_bytes
            .checked_add(item.len())
            .expect("total bytes overflow");
        self.queue.push_back(item);

        while self.total_bytes > self.max_total_bytes || self.queue.len() > self.max_len {
            let item = self.queue.pop_front().expect("can't be empty");
            self.total_bytes = self
                .total_bytes
                .checked_sub(item.len())
                .expect("total bytes underflow");
        }

        datapoint_info!(
            "rpc_subscriptions_recent_items",
            ("num", self.queue.len(), i64),
            ("total_bytes", self.total_bytes, i64),
        );
    }
}

struct RpcNotifier {
    sender: broadcast::Sender<RpcNotification>,
    buf: Vec<u8>,
    recent_items: RecentItems,
}

#[derive(Debug, Serialize)]
struct NotificationParams<T> {
    result: T,
    subscription: SubscriptionId,
}

#[derive(Debug, Serialize)]
struct Notification<T> {
    jsonrpc: Option<jsonrpc_core::Version>,
    method: &'static str,
    params: NotificationParams<T>,
}

impl RpcNotifier {
    fn notify<T>(&mut self, value: T, subscription: &SubscriptionInfo, is_final: bool)
    where
        T: serde::Serialize,
    {
        self.buf.clear();
        let notification = Notification {
            jsonrpc: Some(jsonrpc_core::Version::V2),
            method: subscription.method(),
            params: NotificationParams {
                result: value,
                subscription: subscription.id(),
            },
        };
        serde_json::to_writer(Cursor::new(&mut self.buf), &notification)
            .expect("serialization never fails");
        let buf_str = str::from_utf8(&self.buf).expect("json is always utf-8");
        let buf_arc = Arc::new(String::from(buf_str));

        let notification = RpcNotification {
            subscription_id: subscription.id(),
            json: Arc::downgrade(&buf_arc),
            is_final,
        };
        // There is an unlikely case where this can fail: if the last subscription is closed
        // just as the notifier generates a notification for it.
        let _ = self.sender.send(notification);

        inc_new_counter_info!("rpc-pubsub-messages", 1);
        inc_new_counter_info!("rpc-pubsub-bytes", buf_arc.len());

        self.recent_items.push(buf_arc);
    }
}

fn filter_account_result(
    result: Option<(AccountSharedData, Slot)>,
    params: &AccountSubscriptionParams,
    last_notified_slot: Slot,
    bank: Arc<Bank>,
) -> (Box<dyn Iterator<Item = UiAccount>>, Slot) {
    // If the account is not found, `last_modified_slot` will default to zero and
    // we will notify clients that the account no longer exists if we haven't already
    let (account, last_modified_slot) = result.unwrap_or_default();

    // If last_modified_slot < last_notified_slot this means that we last notified for a fork
    // and should notify that the account state has been reverted.
    let results: Box<dyn Iterator<Item = UiAccount>> = if last_modified_slot != last_notified_slot {
        if account.owner() == &spl_token_id_v2_0()
            && params.encoding == UiAccountEncoding::JsonParsed
        {
            Box::new(iter::once(get_parsed_token_account(
                bank,
                &params.pubkey,
                account,
            )))
        } else {
            Box::new(iter::once(UiAccount::encode(
                &params.pubkey,
                &account,
                params.encoding,
                None,
                None,
            )))
        }
    } else {
        Box::new(iter::empty())
    };

    (results, last_modified_slot)
}

fn filter_signature_result(
    result: Option<transaction::Result<()>>,
    _params: &SignatureSubscriptionParams,
    last_notified_slot: Slot,
    _bank: Arc<Bank>,
) -> (Box<dyn Iterator<Item = RpcSignatureResult>>, Slot) {
    (
        Box::new(result.into_iter().map(|result| {
            RpcSignatureResult::ProcessedSignature(ProcessedSignatureResult { err: result.err() })
        })),
        last_notified_slot,
    )
}

fn filter_program_results(
    accounts: Vec<(Pubkey, AccountSharedData)>,
    params: &ProgramSubscriptionParams,
    last_notified_slot: Slot,
    bank: Arc<Bank>,
) -> (Box<dyn Iterator<Item = RpcKeyedAccount>>, Slot) {
    let accounts_is_empty = accounts.is_empty();
    let encoding = params.encoding;
    let filters = params.filters.clone();
    let keyed_accounts = accounts.into_iter().filter(move |(_, account)| {
        filters.iter().all(|filter_type| match filter_type {
            RpcFilterType::DataSize(size) => account.data().len() as u64 == *size,
            RpcFilterType::Memcmp(compare) => compare.bytes_match(account.data()),
        })
    });
    let accounts: Box<dyn Iterator<Item = RpcKeyedAccount>> = if params.pubkey
        == spl_token_id_v2_0()
        && params.encoding == UiAccountEncoding::JsonParsed
        && !accounts_is_empty
    {
        Box::new(get_parsed_token_accounts(bank, keyed_accounts))
    } else {
        Box::new(
            keyed_accounts.map(move |(pubkey, account)| RpcKeyedAccount {
                pubkey: pubkey.to_string(),
                account: UiAccount::encode(&pubkey, &account, encoding, None, None),
            }),
        )
    };
    (accounts, last_notified_slot)
}

fn filter_logs_results(
    logs: Option<Vec<TransactionLogInfo>>,
    _params: &LogsSubscriptionParams,
    last_notified_slot: Slot,
    _bank: Arc<Bank>,
) -> (Box<dyn Iterator<Item = RpcLogsResponse>>, Slot) {
    match logs {
        None => (Box::new(iter::empty()), last_notified_slot),
        Some(logs) => (
            Box::new(logs.into_iter().map(|log| RpcLogsResponse {
                signature: log.signature.to_string(),
                err: log.result.err(),
                logs: log.log_messages,
            })),
            last_notified_slot,
        ),
    }
}

fn initial_last_notified_slot(
    params: &SubscriptionParams,
    bank_forks: &RwLock<BankForks>,
    block_commitment_cache: &RwLock<BlockCommitmentCache>,
    optimistically_confirmed_bank: &RwLock<OptimisticallyConfirmedBank>,
) -> Slot {
    match params {
        SubscriptionParams::Account(params) => {
            let slot = if params.commitment.is_finalized() {
                block_commitment_cache
                    .read()
                    .unwrap()
                    .highest_confirmed_root()
            } else if params.commitment.is_confirmed() {
                optimistically_confirmed_bank.read().unwrap().bank.slot()
            } else {
                block_commitment_cache.read().unwrap().slot()
            };

            if let Some((_account, slot)) = bank_forks
                .read()
                .unwrap()
                .get(slot)
                .and_then(|bank| bank.get_account_modified_slot(&params.pubkey))
            {
                slot
            } else {
                0
            }
        }
        // last_notified_slot is not utilized for these subscriptions
        SubscriptionParams::Logs(_)
        | SubscriptionParams::Program(_)
        | SubscriptionParams::Signature(_)
        | SubscriptionParams::Slot
        | SubscriptionParams::SlotsUpdates
        | SubscriptionParams::Root
        | SubscriptionParams::Vote => 0,
    }
}

pub struct RpcSubscriptions {
    notification_sender: Sender<NotificationEntry>,

    t_cleanup: Option<JoinHandle<()>>,

    exit: Arc<AtomicBool>,
    control: SubscriptionControl,
}

impl Drop for RpcSubscriptions {
    fn drop(&mut self) {
        self.shutdown().unwrap_or_else(|err| {
            warn!("RPC Notification - shutdown error: {:?}", err);
        });
    }
}

impl RpcSubscriptions {
    pub fn new(
        exit: &Arc<AtomicBool>,
        bank_forks: Arc<RwLock<BankForks>>,
        block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
        optimistically_confirmed_bank: Arc<RwLock<OptimisticallyConfirmedBank>>,
    ) -> Self {
        Self::new_with_config(
            exit,
            bank_forks,
            block_commitment_cache,
            optimistically_confirmed_bank,
            &PubSubConfig::default(),
        )
    }

    pub fn new_for_tests(
        exit: &Arc<AtomicBool>,
        bank_forks: Arc<RwLock<BankForks>>,
        block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
        optimistically_confirmed_bank: Arc<RwLock<OptimisticallyConfirmedBank>>,
    ) -> Self {
        Self::new_with_config(
            exit,
            bank_forks,
            block_commitment_cache,
            optimistically_confirmed_bank,
            &PubSubConfig::default_for_tests(),
        )
    }

    pub fn new_with_config(
        exit: &Arc<AtomicBool>,
        bank_forks: Arc<RwLock<BankForks>>,
        block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
        optimistically_confirmed_bank: Arc<RwLock<OptimisticallyConfirmedBank>>,
        config: &PubSubConfig,
    ) -> Self {
        let (notification_sender, notification_receiver) = crossbeam_channel::unbounded();

        let exit_clone = exit.clone();
        let subscriptions = SubscriptionsTracker::new(bank_forks.clone());

        let (broadcast_sender, _) = broadcast::channel(config.queue_capacity_items);

        let notifier = RpcNotifier {
            sender: broadcast_sender.clone(),
            buf: Vec::new(),
            recent_items: RecentItems::new(
                config.queue_capacity_items,
                config.queue_capacity_bytes,
            ),
        };
        let t_cleanup = Builder::new()
            .name("solana-rpc-notifications".to_string())
            .spawn(move || {
                Self::process_notifications(
                    exit_clone,
                    notifier,
                    notification_receiver,
                    subscriptions,
                    bank_forks,
                    block_commitment_cache,
                    optimistically_confirmed_bank,
                );
            })
            .unwrap();

        let control = SubscriptionControl::new(
            config.max_active_subscriptions,
            notification_sender.clone(),
            broadcast_sender,
        );

        Self {
            notification_sender,
            t_cleanup: Some(t_cleanup),

            exit: exit.clone(),
            control,
        }
    }

    // For tests only...
    pub fn default_with_bank_forks(bank_forks: Arc<RwLock<BankForks>>) -> Self {
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        Self::new(
            &Arc::new(AtomicBool::new(false)),
            bank_forks,
            Arc::new(RwLock::new(BlockCommitmentCache::default())),
            optimistically_confirmed_bank,
        )
    }

    pub fn control(&self) -> &SubscriptionControl {
        &self.control
    }

    /// Notify subscribers of changes to any accounts or new signatures since
    /// the bank's last checkpoint.
    pub fn notify_subscribers(&self, commitment_slots: CommitmentSlots) {
        self.enqueue_notification(NotificationEntry::Bank(commitment_slots));
    }

    /// Notify Confirmed commitment-level subscribers of changes to any accounts or new
    /// signatures.
    pub fn notify_gossip_subscribers(&self, slot: Slot) {
        self.enqueue_notification(NotificationEntry::Gossip(slot));
    }

    pub fn notify_slot_update(&self, slot_update: SlotUpdate) {
        self.enqueue_notification(NotificationEntry::SlotUpdate(slot_update));
    }

    pub fn notify_slot(&self, slot: Slot, parent: Slot, root: Slot) {
        self.enqueue_notification(NotificationEntry::Slot(SlotInfo { slot, parent, root }));
        self.enqueue_notification(NotificationEntry::SlotUpdate(SlotUpdate::CreatedBank {
            slot,
            parent,
            timestamp: timestamp(),
        }));
    }

    pub fn notify_signatures_received(&self, slot_signatures: (Slot, Vec<Signature>)) {
        self.enqueue_notification(NotificationEntry::SignaturesReceived(slot_signatures));
    }

    pub fn notify_vote(&self, vote: &Vote) {
        self.enqueue_notification(NotificationEntry::Vote(vote.clone()));
    }

    pub fn notify_roots(&self, mut rooted_slots: Vec<Slot>) {
        rooted_slots.sort_unstable();
        rooted_slots.into_iter().for_each(|root| {
            self.enqueue_notification(NotificationEntry::SlotUpdate(SlotUpdate::Root {
                slot: root,
                timestamp: timestamp(),
            }));
            self.enqueue_notification(NotificationEntry::Root(root));
        });
    }

    fn enqueue_notification(&self, notification_entry: NotificationEntry) {
        match self.notification_sender.send(notification_entry) {
            Ok(()) => (),
            Err(SendError(notification)) => {
                warn!(
                    "Dropped RPC Notification - receiver disconnected : {:?}",
                    notification
                );
            }
        }
    }

    fn process_notifications(
        exit: Arc<AtomicBool>,
        mut notifier: RpcNotifier,
        notification_receiver: Receiver<NotificationEntry>,
        mut subscriptions: SubscriptionsTracker,
        bank_forks: Arc<RwLock<BankForks>>,
        block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
        optimistically_confirmed_bank: Arc<RwLock<OptimisticallyConfirmedBank>>,
    ) {
        loop {
            if exit.load(Ordering::Relaxed) {
                break;
            }
            match notification_receiver.recv_timeout(Duration::from_millis(RECEIVE_DELAY_MILLIS)) {
                Ok(notification_entry) => {
                    match notification_entry {
                        NotificationEntry::Subscribed(params, id) => {
                            subscriptions.subscribe(params.clone(), id, || {
                                initial_last_notified_slot(
                                    &params,
                                    &bank_forks,
                                    &block_commitment_cache,
                                    &optimistically_confirmed_bank,
                                )
                            });
                        }
                        NotificationEntry::Unsubscribed(params, id) => {
                            subscriptions.unsubscribe(params, id);
                        }
                        NotificationEntry::Slot(slot_info) => {
                            if let Some(sub) = subscriptions
                                .node_progress_watchers()
                                .get(&SubscriptionParams::Slot)
                            {
                                debug!("slot notify: {:?}", slot_info);
                                inc_new_counter_info!("rpc-subscription-notify-slot", 1);
                                notifier.notify(&slot_info, sub, false);
                            }
                        }
                        NotificationEntry::SlotUpdate(slot_update) => {
                            if let Some(sub) = subscriptions
                                .node_progress_watchers()
                                .get(&SubscriptionParams::SlotsUpdates)
                            {
                                inc_new_counter_info!("rpc-subscription-notify-slots-updates", 1);
                                notifier.notify(&slot_update, sub, false);
                            }
                        }
                        // These notifications are only triggered by votes observed on gossip,
                        // unlike `NotificationEntry::Gossip`, which also accounts for slots seen
                        // in VoteState's from bank states built in ReplayStage.
                        NotificationEntry::Vote(ref vote_info) => {
                            let rpc_vote = RpcVote {
                                // TODO: Remove clones
                                slots: vote_info.slots.clone(),
                                hash: bs58::encode(vote_info.hash).into_string(),
                                timestamp: vote_info.timestamp,
                            };
                            if let Some(sub) = subscriptions
                                .node_progress_watchers()
                                .get(&SubscriptionParams::Vote)
                            {
                                debug!("vote notify: {:?}", vote_info);
                                inc_new_counter_info!("rpc-subscription-notify-vote", 1);
                                notifier.notify(&rpc_vote, sub, false);
                            }
                        }
                        NotificationEntry::Root(root) => {
                            if let Some(sub) = subscriptions
                                .node_progress_watchers()
                                .get(&SubscriptionParams::Root)
                            {
                                debug!("root notify: {:?}", root);
                                inc_new_counter_info!("rpc-subscription-notify-root", 1);
                                notifier.notify(&root, sub, false);
                            }
                        }
                        NotificationEntry::Bank(commitment_slots) => {
                            RpcSubscriptions::notify_accounts_logs_programs_signatures(
                                subscriptions.commitment_watchers(),
                                &bank_forks,
                                &commitment_slots,
                                &mut notifier,
                                "bank",
                            )
                        }
                        NotificationEntry::Gossip(slot) => {
                            let commitment_slots = CommitmentSlots {
                                highest_confirmed_slot: slot,
                                ..CommitmentSlots::default()
                            };

                            RpcSubscriptions::notify_accounts_logs_programs_signatures(
                                subscriptions.gossip_watchers(),
                                &bank_forks,
                                &commitment_slots,
                                &mut notifier,
                                "gossip",
                            )
                        }
                        NotificationEntry::SignaturesReceived((slot, slot_signatures)) => {
                            for slot_signature in &slot_signatures {
                                if let Some(subs) = subscriptions.by_signature().get(slot_signature)
                                {
                                    for subscription in subs.values() {
                                        if let SubscriptionParams::Signature(params) =
                                            subscription.params()
                                        {
                                            if params.enable_received_notification {
                                                notifier.notify(
                                                    Response {
                                                        context: RpcResponseContext { slot },
                                                        value: RpcSignatureResult::ReceivedSignature(
                                                            ReceivedSignatureResult::ReceivedSignature,
                                                        ),
                                                    },
                                                    subscription,
                                                    false,
                                                );
                                            }
                                        } else {
                                            error!("invalid params type in visit_by_signature");
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    // not a problem - try reading again
                }
                Err(RecvTimeoutError::Disconnected) => {
                    warn!("RPC Notification thread - sender disconnected");
                    break;
                }
            }
        }
    }

    fn notify_accounts_logs_programs_signatures(
        subscriptions: &HashMap<SubscriptionId, Arc<SubscriptionInfo>>,
        bank_forks: &Arc<RwLock<BankForks>>,
        commitment_slots: &CommitmentSlots,
        notifier: &mut RpcNotifier,
        source: &'static str,
    ) {
        let mut total_time = Measure::start("notify_accounts_logs_programs_signatures");
        let mut num_accounts_found = 0;
        let mut num_accounts_notified = 0;

        let mut num_logs_found = 0;
        let mut num_logs_notified = 0;

        let mut num_signatures_found = 0;
        let mut num_signatures_notified = 0;

        let mut num_programs_found = 0;
        let mut num_programs_notified = 0;

        for subscription in subscriptions.values() {
            match subscription.params() {
                SubscriptionParams::Account(params) => {
                    let notified = check_commitment_and_notify(
                        params,
                        subscription,
                        bank_forks,
                        commitment_slots,
                        |bank, params| bank.get_account_modified_slot(&params.pubkey),
                        filter_account_result,
                        notifier,
                        false,
                    );

                    num_accounts_found += 1;

                    if notified {
                        num_accounts_notified += 1;
                    }
                }
                SubscriptionParams::Logs(params) => {
                    let notified = check_commitment_and_notify(
                        params,
                        subscription,
                        bank_forks,
                        commitment_slots,
                        get_transaction_logs,
                        filter_logs_results,
                        notifier,
                        false,
                    );
                    num_logs_found += 1;

                    if notified {
                        num_logs_notified += 1;
                    }
                }
                SubscriptionParams::Program(params) => {
                    let notified = check_commitment_and_notify(
                        params,
                        subscription,
                        bank_forks,
                        commitment_slots,
                        |bank, params| {
                            bank.get_program_accounts_modified_since_parent(&params.pubkey)
                        },
                        filter_program_results,
                        notifier,
                        false,
                    );
                    num_programs_found += 1;

                    if notified {
                        num_programs_notified += 1;
                    }
                }
                SubscriptionParams::Signature(params) => {
                    let notified = check_commitment_and_notify(
                        params,
                        subscription,
                        bank_forks,
                        commitment_slots,
                        |bank, params| {
                            bank.get_signature_status_processed_since_parent(&params.signature)
                        },
                        filter_signature_result,
                        notifier,
                        true, // Unsubscribe.
                    );
                    num_signatures_found += 1;

                    if notified {
                        num_signatures_notified += 1;
                    }
                }
                _ => error!("wrong subscription type in alps map"),
            }
        }

        total_time.stop();

        let total_notified = num_accounts_notified
            + num_logs_notified
            + num_programs_notified
            + num_signatures_notified;
        let total_ms = total_time.as_ms();
        if total_notified > 0 || total_ms > 10 {
            debug!(
                "notified({}): accounts: {} / {} logs: {} / {} programs: {} / {} signatures: {} / {}",
                source,
                num_accounts_found,
                num_accounts_notified,
                num_logs_found,
                num_logs_notified,
                num_programs_found,
                num_programs_notified,
                num_signatures_found,
                num_signatures_notified,
            );
            inc_new_counter_info!("rpc-subscription-notify-bank-or-gossip", total_notified);
            datapoint_info!(
                "rpc_subscriptions",
                ("source", source.to_string(), String),
                ("num_account_subscriptions", num_accounts_found, i64),
                ("num_account_pubkeys_notified", num_accounts_notified, i64),
                ("num_logs_subscriptions", num_logs_found, i64),
                ("num_logs_notified", num_logs_notified, i64),
                ("num_program_subscriptions", num_programs_found, i64),
                ("num_programs_notified", num_programs_notified, i64),
                ("num_signature_subscriptions", num_signatures_found, i64),
                ("num_signatures_notified", num_signatures_notified, i64),
                ("notifications_time", total_time.as_us() as i64, i64),
            );
            inc_new_counter_info!(
                "rpc-subscription-counter-num_accounts_notified",
                num_accounts_notified
            );
            inc_new_counter_info!(
                "rpc-subscription-counter-num_logs_notified",
                num_logs_notified
            );
            inc_new_counter_info!(
                "rpc-subscription-counter-num_programs_notified",
                num_programs_notified
            );
            inc_new_counter_info!(
                "rpc-subscription-counter-num_signatures_notified",
                num_signatures_notified
            );
        }
    }

    fn shutdown(&mut self) -> std::thread::Result<()> {
        if self.t_cleanup.is_some() {
            info!("RPC Notification thread - shutting down");
            self.exit.store(true, Ordering::Relaxed);
            let x = self.t_cleanup.take().unwrap().join();
            info!("RPC Notification thread - shut down.");
            x
        } else {
            warn!("RPC Notification thread - already shut down.");
            Ok(())
        }
    }

    #[cfg(test)]
    fn total(&self) -> usize {
        self.control.total()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use {
        super::*,
        crate::{
            optimistically_confirmed_bank_tracker::{
                BankNotification, OptimisticallyConfirmedBank, OptimisticallyConfirmedBankTracker,
            },
            rpc_pubsub::RpcSolPubSubInternal,
            rpc_pubsub_service,
        },
        serial_test::serial,
        solana_client::rpc_config::{
            RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcSignatureSubscribeConfig,
            RpcTransactionLogsFilter,
        },
        solana_runtime::{
            commitment::BlockCommitment,
            genesis_utils::{create_genesis_config, GenesisConfigInfo},
        },
        solana_sdk::{
            commitment_config::CommitmentConfig,
            message::Message,
            signature::{Keypair, Signer},
            stake, system_instruction, system_program, system_transaction,
            transaction::Transaction,
        },
        std::{collections::HashSet, sync::atomic::Ordering::Relaxed},
    };

    fn make_account_result(lamports: u64, subscription: u64, data: &str) -> serde_json::Value {
        json!({
           "jsonrpc": "2.0",
           "method": "accountNotification",
           "params": {
               "result": {
                   "context": { "slot": 1 },
                   "value": {
                       "data": data,
                       "executable": false,
                       "lamports": lamports,
                       "owner": "11111111111111111111111111111111",
                       "rentEpoch": 0,
                    },
               },
               "subscription": subscription,
           }
        })
    }

    #[test]
    #[serial]
    fn test_check_account_subscribe() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(100);
        let bank = Bank::new_for_tests(&genesis_config);
        let blockhash = bank.last_blockhash();
        let bank_forks = Arc::new(RwLock::new(BankForks::new(bank)));
        let bank0 = bank_forks.read().unwrap().get(0).unwrap().clone();
        let bank1 = Bank::new_from_parent(&bank0, &Pubkey::default(), 1);
        bank_forks.write().unwrap().insert(bank1);
        let alice = Keypair::new();

        let exit = Arc::new(AtomicBool::new(false));
        let subscriptions = Arc::new(RpcSubscriptions::new_for_tests(
            &exit,
            bank_forks.clone(),
            Arc::new(RwLock::new(BlockCommitmentCache::new_for_tests_with_slots(
                1, 1,
            ))),
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks),
        ));

        let tx0 = system_transaction::create_account(
            &mint_keypair,
            &alice,
            blockhash,
            1,
            0,
            &system_program::id(),
        );
        let expected0 = make_account_result(1, 0, "");

        let tx1 = {
            let instruction =
                system_instruction::transfer(&alice.pubkey(), &mint_keypair.pubkey(), 1);
            let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
            Transaction::new(&[&alice, &mint_keypair], message, blockhash)
        };
        let expected1 = make_account_result(0, 1, "");

        let tx2 = system_transaction::create_account(
            &mint_keypair,
            &alice,
            blockhash,
            1,
            1024,
            &system_program::id(),
        );
        let expected2 = make_account_result(1, 2, "error: data too large for bs58 encoding");

        let subscribe_cases = vec![
            (alice.pubkey(), tx0, expected0),
            (alice.pubkey(), tx1, expected1),
            (alice.pubkey(), tx2, expected2),
        ];

        for (pubkey, tx, expected) in subscribe_cases {
            let (rpc, mut receiver) = rpc_pubsub_service::test_connection(&subscriptions);

            let sub_id = rpc
                .account_subscribe(
                    pubkey.to_string(),
                    Some(RpcAccountInfoConfig {
                        commitment: Some(CommitmentConfig::processed()),
                        encoding: None,
                        data_slice: None,
                    }),
                )
                .unwrap();

            subscriptions
                .control
                .assert_subscribed(&SubscriptionParams::Account(AccountSubscriptionParams {
                    pubkey,
                    commitment: CommitmentConfig::processed(),
                    data_slice: None,
                    encoding: UiAccountEncoding::Binary,
                }));

            bank_forks
                .read()
                .unwrap()
                .get(1)
                .unwrap()
                .process_transaction(&tx)
                .unwrap();
            let commitment_slots = CommitmentSlots {
                slot: 1,
                ..CommitmentSlots::default()
            };
            subscriptions.notify_subscribers(commitment_slots);
            let response = receiver.recv();

            assert_eq!(
                expected,
                serde_json::from_str::<serde_json::Value>(&response).unwrap(),
            );
            rpc.account_unsubscribe(sub_id).unwrap();

            subscriptions
                .control
                .assert_unsubscribed(&SubscriptionParams::Account(AccountSubscriptionParams {
                    pubkey,
                    commitment: CommitmentConfig::processed(),
                    data_slice: None,
                    encoding: UiAccountEncoding::Binary,
                }));
        }
    }

    #[test]
    #[serial]
    fn test_check_program_subscribe() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(100);
        let bank = Bank::new_for_tests(&genesis_config);
        let blockhash = bank.last_blockhash();
        let bank_forks = Arc::new(RwLock::new(BankForks::new(bank)));
        let alice = Keypair::new();
        let tx = system_transaction::create_account(
            &mint_keypair,
            &alice,
            blockhash,
            1,
            16,
            &stake::program::id(),
        );
        bank_forks
            .write()
            .unwrap()
            .get(0)
            .unwrap()
            .process_transaction(&tx)
            .unwrap();

        let exit = Arc::new(AtomicBool::new(false));
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let subscriptions = Arc::new(RpcSubscriptions::new_for_tests(
            &exit,
            bank_forks,
            Arc::new(RwLock::new(BlockCommitmentCache::new_for_tests())),
            optimistically_confirmed_bank,
        ));
        let (rpc, mut receiver) = rpc_pubsub_service::test_connection(&subscriptions);
        let sub_id = rpc
            .program_subscribe(
                stake::program::id().to_string(),
                Some(RpcProgramAccountsConfig {
                    account_config: RpcAccountInfoConfig {
                        commitment: Some(CommitmentConfig::processed()),
                        ..RpcAccountInfoConfig::default()
                    },
                    ..RpcProgramAccountsConfig::default()
                }),
            )
            .unwrap();

        subscriptions
            .control
            .assert_subscribed(&SubscriptionParams::Program(ProgramSubscriptionParams {
                pubkey: stake::program::id(),
                filters: Vec::new(),
                commitment: CommitmentConfig::processed(),
                data_slice: None,
                encoding: UiAccountEncoding::Binary,
                with_context: false,
            }));

        subscriptions.notify_subscribers(CommitmentSlots::default());
        let response = receiver.recv();
        let expected = json!({
           "jsonrpc": "2.0",
           "method": "programNotification",
           "params": {
               "result": {
                   "context": { "slot": 0 },
                   "value": {
                       "account": {
                          "data": "1111111111111111",
                          "executable": false,
                          "lamports": 1,
                          "owner": "Stake11111111111111111111111111111111111111",
                          "rentEpoch": 0,
                       },
                       "pubkey": alice.pubkey().to_string(),
                    },
               },
               "subscription": 0,
           }
        });
        assert_eq!(
            expected,
            serde_json::from_str::<serde_json::Value>(&response).unwrap(),
        );

        rpc.program_unsubscribe(sub_id).unwrap();
        subscriptions
            .control
            .assert_unsubscribed(&SubscriptionParams::Program(ProgramSubscriptionParams {
                pubkey: stake::program::id(),
                filters: Vec::new(),
                commitment: CommitmentConfig::processed(),
                data_slice: None,
                encoding: UiAccountEncoding::Binary,
                with_context: false,
            }));
    }

    #[test]
    #[serial]
    fn test_check_program_subscribe_for_missing_optimistically_confirmed_slot() {
        // Testing if we can get the pubsub notification if a slot does not
        // receive OptimisticallyConfirmed but its descendant slot get the confirmed
        // notification.
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(100);
        let bank = Bank::new_for_tests(&genesis_config);
        bank.lazy_rent_collection.store(true, Relaxed);

        let blockhash = bank.last_blockhash();
        let bank_forks = Arc::new(RwLock::new(BankForks::new(bank)));

        let bank0 = bank_forks.read().unwrap().get(0).unwrap().clone();
        let bank1 = Bank::new_from_parent(&bank0, &Pubkey::default(), 1);
        bank_forks.write().unwrap().insert(bank1);
        let bank1 = bank_forks.read().unwrap().get(1).unwrap().clone();

        // add account for alice and process the transaction at bank1
        let alice = Keypair::new();
        let tx = system_transaction::create_account(
            &mint_keypair,
            &alice,
            blockhash,
            1,
            16,
            &stake::program::id(),
        );

        bank1.process_transaction(&tx).unwrap();

        let bank2 = Bank::new_from_parent(&bank1, &Pubkey::default(), 2);
        bank_forks.write().unwrap().insert(bank2);

        // add account for bob and process the transaction at bank2
        let bob = Keypair::new();
        let tx = system_transaction::create_account(
            &mint_keypair,
            &bob,
            blockhash,
            2,
            16,
            &stake::program::id(),
        );
        let bank2 = bank_forks.read().unwrap().get(2).unwrap().clone();

        bank2.process_transaction(&tx).unwrap();

        let bank3 = Bank::new_from_parent(&bank2, &Pubkey::default(), 3);
        bank_forks.write().unwrap().insert(bank3);

        // add account for joe and process the transaction at bank3
        let joe = Keypair::new();
        let tx = system_transaction::create_account(
            &mint_keypair,
            &joe,
            blockhash,
            3,
            16,
            &stake::program::id(),
        );
        let bank3 = bank_forks.read().unwrap().get(3).unwrap().clone();

        bank3.process_transaction(&tx).unwrap();

        // now add programSubscribe at the "confirmed" commitment level
        let exit = Arc::new(AtomicBool::new(false));
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let mut pending_optimistically_confirmed_banks = HashSet::new();

        let subscriptions = Arc::new(RpcSubscriptions::new_for_tests(
            &exit,
            bank_forks.clone(),
            Arc::new(RwLock::new(BlockCommitmentCache::new_for_tests_with_slots(
                1, 1,
            ))),
            optimistically_confirmed_bank.clone(),
        ));

        let (rpc, mut receiver) = rpc_pubsub_service::test_connection(&subscriptions);

        let sub_id = rpc
            .program_subscribe(
                stake::program::id().to_string(),
                Some(RpcProgramAccountsConfig {
                    account_config: RpcAccountInfoConfig {
                        commitment: Some(CommitmentConfig::confirmed()),
                        ..RpcAccountInfoConfig::default()
                    },
                    ..RpcProgramAccountsConfig::default()
                }),
            )
            .unwrap();

        subscriptions
            .control
            .assert_subscribed(&SubscriptionParams::Program(ProgramSubscriptionParams {
                pubkey: stake::program::id(),
                filters: Vec::new(),
                encoding: UiAccountEncoding::Binary,
                data_slice: None,
                commitment: CommitmentConfig::confirmed(),
                with_context: false,
            }));

        let mut highest_confirmed_slot: Slot = 0;
        let mut last_notified_confirmed_slot: Slot = 0;
        // Optimistically notifying slot 3 without notifying slot 1 and 2, bank3 is unfrozen, we expect
        // to see transaction for alice and bob to be notified in order.
        OptimisticallyConfirmedBankTracker::process_notification(
            BankNotification::OptimisticallyConfirmed(3),
            &bank_forks,
            &optimistically_confirmed_bank,
            &subscriptions,
            &mut pending_optimistically_confirmed_banks,
            &mut last_notified_confirmed_slot,
            &mut highest_confirmed_slot,
            &None,
        );

        // a closure to reduce code duplications in building expected responses:
        let build_expected_resp = |slot: Slot, lamports: u64, pubkey: &str, subscription: i32| {
            json!({
               "jsonrpc": "2.0",
               "method": "programNotification",
               "params": {
                   "result": {
                       "context": { "slot": slot },
                       "value": {
                           "account": {
                              "data": "1111111111111111",
                              "executable": false,
                              "lamports": lamports,
                              "owner": "Stake11111111111111111111111111111111111111",
                              "rentEpoch": 0,
                           },
                           "pubkey": pubkey,
                        },
                   },
                   "subscription": subscription,
               }
            })
        };

        let response = receiver.recv();
        let expected = build_expected_resp(1, 1, &alice.pubkey().to_string(), 0);
        assert_eq!(
            expected,
            serde_json::from_str::<serde_json::Value>(&response).unwrap(),
        );

        let response = receiver.recv();
        let expected = build_expected_resp(2, 2, &bob.pubkey().to_string(), 0);
        assert_eq!(
            expected,
            serde_json::from_str::<serde_json::Value>(&response).unwrap(),
        );

        bank3.freeze();
        OptimisticallyConfirmedBankTracker::process_notification(
            BankNotification::Frozen(bank3),
            &bank_forks,
            &optimistically_confirmed_bank,
            &subscriptions,
            &mut pending_optimistically_confirmed_banks,
            &mut last_notified_confirmed_slot,
            &mut highest_confirmed_slot,
            &None,
        );

        let response = receiver.recv();
        let expected = build_expected_resp(3, 3, &joe.pubkey().to_string(), 0);
        assert_eq!(
            expected,
            serde_json::from_str::<serde_json::Value>(&response).unwrap(),
        );
        rpc.program_unsubscribe(sub_id).unwrap();
    }

    #[test]
    #[serial]
    #[should_panic]
    fn test_check_program_subscribe_for_missing_optimistically_confirmed_slot_with_no_banks_no_notifications(
    ) {
        // Testing if we can get the pubsub notification if a slot does not
        // receive OptimisticallyConfirmed but its descendant slot get the confirmed
        // notification with a bank in the BankForks. We are not expecting to receive any notifications -- should panic.
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(100);
        let bank = Bank::new_for_tests(&genesis_config);
        bank.lazy_rent_collection.store(true, Relaxed);

        let blockhash = bank.last_blockhash();
        let bank_forks = Arc::new(RwLock::new(BankForks::new(bank)));

        let bank0 = bank_forks.read().unwrap().get(0).unwrap().clone();
        let bank1 = Bank::new_from_parent(&bank0, &Pubkey::default(), 1);
        bank_forks.write().unwrap().insert(bank1);
        let bank1 = bank_forks.read().unwrap().get(1).unwrap().clone();

        // add account for alice and process the transaction at bank1
        let alice = Keypair::new();
        let tx = system_transaction::create_account(
            &mint_keypair,
            &alice,
            blockhash,
            1,
            16,
            &stake::program::id(),
        );

        bank1.process_transaction(&tx).unwrap();

        let bank2 = Bank::new_from_parent(&bank1, &Pubkey::default(), 2);
        bank_forks.write().unwrap().insert(bank2);

        // add account for bob and process the transaction at bank2
        let bob = Keypair::new();
        let tx = system_transaction::create_account(
            &mint_keypair,
            &bob,
            blockhash,
            2,
            16,
            &stake::program::id(),
        );
        let bank2 = bank_forks.read().unwrap().get(2).unwrap().clone();

        bank2.process_transaction(&tx).unwrap();

        // now add programSubscribe at the "confirmed" commitment level
        let exit = Arc::new(AtomicBool::new(false));
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let mut pending_optimistically_confirmed_banks = HashSet::new();

        let subscriptions = Arc::new(RpcSubscriptions::new_for_tests(
            &exit,
            bank_forks.clone(),
            Arc::new(RwLock::new(BlockCommitmentCache::new_for_tests_with_slots(
                1, 1,
            ))),
            optimistically_confirmed_bank.clone(),
        ));
        let (rpc, mut receiver) = rpc_pubsub_service::test_connection(&subscriptions);
        rpc.program_subscribe(
            stake::program::id().to_string(),
            Some(RpcProgramAccountsConfig {
                account_config: RpcAccountInfoConfig {
                    commitment: Some(CommitmentConfig::confirmed()),
                    ..RpcAccountInfoConfig::default()
                },
                ..RpcProgramAccountsConfig::default()
            }),
        )
        .unwrap();

        subscriptions
            .control
            .assert_subscribed(&SubscriptionParams::Program(ProgramSubscriptionParams {
                pubkey: stake::program::id(),
                filters: Vec::new(),
                encoding: UiAccountEncoding::Binary,
                data_slice: None,
                commitment: CommitmentConfig::confirmed(),
                with_context: false,
            }));

        let mut highest_confirmed_slot: Slot = 0;
        let mut last_notified_confirmed_slot: Slot = 0;
        // Optimistically notifying slot 3 without notifying slot 1 and 2, bank3 is not in the bankforks, we do not
        // expect to see any RPC notifications.
        OptimisticallyConfirmedBankTracker::process_notification(
            BankNotification::OptimisticallyConfirmed(3),
            &bank_forks,
            &optimistically_confirmed_bank,
            &subscriptions,
            &mut pending_optimistically_confirmed_banks,
            &mut last_notified_confirmed_slot,
            &mut highest_confirmed_slot,
            &None,
        );

        // The following should panic
        let _response = receiver.recv();
    }

    #[test]
    #[serial]
    fn test_check_program_subscribe_for_missing_optimistically_confirmed_slot_with_no_banks() {
        // Testing if we can get the pubsub notification if a slot does not
        // receive OptimisticallyConfirmed but its descendant slot get the confirmed
        // notification. It differs from the test_check_program_subscribe_for_missing_optimistically_confirmed_slot
        // test in that when the descendant get confirmed, the descendant does not have a bank yet.
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(100);
        let bank = Bank::new_for_tests(&genesis_config);
        bank.lazy_rent_collection.store(true, Relaxed);

        let blockhash = bank.last_blockhash();
        let bank_forks = Arc::new(RwLock::new(BankForks::new(bank)));

        let bank0 = bank_forks.read().unwrap().get(0).unwrap().clone();
        let bank1 = Bank::new_from_parent(&bank0, &Pubkey::default(), 1);
        bank_forks.write().unwrap().insert(bank1);
        let bank1 = bank_forks.read().unwrap().get(1).unwrap().clone();

        // add account for alice and process the transaction at bank1
        let alice = Keypair::new();
        let tx = system_transaction::create_account(
            &mint_keypair,
            &alice,
            blockhash,
            1,
            16,
            &stake::program::id(),
        );

        bank1.process_transaction(&tx).unwrap();

        let bank2 = Bank::new_from_parent(&bank1, &Pubkey::default(), 2);
        bank_forks.write().unwrap().insert(bank2);

        // add account for bob and process the transaction at bank2
        let bob = Keypair::new();
        let tx = system_transaction::create_account(
            &mint_keypair,
            &bob,
            blockhash,
            2,
            16,
            &stake::program::id(),
        );
        let bank2 = bank_forks.read().unwrap().get(2).unwrap().clone();

        bank2.process_transaction(&tx).unwrap();

        // now add programSubscribe at the "confirmed" commitment level
        let exit = Arc::new(AtomicBool::new(false));
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let mut pending_optimistically_confirmed_banks = HashSet::new();

        let subscriptions = Arc::new(RpcSubscriptions::new_for_tests(
            &exit,
            bank_forks.clone(),
            Arc::new(RwLock::new(BlockCommitmentCache::new_for_tests_with_slots(
                1, 1,
            ))),
            optimistically_confirmed_bank.clone(),
        ));
        let (rpc, mut receiver) = rpc_pubsub_service::test_connection(&subscriptions);
        let sub_id = rpc
            .program_subscribe(
                stake::program::id().to_string(),
                Some(RpcProgramAccountsConfig {
                    account_config: RpcAccountInfoConfig {
                        commitment: Some(CommitmentConfig::confirmed()),
                        ..RpcAccountInfoConfig::default()
                    },
                    ..RpcProgramAccountsConfig::default()
                }),
            )
            .unwrap();

        subscriptions
            .control
            .assert_subscribed(&SubscriptionParams::Program(ProgramSubscriptionParams {
                pubkey: stake::program::id(),
                filters: Vec::new(),
                encoding: UiAccountEncoding::Binary,
                data_slice: None,
                commitment: CommitmentConfig::confirmed(),
                with_context: false,
            }));

        let mut highest_confirmed_slot: Slot = 0;
        let mut last_notified_confirmed_slot: Slot = 0;
        // Optimistically notifying slot 3 without notifying slot 1 and 2, bank3 is not in the bankforks, we expect
        // to see transaction for alice and bob to be notified only when bank3 is added to the fork and
        // frozen. The notifications should be in the increasing order of the slot.
        OptimisticallyConfirmedBankTracker::process_notification(
            BankNotification::OptimisticallyConfirmed(3),
            &bank_forks,
            &optimistically_confirmed_bank,
            &subscriptions,
            &mut pending_optimistically_confirmed_banks,
            &mut last_notified_confirmed_slot,
            &mut highest_confirmed_slot,
            &None,
        );

        // a closure to reduce code duplications in building expected responses:
        let build_expected_resp = |slot: Slot, lamports: u64, pubkey: &str, subscription: i32| {
            json!({
               "jsonrpc": "2.0",
               "method": "programNotification",
               "params": {
                   "result": {
                       "context": { "slot": slot },
                       "value": {
                           "account": {
                              "data": "1111111111111111",
                              "executable": false,
                              "lamports": lamports,
                              "owner": "Stake11111111111111111111111111111111111111",
                              "rentEpoch": 0,
                           },
                           "pubkey": pubkey,
                        },
                   },
                   "subscription": subscription,
               }
            })
        };

        let bank3 = Bank::new_from_parent(&bank2, &Pubkey::default(), 3);
        bank_forks.write().unwrap().insert(bank3);

        // add account for joe and process the transaction at bank3
        let joe = Keypair::new();
        let tx = system_transaction::create_account(
            &mint_keypair,
            &joe,
            blockhash,
            3,
            16,
            &stake::program::id(),
        );
        let bank3 = bank_forks.read().unwrap().get(3).unwrap().clone();

        bank3.process_transaction(&tx).unwrap();
        bank3.freeze();
        OptimisticallyConfirmedBankTracker::process_notification(
            BankNotification::Frozen(bank3),
            &bank_forks,
            &optimistically_confirmed_bank,
            &subscriptions,
            &mut pending_optimistically_confirmed_banks,
            &mut last_notified_confirmed_slot,
            &mut highest_confirmed_slot,
            &None,
        );

        let response = receiver.recv();
        let expected = build_expected_resp(1, 1, &alice.pubkey().to_string(), 0);
        assert_eq!(
            expected,
            serde_json::from_str::<serde_json::Value>(&response).unwrap(),
        );

        let response = receiver.recv();
        let expected = build_expected_resp(2, 2, &bob.pubkey().to_string(), 0);
        assert_eq!(
            expected,
            serde_json::from_str::<serde_json::Value>(&response).unwrap(),
        );

        let response = receiver.recv();
        let expected = build_expected_resp(3, 3, &joe.pubkey().to_string(), 0);
        assert_eq!(
            expected,
            serde_json::from_str::<serde_json::Value>(&response).unwrap(),
        );
        rpc.program_unsubscribe(sub_id).unwrap();
    }

    #[test]
    #[serial]
    fn test_check_signature_subscribe() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(100);
        let bank = Bank::new_for_tests(&genesis_config);
        let blockhash = bank.last_blockhash();
        let mut bank_forks = BankForks::new(bank);
        let alice = Keypair::new();

        let past_bank_tx =
            system_transaction::transfer(&mint_keypair, &alice.pubkey(), 1, blockhash);
        let unprocessed_tx =
            system_transaction::transfer(&mint_keypair, &alice.pubkey(), 2, blockhash);
        let processed_tx =
            system_transaction::transfer(&mint_keypair, &alice.pubkey(), 3, blockhash);

        bank_forks
            .get(0)
            .unwrap()
            .process_transaction(&past_bank_tx)
            .unwrap();

        let next_bank = Bank::new_from_parent(
            &bank_forks.get(0).unwrap().clone(),
            &solana_sdk::pubkey::new_rand(),
            1,
        );
        bank_forks.insert(next_bank);

        bank_forks
            .get(1)
            .unwrap()
            .process_transaction(&processed_tx)
            .unwrap();
        let bank1 = bank_forks[1].clone();

        let bank_forks = Arc::new(RwLock::new(bank_forks));

        let mut cache0 = BlockCommitment::default();
        cache0.increase_confirmation_stake(1, 10);
        let cache1 = BlockCommitment::default();

        let mut block_commitment = HashMap::new();
        block_commitment.entry(0).or_insert(cache0);
        block_commitment.entry(1).or_insert(cache1);
        let block_commitment_cache = BlockCommitmentCache::new(
            block_commitment,
            10,
            CommitmentSlots {
                slot: bank1.slot(),
                ..CommitmentSlots::default()
            },
        );

        let exit = Arc::new(AtomicBool::new(false));
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let subscriptions = Arc::new(RpcSubscriptions::new_for_tests(
            &exit,
            bank_forks,
            Arc::new(RwLock::new(block_commitment_cache)),
            optimistically_confirmed_bank,
        ));

        let (past_bank_rpc1, mut past_bank_receiver1) =
            rpc_pubsub_service::test_connection(&subscriptions);
        let (past_bank_rpc2, mut past_bank_receiver2) =
            rpc_pubsub_service::test_connection(&subscriptions);
        let (processed_rpc, mut processed_receiver) =
            rpc_pubsub_service::test_connection(&subscriptions);
        let (another_rpc, _another_receiver) = rpc_pubsub_service::test_connection(&subscriptions);
        let (processed_rpc3, mut processed_receiver3) =
            rpc_pubsub_service::test_connection(&subscriptions);

        let past_bank_sub_id1 = past_bank_rpc1
            .signature_subscribe(
                past_bank_tx.signatures[0].to_string(),
                Some(RpcSignatureSubscribeConfig {
                    commitment: Some(CommitmentConfig::processed()),
                    enable_received_notification: Some(false),
                }),
            )
            .unwrap();
        let past_bank_sub_id2 = past_bank_rpc2
            .signature_subscribe(
                past_bank_tx.signatures[0].to_string(),
                Some(RpcSignatureSubscribeConfig {
                    commitment: Some(CommitmentConfig::finalized()),
                    enable_received_notification: Some(false),
                }),
            )
            .unwrap();
        let processed_sub_id = processed_rpc
            .signature_subscribe(
                processed_tx.signatures[0].to_string(),
                Some(RpcSignatureSubscribeConfig {
                    commitment: Some(CommitmentConfig::processed()),
                    enable_received_notification: Some(false),
                }),
            )
            .unwrap();
        another_rpc
            .signature_subscribe(
                unprocessed_tx.signatures[0].to_string(),
                Some(RpcSignatureSubscribeConfig {
                    commitment: Some(CommitmentConfig::processed()),
                    enable_received_notification: Some(false),
                }),
            )
            .unwrap();

        // Add a subscription that gets `received` notifications
        let processed_sub_id3 = processed_rpc3
            .signature_subscribe(
                unprocessed_tx.signatures[0].to_string(),
                Some(RpcSignatureSubscribeConfig {
                    commitment: Some(CommitmentConfig::processed()),
                    enable_received_notification: Some(true),
                }),
            )
            .unwrap();

        assert!(subscriptions
            .control
            .signature_subscribed(&unprocessed_tx.signatures[0]));
        assert!(subscriptions
            .control
            .signature_subscribed(&processed_tx.signatures[0]));

        let mut commitment_slots = CommitmentSlots::default();
        let received_slot = 1;
        commitment_slots.slot = received_slot;
        subscriptions
            .notify_signatures_received((received_slot, vec![unprocessed_tx.signatures[0]]));
        subscriptions.notify_subscribers(commitment_slots);
        let expected_res =
            RpcSignatureResult::ProcessedSignature(ProcessedSignatureResult { err: None });
        let received_expected_res =
            RpcSignatureResult::ReceivedSignature(ReceivedSignatureResult::ReceivedSignature);
        struct Notification {
            slot: Slot,
            id: u64,
        }

        let expected_notification =
            |exp: Notification, expected_res: &RpcSignatureResult| -> String {
                let json = json!({
                    "jsonrpc": "2.0",
                    "method": "signatureNotification",
                    "params": {
                        "result": {
                            "context": { "slot": exp.slot },
                            "value": expected_res,
                        },
                        "subscription": exp.id,
                    }
                });
                serde_json::to_string(&json).unwrap()
            };

        // Expect to receive a notification from bank 1 because this subscription is
        // looking for 0 confirmations and so checks the current bank
        let expected = expected_notification(
            Notification {
                slot: 1,
                id: past_bank_sub_id1.into(),
            },
            &expected_res,
        );
        let response = past_bank_receiver1.recv();
        assert_eq!(expected, response);

        // Expect to receive a notification from bank 0 because this subscription is
        // looking for 1 confirmation and so checks the past bank
        let expected = expected_notification(
            Notification {
                slot: 0,
                id: past_bank_sub_id2.into(),
            },
            &expected_res,
        );
        let response = past_bank_receiver2.recv();
        assert_eq!(expected, response);

        let expected = expected_notification(
            Notification {
                slot: 1,
                id: processed_sub_id.into(),
            },
            &expected_res,
        );
        let response = processed_receiver.recv();
        assert_eq!(expected, response);

        // Expect a "received" notification
        let expected = expected_notification(
            Notification {
                slot: received_slot,
                id: processed_sub_id3.into(),
            },
            &received_expected_res,
        );
        let response = processed_receiver3.recv();
        assert_eq!(expected, response);

        // Subscription should be automatically removed after notification

        assert!(!subscriptions
            .control
            .signature_subscribed(&processed_tx.signatures[0]));
        assert!(!subscriptions
            .control
            .signature_subscribed(&past_bank_tx.signatures[0]));

        // Unprocessed signature subscription should not be removed
        assert!(subscriptions
            .control
            .signature_subscribed(&unprocessed_tx.signatures[0]));
    }

    #[test]
    #[serial]
    fn test_check_slot_subscribe() {
        let exit = Arc::new(AtomicBool::new(false));
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = Arc::new(RwLock::new(BankForks::new(bank)));
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let subscriptions = Arc::new(RpcSubscriptions::new_for_tests(
            &exit,
            bank_forks,
            Arc::new(RwLock::new(BlockCommitmentCache::new_for_tests())),
            optimistically_confirmed_bank,
        ));
        let (rpc, mut receiver) = rpc_pubsub_service::test_connection(&subscriptions);
        let sub_id = rpc.slot_subscribe().unwrap();

        subscriptions
            .control
            .assert_subscribed(&SubscriptionParams::Slot);

        subscriptions.notify_slot(0, 0, 0);
        let response = receiver.recv();

        let expected_res = SlotInfo {
            parent: 0,
            slot: 0,
            root: 0,
        };
        let expected_res_str = serde_json::to_string(&expected_res).unwrap();

        let expected = format!(
            r#"{{"jsonrpc":"2.0","method":"slotNotification","params":{{"result":{},"subscription":0}}}}"#,
            expected_res_str
        );
        assert_eq!(expected, response);

        rpc.slot_unsubscribe(sub_id).unwrap();
        subscriptions
            .control
            .assert_unsubscribed(&SubscriptionParams::Slot);
    }

    #[test]
    #[serial]
    fn test_check_root_subscribe() {
        let exit = Arc::new(AtomicBool::new(false));
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = Arc::new(RwLock::new(BankForks::new(bank)));
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let subscriptions = Arc::new(RpcSubscriptions::new_for_tests(
            &exit,
            bank_forks,
            Arc::new(RwLock::new(BlockCommitmentCache::new_for_tests())),
            optimistically_confirmed_bank,
        ));
        let (rpc, mut receiver) = rpc_pubsub_service::test_connection(&subscriptions);
        let sub_id = rpc.root_subscribe().unwrap();

        subscriptions
            .control
            .assert_subscribed(&SubscriptionParams::Root);

        subscriptions.notify_roots(vec![2, 1, 3]);

        for expected_root in 1..=3 {
            let response = receiver.recv();

            let expected_res_str =
                serde_json::to_string(&serde_json::to_value(expected_root).unwrap()).unwrap();
            let expected = format!(
                r#"{{"jsonrpc":"2.0","method":"rootNotification","params":{{"result":{},"subscription":0}}}}"#,
                expected_res_str
            );
            assert_eq!(expected, response);
        }

        rpc.root_unsubscribe(sub_id).unwrap();
        subscriptions
            .control
            .assert_unsubscribed(&SubscriptionParams::Root);
    }

    #[test]
    #[serial]
    fn test_gossip_separate_account_notifications() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(100);
        let bank = Bank::new_for_tests(&genesis_config);
        let blockhash = bank.last_blockhash();
        let bank_forks = Arc::new(RwLock::new(BankForks::new(bank)));
        let bank0 = bank_forks.read().unwrap().get(0).unwrap().clone();
        let bank1 = Bank::new_from_parent(&bank0, &Pubkey::default(), 1);
        bank_forks.write().unwrap().insert(bank1);
        let bank2 = Bank::new_from_parent(&bank0, &Pubkey::default(), 2);
        bank_forks.write().unwrap().insert(bank2);
        let alice = Keypair::new();

        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let mut pending_optimistically_confirmed_banks = HashSet::new();

        let exit = Arc::new(AtomicBool::new(false));
        let subscriptions = Arc::new(RpcSubscriptions::new_for_tests(
            &exit,
            bank_forks.clone(),
            Arc::new(RwLock::new(BlockCommitmentCache::new_for_tests_with_slots(
                1, 1,
            ))),
            optimistically_confirmed_bank.clone(),
        ));
        let (rpc0, mut receiver0) = rpc_pubsub_service::test_connection(&subscriptions);
        let (rpc1, mut receiver1) = rpc_pubsub_service::test_connection(&subscriptions);
        let sub_id0 = rpc0
            .account_subscribe(
                alice.pubkey().to_string(),
                Some(RpcAccountInfoConfig {
                    commitment: Some(CommitmentConfig::confirmed()),
                    encoding: None,
                    data_slice: None,
                }),
            )
            .unwrap();

        assert!(subscriptions.control.account_subscribed(&alice.pubkey()));

        let tx = system_transaction::create_account(
            &mint_keypair,
            &alice,
            blockhash,
            1,
            16,
            &stake::program::id(),
        );

        // Add the transaction to the 1st bank and then freeze the bank
        let bank1 = bank_forks.write().unwrap().get(1).cloned().unwrap();
        bank1.process_transaction(&tx).unwrap();
        bank1.freeze();

        // Add the same transaction to the unfrozen 2nd bank
        bank_forks
            .write()
            .unwrap()
            .get(2)
            .unwrap()
            .process_transaction(&tx)
            .unwrap();

        // First, notify the unfrozen bank first to queue pending notification
        let mut highest_confirmed_slot: Slot = 0;
        let mut last_notified_confirmed_slot: Slot = 0;
        OptimisticallyConfirmedBankTracker::process_notification(
            BankNotification::OptimisticallyConfirmed(2),
            &bank_forks,
            &optimistically_confirmed_bank,
            &subscriptions,
            &mut pending_optimistically_confirmed_banks,
            &mut last_notified_confirmed_slot,
            &mut highest_confirmed_slot,
            &None,
        );

        // Now, notify the frozen bank and ensure its notifications are processed
        highest_confirmed_slot = 0;
        OptimisticallyConfirmedBankTracker::process_notification(
            BankNotification::OptimisticallyConfirmed(1),
            &bank_forks,
            &optimistically_confirmed_bank,
            &subscriptions,
            &mut pending_optimistically_confirmed_banks,
            &mut last_notified_confirmed_slot,
            &mut highest_confirmed_slot,
            &None,
        );

        let response = receiver0.recv();
        let expected = json!({
           "jsonrpc": "2.0",
           "method": "accountNotification",
           "params": {
               "result": {
                   "context": { "slot": 1 },
                   "value": {
                       "data": "1111111111111111",
                       "executable": false,
                       "lamports": 1,
                       "owner": "Stake11111111111111111111111111111111111111",
                       "rentEpoch": 0,
                    },
               },
               "subscription": 0,
           }
        });
        assert_eq!(
            expected,
            serde_json::from_str::<serde_json::Value>(&response).unwrap(),
        );
        rpc0.account_unsubscribe(sub_id0).unwrap();

        let sub_id1 = rpc1
            .account_subscribe(
                alice.pubkey().to_string(),
                Some(RpcAccountInfoConfig {
                    commitment: Some(CommitmentConfig::confirmed()),
                    encoding: None,
                    data_slice: None,
                }),
            )
            .unwrap();

        let bank2 = bank_forks.read().unwrap().get(2).unwrap().clone();
        bank2.freeze();
        highest_confirmed_slot = 0;
        OptimisticallyConfirmedBankTracker::process_notification(
            BankNotification::Frozen(bank2),
            &bank_forks,
            &optimistically_confirmed_bank,
            &subscriptions,
            &mut pending_optimistically_confirmed_banks,
            &mut last_notified_confirmed_slot,
            &mut highest_confirmed_slot,
            &None,
        );
        let response = receiver1.recv();
        let expected = json!({
           "jsonrpc": "2.0",
           "method": "accountNotification",
           "params": {
               "result": {
                   "context": { "slot": 2 },
                   "value": {
                       "data": "1111111111111111",
                       "executable": false,
                       "lamports": 1,
                       "owner": "Stake11111111111111111111111111111111111111",
                       "rentEpoch": 0,
                    },
               },
               "subscription": 1,
           }
        });
        assert_eq!(
            expected,
            serde_json::from_str::<serde_json::Value>(&response).unwrap(),
        );
        rpc1.account_unsubscribe(sub_id1).unwrap();

        assert!(!subscriptions.control.account_subscribed(&alice.pubkey()));
    }

    #[test]
    fn test_total_subscriptions() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(100);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = Arc::new(RwLock::new(BankForks::new(bank)));
        let subscriptions = Arc::new(RpcSubscriptions::default_with_bank_forks(bank_forks));

        let (rpc1, _receiver1) = rpc_pubsub_service::test_connection(&subscriptions);
        let sub_id1 = rpc1
            .account_subscribe(Pubkey::default().to_string(), None)
            .unwrap();

        assert_eq!(subscriptions.total(), 1);

        let (rpc2, _receiver2) = rpc_pubsub_service::test_connection(&subscriptions);
        let sub_id2 = rpc2
            .program_subscribe(Pubkey::default().to_string(), None)
            .unwrap();

        assert_eq!(subscriptions.total(), 2);

        let (rpc3, _receiver3) = rpc_pubsub_service::test_connection(&subscriptions);
        let sub_id3 = rpc3
            .logs_subscribe(RpcTransactionLogsFilter::All, None)
            .unwrap();
        assert_eq!(subscriptions.total(), 3);

        let (rpc4, _receiver4) = rpc_pubsub_service::test_connection(&subscriptions);
        let sub_id4 = rpc4
            .signature_subscribe(Signature::default().to_string(), None)
            .unwrap();

        assert_eq!(subscriptions.total(), 4);

        let (rpc5, _receiver5) = rpc_pubsub_service::test_connection(&subscriptions);
        let sub_id5 = rpc5.slot_subscribe().unwrap();

        assert_eq!(subscriptions.total(), 5);

        let (rpc6, _receiver6) = rpc_pubsub_service::test_connection(&subscriptions);
        let sub_id6 = rpc6.vote_subscribe().unwrap();

        assert_eq!(subscriptions.total(), 6);

        let (rpc7, _receiver7) = rpc_pubsub_service::test_connection(&subscriptions);
        let sub_id7 = rpc7.root_subscribe().unwrap();

        assert_eq!(subscriptions.total(), 7);

        // Add duplicate account subscription, but it shouldn't increment the count.
        let (rpc8, _receiver8) = rpc_pubsub_service::test_connection(&subscriptions);
        let sub_id8 = rpc8
            .account_subscribe(Pubkey::default().to_string(), None)
            .unwrap();
        assert_eq!(subscriptions.total(), 7);

        rpc1.account_unsubscribe(sub_id1).unwrap();
        assert_eq!(subscriptions.total(), 7);

        rpc8.account_unsubscribe(sub_id8).unwrap();
        assert_eq!(subscriptions.total(), 6);

        rpc2.program_unsubscribe(sub_id2).unwrap();
        assert_eq!(subscriptions.total(), 5);

        rpc3.logs_unsubscribe(sub_id3).unwrap();
        assert_eq!(subscriptions.total(), 4);

        rpc4.signature_unsubscribe(sub_id4).unwrap();
        assert_eq!(subscriptions.total(), 3);

        rpc5.slot_unsubscribe(sub_id5).unwrap();
        assert_eq!(subscriptions.total(), 2);

        rpc6.vote_unsubscribe(sub_id6).unwrap();
        assert_eq!(subscriptions.total(), 1);

        rpc7.root_unsubscribe(sub_id7).unwrap();
        assert_eq!(subscriptions.total(), 0);
    }
}
