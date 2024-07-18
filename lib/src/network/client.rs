use super::{
    constants::MAX_PENDING_REQUESTS_PER_CLIENT,
    debug_payload::{DebugResponse, PendingDebugRequest},
    message::{Content, Request, Response, ResponseDisambiguator},
    pending::{PendingRequest, PendingRequests, PendingResponse, ProcessedResponse},
};
use crate::{
    block_tracker::{BlockPromise, OfferState, TrackerClient},
    crypto::{sign::PublicKey, CacheHash, Hashable},
    error::{Error, Result},
    protocol::{
        Block, BlockId, InnerNodes, LeafNodes, MultiBlockPresence, RootNodeFilter, UntrustedProof,
    },
    repository::{BlockRequestMode, Vault},
    store,
};
use std::{future, sync::Arc, time::Instant};
use tokio::{
    select,
    sync::{mpsc, OwnedSemaphorePermit, Semaphore},
};
use tracing::{instrument, Level};

pub(super) struct Client {
    inner: Inner,
    response_rx: mpsc::Receiver<Response>,
    send_queue_rx: mpsc::UnboundedReceiver<(PendingRequest, Instant)>,
}

impl Client {
    pub fn new(
        vault: Vault,
        content_tx: mpsc::Sender<Content>,
        response_rx: mpsc::Receiver<Response>,
        peer_request_limiter: Arc<Semaphore>,
    ) -> Self {
        let pending_requests = PendingRequests::new(vault.monitor.clone());
        let block_tracker = vault.block_tracker.client();

        // We run the sender in a separate task so we can keep sending requests while we're
        // processing responses (which sometimes takes a while).
        let (send_queue_tx, send_queue_rx) = mpsc::unbounded_channel();

        let inner = Inner {
            vault,
            pending_requests,
            peer_request_limiter,
            link_request_limiter: Arc::new(Semaphore::new(MAX_PENDING_REQUESTS_PER_CLIENT)),
            block_tracker,
            content_tx,
            send_queue_tx,
        };

        Self {
            inner,
            response_rx,
            send_queue_rx,
        }
    }
}

impl Client {
    pub async fn run(&mut self) -> Result<()> {
        let Self {
            inner,
            response_rx,
            send_queue_rx,
        } = self;

        inner.run(response_rx, send_queue_rx).await
    }
}

struct Inner {
    vault: Vault,
    pending_requests: PendingRequests,
    peer_request_limiter: Arc<Semaphore>,
    link_request_limiter: Arc<Semaphore>,
    block_tracker: TrackerClient,
    content_tx: mpsc::Sender<Content>,
    send_queue_tx: mpsc::UnboundedSender<(PendingRequest, Instant)>,
}

impl Inner {
    async fn run(
        &mut self,
        response_rx: &mut mpsc::Receiver<Response>,
        send_queue_rx: &mut mpsc::UnboundedReceiver<(PendingRequest, Instant)>,
    ) -> Result<()> {
        select! {
            result = self.handle_responses(response_rx) => result,
            _ = self.send_requests(send_queue_rx) => Ok(()),
            _ = self.handle_available_block_offers() => Ok(()),
            _ = self.handle_reload_index() => Ok(()),
        }
    }

    fn enqueue_request(&self, request: PendingRequest) {
        self.send_queue_tx.send((request, Instant::now())).ok();
    }

    async fn send_requests(
        &self,
        send_queue_rx: &mut mpsc::UnboundedReceiver<(PendingRequest, Instant)>,
    ) {
        loop {
            let Some((request, timestamp)) = send_queue_rx.recv().await else {
                break;
            };

            let permits = self.acquire_send_permits().await;

            self.vault
                .monitor
                .request_queue_time
                .record(timestamp.elapsed());

            if let Some(request) = self
                .pending_requests
                .insert(request, permits.link, permits.peer)
            {
                self.send_request(request).await;
            }
        }
    }

    async fn send_request(&self, request: Request) {
        self.content_tx
            .send(Content::Request(request))
            .await
            .unwrap_or(());
    }

    async fn acquire_send_permits(&self) -> SendPermits {
        // Unwraps OK because we never `close()` the semaphores.
        //
        // NOTE that the order here is important, we don't want to block the other clients
        // on this peer if we have too many responses queued up (which is what the
        // `link_permit` is responsible for limiting)..
        let link = self
            .link_request_limiter
            .clone()
            .acquire_owned()
            .await
            .unwrap();

        let peer = self
            .peer_request_limiter
            .clone()
            .acquire_owned()
            .await
            .unwrap();

        SendPermits { link, peer }
    }

