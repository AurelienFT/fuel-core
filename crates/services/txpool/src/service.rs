use crate::{
    ports::{
        BlockImporter,
        PeerToPeer,
        TxPoolDb,
    },
    transaction_selector::select_transactions,
    Config,
    Error as TxPoolError,
    TxPool,
};
use fuel_core_services::{
    stream::BoxStream,
    RunnableService,
    RunnableTask,
    ServiceRunner,
    StateWatcher,
};
use fuel_core_types::{
    fuel_tx::{
        Transaction,
        TxId,
    },
    fuel_types::Bytes32,
    services::{
        block_importer::ImportResult,
        p2p::{
            GossipData,
            TransactionGossipData,
        },
        txpool::{
            ArcPoolTx,
            InsertionResult,
            TxInfo,
            TxStatus,
        },
    },
};
use parking_lot::Mutex as ParkingMutex;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;

pub type Service<P2P, DB> = ServiceRunner<Task<P2P, DB>>;

#[derive(Clone)]
pub struct TxStatusChange {
    status_sender: broadcast::Sender<TxStatus>,
    update_sender: broadcast::Sender<TxUpdate>,
}

impl TxStatusChange {
    pub fn new(capacity: usize) -> Self {
        let (status_sender, _) = broadcast::channel(capacity);
        let (update_sender, _) = broadcast::channel(capacity);
        Self {
            status_sender,
            update_sender,
        }
    }

    pub fn send_complete(&self, id: Bytes32) {
        let _ = self.status_sender.send(TxStatus::Completed);
        self.updated(id);
    }

    pub fn send_submitted(&self, id: Bytes32) {
        let _ = self.status_sender.send(TxStatus::Submitted);
        self.updated(id);
    }

    pub fn send_squeezed_out(&self, id: Bytes32, reason: TxPoolError) {
        let _ = self.status_sender.send(TxStatus::SqueezedOut {
            reason: reason.clone(),
        });
        let _ = self.update_sender.send(TxUpdate::squeezed_out(id, reason));
    }

    fn updated(&self, id: Bytes32) {
        let _ = self.update_sender.send(TxUpdate::updated(id));
    }
}

pub struct SharedState<P2P, DB> {
    tx_status_sender: TxStatusChange,
    txpool: Arc<ParkingMutex<TxPool<DB>>>,
    p2p: Arc<P2P>,
}

impl<P2P, DB> Clone for SharedState<P2P, DB> {
    fn clone(&self) -> Self {
        Self {
            tx_status_sender: self.tx_status_sender.clone(),
            txpool: self.txpool.clone(),
            p2p: self.p2p.clone(),
        }
    }
}

pub struct Task<P2P, DB> {
    gossiped_tx_stream: BoxStream<TransactionGossipData>,
    committed_block_stream: BoxStream<Arc<ImportResult>>,
    shared: SharedState<P2P, DB>,
}

#[async_trait::async_trait]
impl<P2P, DB> RunnableService for Task<P2P, DB>
where
    P2P: Send + Sync,
    DB: TxPoolDb,
{
    const NAME: &'static str = "TxPool";

    type SharedData = SharedState<P2P, DB>;
    type Task = Task<P2P, DB>;

    fn shared_data(&self) -> Self::SharedData {
        self.shared.clone()
    }

    async fn into_task(self, _: &StateWatcher) -> anyhow::Result<Self::Task> {
        Ok(self)
    }
}

#[async_trait::async_trait]
impl<P2P, DB> RunnableTask for Task<P2P, DB>
where
    P2P: Send + Sync,
    DB: TxPoolDb,
{
    async fn run(&mut self, watcher: &mut StateWatcher) -> anyhow::Result<bool> {
        let should_continue;
        tokio::select! {
            _ = watcher.while_started() => {
                should_continue = false;
            }
            new_transaction = self.gossiped_tx_stream.next() => {
                if let Some(GossipData { data: Some(tx), .. }) = new_transaction {
                    let txs = vec!(Arc::new(tx));
                    self.shared.txpool.lock().insert(
                        &self.shared.tx_status_sender,
                        &txs
                    );
                    should_continue = true;
                } else {
                    should_continue = false;
                }
            }

            result = self.committed_block_stream.next() => {
                if let Some(result) = result {
                    self.shared.txpool.lock().block_update(&self.shared.tx_status_sender, &result.sealed_block);
                    should_continue = true;
                } else {
                    should_continue = false;
                }
            }
        }
        Ok(should_continue)
    }
}