    async fn handle_responses(&self, rx: &mut mpsc::Receiver<Response>) -> Result<()> {
        while let Some(response) = rx.recv().await {
            self.vault.monitor.responses_received.increment(1);

            let response = self.pending_requests.remove(response);

            let start = Instant::now();
            self.handle_response(response).await?;
            self.vault
                .monitor
                .response_handle_time
                .record(start.elapsed());
        }

        Ok(())
    }

    async fn handle_response(&self, response: PendingResponse) -> Result<()> {
        match response.response {
            ProcessedResponse::RootNode(proof, block_presence, debug) => {
                self.handle_root_node(proof, block_presence, debug).await
            }
            ProcessedResponse::InnerNodes(nodes, _, debug) => {
                self.handle_inner_nodes(nodes, debug).await
            }
            ProcessedResponse::LeafNodes(nodes, _, debug) => {
                self.handle_leaf_nodes(nodes, debug).await
            }
            ProcessedResponse::BlockOffer(block_id, debug) => {
                self.handle_block_offer(block_id, debug).await
            }
            ProcessedResponse::Block(block, debug) => {
                self.handle_block(block, response.block_promise, debug)
                    .await
            }
            ProcessedResponse::BlockError(block_id, debug) => {
                self.handle_block_not_found(block_id, debug).await
            }
            ProcessedResponse::RootNodeError(..) | ProcessedResponse::ChildNodesError(..) => Ok(()),
        }
    }