// TODO: Remove `find` and `find_one` methods from `txpool`. It is used only by GraphQL.
//  Instead, `fuel-core` can create a `DatabaseWithTxPool` that aggregates `TxPool` and
//  storage `Database` together. GraphQL will retrieve data from this `DatabaseWithTxPool` via
//  `StorageInspect` trait.
impl<P2P, DB> SharedState<P2P, DB>
where
    DB: TxPoolDb,
{
    pub fn pending_number(&self) -> usize {
        self.txpool.lock().pending_number()
    }

    pub fn total_consumable_gas(&self) -> u64 {
        self.txpool.lock().consumable_gas()
    }

    pub fn remove_txs(&self, ids: Vec<TxId>) -> Vec<ArcPoolTx> {
        self.txpool.lock().remove(&self.tx_status_sender, &ids)
    }

    pub fn find(&self, ids: Vec<TxId>) -> Vec<Option<TxInfo>> {
        self.txpool.lock().find(&ids)
    }

    pub fn find_one(&self, id: TxId) -> Option<TxInfo> {
        self.txpool.lock().find_one(&id)
    }

    pub fn find_dependent(&self, ids: Vec<TxId>) -> Vec<ArcPoolTx> {
        self.txpool.lock().find_dependent(&ids)
    }

    pub fn select_transactions(&self, max_gas: u64) -> Vec<ArcPoolTx> {
        let mut guard = self.txpool.lock();
        let txs = guard.includable();
        let sorted_txs = select_transactions(txs, max_gas);

        for tx in sorted_txs.iter() {
            guard.remove_committed_tx(&tx.id());
        }
        sorted_txs
    }

    pub fn remove(&self, ids: Vec<TxId>) -> Vec<ArcPoolTx> {
        self.txpool.lock().remove(&self.tx_status_sender, &ids)
    }

    pub fn tx_status_subscribe(&self) -> broadcast::Receiver<TxStatus> {
        self.tx_status_sender.status_sender.subscribe()
    }

    pub fn tx_update_subscribe(&self) -> broadcast::Receiver<TxUpdate> {
        self.tx_status_sender.update_sender.subscribe()
    }
}

impl<P2P, DB> SharedState<P2P, DB>
where
    P2P: PeerToPeer<GossipedTransaction = TransactionGossipData>,
    DB: TxPoolDb,
{
    pub fn insert(
        &self,
        txs: Vec<Arc<Transaction>>,
    ) -> Vec<anyhow::Result<InsertionResult>> {
        let insert = { self.txpool.lock().insert(&self.tx_status_sender, &txs) };

        for (ret, tx) in insert.iter().zip(txs.into_iter()) {
            match ret {
                Ok(_) => {
                    let result = self.p2p.broadcast_transaction(tx.clone());
                    if let Err(e) = result {
                        // It can be only in the case of p2p being down or requests overloading it.
                        tracing::error!(
                            "Unable to broadcast transaction, got an {} error",
                            e
                        );
                    }
                }
                Err(_) => {}
            }
        }
        insert
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TxUpdate {
    tx_id: Bytes32,
    squeezed_out: Option<TxPoolError>,
}

impl TxUpdate {
    pub fn updated(tx_id: Bytes32) -> Self {
        Self {
            tx_id,
            squeezed_out: None,
        }
    }

    pub fn squeezed_out(tx_id: Bytes32, reason: TxPoolError) -> Self {
        Self {
            tx_id,
            squeezed_out: Some(reason),
        }
    }

    pub fn tx_id(&self) -> &Bytes32 {
        &self.tx_id
    }

    pub fn was_squeezed_out(&self) -> bool {
        self.squeezed_out.is_some()
    }

    pub fn into_squeezed_out_reason(self) -> Option<TxPoolError> {
        self.squeezed_out
    }
}

pub fn new_service<P2P, Importer, DB>(
    config: Config,
    db: DB,
    importer: Importer,
    p2p: P2P,
) -> Service<P2P, DB>
where
    Importer: BlockImporter,
    P2P: PeerToPeer<GossipedTransaction = TransactionGossipData> + 'static,
    DB: TxPoolDb + 'static,
{
    let p2p = Arc::new(p2p);
    let gossiped_tx_stream = p2p.gossiped_transaction_events();
    let committed_block_stream = importer.block_events();
    let txpool = Arc::new(ParkingMutex::new(TxPool::new(config, db)));
    let task = Task {
        gossiped_tx_stream,
        committed_block_stream,
        shared: SharedState {
            tx_status_sender: TxStatusChange::new(100),
            txpool,
            p2p,
        },
    };

    Service::new(task)
}

#[cfg(test)]
pub mod test_helpers;
#[cfg(test)]
pub mod tests;
#[cfg(test)]
pub mod tests_p2p;