    #[instrument(
        skip_all,
        fields(
            writer_id = ?proof.writer_id,
            vv = ?proof.version_vector,
            hash = ?proof.hash,
            ?block_presence,
            ?debug_payload,
        ),
        err(Debug)
    )]
    async fn handle_root_node(
        &self,
        proof: UntrustedProof,
        block_presence: MultiBlockPresence,
        debug_payload: DebugResponse,
    ) -> Result<()> {
        let hash = proof.hash;
        let status = self.vault.receive_root_node(proof, block_presence).await?;

        if status.request_children {
            self.enqueue_request(PendingRequest::ChildNodes(
                hash,
                ResponseDisambiguator::new(block_presence),
                debug_payload.follow_up(),
            ));
        }

        if status.new_snapshot {
            tracing::debug!("Received root node - new snapshot");
        } else if status.request_children {
            tracing::debug!("Received root node - new blocks");
        } else {
            tracing::trace!("Received root node - outdated");
        }

        self.log_approved(&status.new_approved).await;

        Ok(())
    }

    #[instrument(skip_all, fields(hash = ?nodes.hash(), ?debug_payload), err(Debug))]
    async fn handle_inner_nodes(
        &self,
        nodes: CacheHash<InnerNodes>,
        debug_payload: DebugResponse,
    ) -> Result<()> {
        let total = nodes.len();

        let quota = self.vault.quota().await?.map(Into::into);
        let status = self.vault.receive_inner_nodes(nodes, quota).await?;

        let debug = debug_payload.follow_up();

        tracing::trace!(
            "Received {}/{} inner nodes: {:?}",
            status.request_children.len(),
            total,
            status
                .request_children
                .iter()
                .map(|node| &node.hash)
                .collect::<Vec<_>>()
        );

        for node in status.request_children {
            self.enqueue_request(PendingRequest::ChildNodes(
                node.hash,
                ResponseDisambiguator::new(node.summary.block_presence),
                debug.clone(),
            ));
        }

        if quota.is_some() {
            for branch_id in &status.new_approved {
                self.vault.approve_offers(branch_id).await?;
            }
        }

        self.refresh_branches(status.new_approved.iter().copied());
        self.log_approved(&status.new_approved).await;

        Ok(())
    }

    #[instrument(skip_all, fields(hash = ?nodes.hash(), ?debug_payload), err(Debug))]
    async fn handle_leaf_nodes(
        &self,
        nodes: CacheHash<LeafNodes>,
        debug_payload: DebugResponse,
    ) -> Result<()> {
        let total = nodes.len();
        let quota = self.vault.quota().await?.map(Into::into);
        let status = self.vault.receive_leaf_nodes(nodes, quota).await?;

        tracing::trace!(
            "Received {}/{} leaf nodes: {:?}",
            status.request_blocks.len(),
            total,
            status
                .request_blocks
                .iter()
                .map(|node| &node.block_id)
                .collect::<Vec<_>>(),
        );

        let offer_state =
            if quota.is_none() || !status.new_approved.is_empty() || status.old_approved {
                OfferState::Approved
            } else {
                OfferState::Pending
            };

        match self.vault.block_request_mode {
            BlockRequestMode::Lazy => {
                for node in status.request_blocks {
                    self.block_tracker.register(node.block_id, offer_state);
                }
            }
            BlockRequestMode::Greedy => {
                for node in status.request_blocks {
                    if self.block_tracker.register(node.block_id, offer_state) {
                        self.vault.block_tracker.require(node.block_id);
                    }
                }
            }
        }

        if quota.is_some() {
            for branch_id in &status.new_approved {
                self.vault.approve_offers(branch_id).await?;
            }
        }

        self.refresh_branches(status.new_approved.iter().copied());
        self.log_approved(&status.new_approved).await;

        Ok(())
    }

    #[instrument(skip_all, fields(id = ?block_id, ?debug_payload), err(Debug))]
    async fn handle_block_offer(
        &self,
        block_id: BlockId,
        debug_payload: DebugResponse,
    ) -> Result<()> {
        let Some(offer_state) = self.vault.offer_state(&block_id).await? else {
            return Ok(());
        };

        tracing::trace!(?offer_state, "Received block offer");

        if !self.block_tracker.register(block_id, offer_state) {
            return Ok(());
        }

        match self.vault.block_request_mode {
            BlockRequestMode::Lazy => (),
            BlockRequestMode::Greedy => {
                self.vault.block_tracker.require(block_id);
            }
        }

        Ok(())
    }

    #[instrument(skip_all, fields(id = ?block.id, ?debug_payload), err(Debug))]
    async fn handle_block(
        &self,
        block: Block,
        block_promise: Option<BlockPromise>,
        debug_payload: DebugResponse,
    ) -> Result<()> {
        tracing::trace!("Received block");

        match self.vault.receive_block(&block, block_promise).await {
            // Ignore `BlockNotReferenced` errors as they only mean that the block is no longer
            // needed.
            Ok(()) | Err(Error::Store(store::Error::BlockNotReferenced)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    #[instrument(skip_all, fields(block_id, debug_payload = ?_debug_payload), err(Debug))]
    async fn handle_block_not_found(
        &self,
        block_id: BlockId,
        _debug_payload: DebugResponse,
    ) -> Result<()> {
        tracing::trace!("Received block not found {:?}", block_id);
        Ok(())
    }

    async fn handle_available_block_offers(&self) {
        let mut block_offers = self.block_tracker.offers();

        loop {
            let block_offer = block_offers.next().await;
            let debug = PendingDebugRequest::start();
            self.enqueue_request(PendingRequest::Block(block_offer, debug));
        }
    }

    async fn handle_reload_index(&self) {
        let mut reload_index_rx = self.vault.store().client_reload_index_tx.subscribe();

        while let Ok(branch_ids) = reload_index_rx.changed().await {
            self.refresh_branches(branch_ids);
        }

        future::pending().await
    }

    // Request again the branches that became completed. This is to cover the following edge
    // case:
    //
    // A block is initially present, but is part of a an outdated file/directory. A new snapshot
    // is in the process of being downloaded from a remote replica. During this download, the
    // block is still present and so is not marked as offered (because at least one of its
    // local ancestor nodes is still seen as up-to-date). Then before the download completes,
    // the worker garbage-collects the block. Then the download completes and triggers another
    // worker run. During this run the block might be marked as required again (because e.g.
    // the file was modified by the remote replica). But the block hasn't been marked as
    // offered (because it was still present during the last snapshot download) and so is not
    // requested. We would now have to wait for the next snapshot update from the remote replica
    // before the block is marked as offered and only then we proceed with requesting it. This
    // can take arbitrarily long (even indefinitely).
    //
    // By requesting the root node again immediatelly, we ensure that the missing block is
    // requested as soon as possible.
    fn refresh_branches(&self, branches: impl IntoIterator<Item = PublicKey>) {
        for branch_id in branches {
            self.enqueue_request(PendingRequest::RootNode(
                branch_id,
                PendingDebugRequest::start(),
            ));
        }
    }

    /// Log new approved snapshots
    async fn log_approved(&self, branches: &[PublicKey]) {
        if !tracing::enabled!(Level::DEBUG) {
            return;
        }

        if branches.is_empty() {
            return;
        }

        let mut reader = match self.vault.store().acquire_read().await {
            Ok(reader) => reader,
            Err(error) => {
                tracing::error!(?error, "Failed to acquire reader");
                return;
            }
        };

        for branch_id in branches {
            let root_node = match reader.load_root_node(branch_id, RootNodeFilter::Any).await {
                Ok(root_node) => root_node,
                Err(error) => {
                    tracing::error!(?branch_id, ?error, "Failed to load root node");
                    continue;
                }
            };

            tracing::debug!(
                writer_id = ?root_node.proof.writer_id,
                hash = ?root_node.proof.hash,
                vv = ?root_node.proof.version_vector,
                block_presence = ?root_node.summary.block_presence,
                "Snapshot complete"
            );
        }
    }
}

struct SendPermits {
    peer: OwnedSemaphorePermit,
    link: OwnedSemaphorePermit,
}
