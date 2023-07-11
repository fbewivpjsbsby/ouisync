mod node;
mod proof;
mod receive_filter;
#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use self::node::test_utils as node_test_utils;
pub(crate) use self::{
    node::{
        receive_block, update_summaries, MultiBlockPresence, NodeState, SingleBlockPresence,
        Summary, UpdateSummaryReason,
    },
    proof::{Proof, UntrustedProof},
    receive_filter::ReceiveFilter,
};

use self::proof::ProofError;
use crate::{
    block::BlockId,
    crypto::{sign::PublicKey, CacheHash, Hash, Hashable},
    db,
    debug::DebugPrinter,
    error::{Error, Result},
    event::{Event, EventSender, Payload},
    future::try_collect_into,
    repository::RepositoryId,
    storage_size::StorageSize,
    store::{
        self, InnerNode, InnerNodeMap, LeafNode, LeafNodeSet, RootNode, Store, WriteTransaction,
    },
    version_vector::VersionVector,
};
use futures_util::TryStreamExt;
use std::cmp::Ordering;
use thiserror::Error;
use tokio::sync::broadcast;
use tracing::Level;

pub(crate) type SnapshotId = u32;

#[derive(Clone)]
pub(crate) struct Index {
    store: Store,
    repository_id: RepositoryId,
    event_tx: EventSender,
}

impl Index {
    pub fn new(store: Store, repository_id: RepositoryId, event_tx: EventSender) -> Self {
        Self {
            store,
            repository_id,
            event_tx,
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    #[deprecated = "use store"]
    pub fn db(&self) -> &db::Pool {
        self.store.raw()
    }

    pub fn repository_id(&self) -> &RepositoryId {
        &self.repository_id
    }

    /// Subscribe to change notification from all current and future branches.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.event_tx.subscribe()
    }

    pub(crate) fn notify(&self) -> &EventSender {
        &self.event_tx
    }

    pub async fn debug_print(&self, print: DebugPrinter) {
        let mut reader = self.store().acquire_read().await.unwrap();
        RootNode::debug_print(reader.raw_mut(), print).await;
    }

    /// Receive `RootNode` from other replica and store it into the db. Returns whether the
    /// received node has any new information compared to all the nodes already stored locally.
    pub async fn receive_root_node(
        &self,
        proof: UntrustedProof,
        block_presence: MultiBlockPresence,
    ) -> Result<bool, ReceiveError> {
        let proof = proof.verify(self.repository_id())?;

        // Ignore branches with empty version vectors because they have no content yet.
        if proof.version_vector.is_empty() {
            return Ok(false);
        }

        // Make sure the loading of the existing nodes and the potential creation of the new node
        // happens atomically. Otherwise we could conclude the incoming node is up-to-date but
        // just before we start inserting it another snapshot might get created locally and the
        // incoming node might become outdated. But because we already concluded it's up-to-date,
        // we end up inserting it anyway which breaks the invariant that a node inserted later must
        // be happens-after any node inserted earlier in the same branch.
        let mut tx = self.store().begin_write().await?;

        // Determine further actions by comparing the incoming node against the existing nodes:
        let action = decide_root_node_action(&mut tx, &proof, &block_presence).await?;

        if action.insert {
            let (node, status) = tx.receive_root_node(proof).await?;

            tracing::debug!(
                branch_id = ?node.proof.writer_id,
                hash = ?node.proof.hash,
                vv = ?node.proof.version_vector,
                "snapshot started"
            );

            self.finalize_receive(tx, &status).await?;
        }

        Ok(action.request_children)
    }

    /// Receive inner nodes from other replica and store them into the db.
    /// Returns hashes of those nodes that were more up to date than the locally stored ones.
    /// Also returns the receive status.
    pub async fn receive_inner_nodes(
        &self,
        nodes: CacheHash<InnerNodeMap>,
        receive_filter: &ReceiveFilter,
        quota: Option<StorageSize>,
    ) -> Result<(Vec<InnerNode>, ReceiveStatus), ReceiveError> {
        let mut tx = self.store().begin_write().await?;
        let parent_hash = nodes.hash();

        self.check_parent_node_exists(tx.raw_mut(), &parent_hash)
            .await?;

        let updated_nodes = self
            .find_inner_nodes_with_new_blocks(tx.raw_mut(), &nodes, receive_filter)
            .await?;

        let mut nodes = nodes.into_inner().into_incomplete();
        nodes.inherit_summaries(tx.raw_mut()).await?;
        nodes.save(tx.raw_mut(), &parent_hash).await?;

        let status = tx.finalize_receive(parent_hash, quota).await?;
        self.finalize_receive(tx, &status).await?;

        Ok((updated_nodes, status))
    }

    /// Receive leaf nodes from other replica and store them into the db.
    /// Returns the ids of the blocks that the remote replica has but the local one has not.
    /// Also returns the receive status.
    pub async fn receive_leaf_nodes(
        &self,
        nodes: CacheHash<LeafNodeSet>,
        quota: Option<StorageSize>,
    ) -> Result<(Vec<BlockId>, ReceiveStatus), ReceiveError> {
        let mut tx = self.store().begin_write().await?;
        let parent_hash = nodes.hash();

        self.check_parent_node_exists(tx.raw_mut(), &parent_hash)
            .await?;

        let updated_blocks = self
            .find_leaf_nodes_with_new_blocks(tx.raw_mut(), &nodes)
            .await?;

        nodes
            .into_inner()
            .into_missing()
            .save(tx.raw_mut(), &parent_hash)
            .await?;

        let status = tx.finalize_receive(parent_hash, quota).await?;
        self.finalize_receive(tx, &status).await?;

        Ok((updated_blocks, status))
    }

    // Filter inner nodes that the remote replica has some blocks in that the local one is missing.
    //
    // Assumes (but does not enforce) that `parent_hash` is the parent hash of all nodes in
    // `remote_nodes`.
    async fn find_inner_nodes_with_new_blocks(
        &self,
        tx: &mut db::WriteTransaction,
        remote_nodes: &InnerNodeMap,
        receive_filter: &ReceiveFilter,
    ) -> Result<Vec<InnerNode>> {
        let mut output = Vec::with_capacity(remote_nodes.len());

        for (_, remote_node) in remote_nodes {
            if !receive_filter
                .check(tx, &remote_node.hash, &remote_node.summary.block_presence)
                .await?
            {
                continue;
            }

            let local_node = InnerNode::load(tx, &remote_node.hash).await?;
            let insert = if let Some(local_node) = local_node {
                local_node.summary.is_outdated(&remote_node.summary)
            } else {
                // node not present locally - we implicitly treat this as if the local replica
                // had zero blocks under this node unless the remote node is empty, in that
                // case we ignore it.
                !remote_node.is_empty()
            };

            if insert {
                output.push(*remote_node);
            }
        }

        Ok(output)
    }

    // Filter leaf nodes that the remote replica has a block for but the local one is missing it.
    //
    // Assumes (but does not enforce) that `parent_hash` is the parent hash of all nodes in
    // `remote_nodes`.
    async fn find_leaf_nodes_with_new_blocks(
        &self,
        conn: &mut db::Connection,
        remote_nodes: &LeafNodeSet,
    ) -> Result<Vec<BlockId>> {
        let mut output = Vec::new();

        for remote_node in remote_nodes.present() {
            if !LeafNode::is_present(conn, &remote_node.block_id).await? {
                output.push(remote_node.block_id);
            }
        }

        Ok(output)
    }

    // Finalizes receiving nodes from a remote replica, commits the transaction and notifies the
    // affected branches.
    async fn finalize_receive(
        &self,
        mut tx: WriteTransaction,
        status: &ReceiveStatus,
    ) -> Result<()> {
        // For logging completed snapshots
        let root_nodes = if tracing::enabled!(Level::DEBUG) {
            let mut root_nodes = Vec::with_capacity(status.new_approved.len());

            for branch_id in &status.new_approved {
                root_nodes.push(tx.load_root_node(branch_id).await?);
            }

            root_nodes
        } else {
            Vec::new()
        };

        tx.commit_and_then({
            let new_approved = status.new_approved.clone();
            let event_tx = self.notify().clone();

            move || {
                for root_node in root_nodes {
                    tracing::debug!(
                        branch_id = ?root_node.proof.writer_id,
                        hash = ?root_node.proof.hash,
                        vv = ?root_node.proof.version_vector,
                        "snapshot complete"
                    );
                }

                for branch_id in new_approved {
                    event_tx.send(Payload::BranchChanged(branch_id));
                }
            }
        })
        .await?;

        Ok(())
    }

    async fn check_parent_node_exists(
        &self,
        conn: &mut db::Connection,
        hash: &Hash,
    ) -> Result<(), ReceiveError> {
        if node::parent_exists(conn, hash).await? {
            Ok(())
        } else {
            Err(ReceiveError::ParentNodeNotFound)
        }
    }
}

/// Status of receiving nodes from remote replica.
#[derive(Debug)]
pub(crate) struct ReceiveStatus {
    /// Whether any of the snapshots were already approved.
    pub old_approved: bool,
    /// List of branches whose snapshots have been approved.
    pub new_approved: Vec<PublicKey>,
}

#[derive(Debug, Error)]
pub(crate) enum ReceiveError {
    #[error("proof is invalid")]
    InvalidProof,
    #[error("parent node not found")]
    ParentNodeNotFound,
    #[error("fatal error")]
    Fatal(#[from] Error),
}

impl From<ProofError> for ReceiveError {
    fn from(_: ProofError) -> Self {
        Self::InvalidProof
    }
}

impl From<sqlx::Error> for ReceiveError {
    fn from(error: sqlx::Error) -> Self {
        Self::from(Error::from(error))
    }
}

impl From<store::Error> for ReceiveError {
    fn from(error: store::Error) -> Self {
        Self::from(Error::from(error))
    }
}

/// Operation on version vector
#[derive(Clone, Copy, Debug)]
pub(crate) enum VersionVectorOp<'a> {
    IncrementLocal,
    Merge(&'a VersionVector),
}

impl VersionVectorOp<'_> {
    pub fn apply(self, local_id: &PublicKey, target: &mut VersionVector) {
        match self {
            Self::IncrementLocal => {
                target.increment(*local_id);
            }
            Self::Merge(other) => {
                target.merge(other);
            }
        }
    }
}

// Decide what to do with an incoming root node.
struct RootNodeAction {
    // Should we insert the incoming node to the db?
    insert: bool,
    // Should we request the children of the incoming node?
    request_children: bool,
}

async fn decide_root_node_action(
    tx: &mut WriteTransaction,
    new_proof: &Proof,
    new_block_presence: &MultiBlockPresence,
) -> Result<RootNodeAction> {
    let mut action = RootNodeAction {
        insert: true,
        request_children: true,
    };

    let mut old_nodes = tx.load_root_nodes_in_any_state();
    while let Some(old_node) = old_nodes.try_next().await? {
        match new_proof
            .version_vector
            .partial_cmp(&old_node.proof.version_vector)
        {
            Some(Ordering::Less) => {
                // The incoming node is outdated compared to at least one existing node - discard
                // it.
                action.insert = false;
                action.request_children = false;
            }
            Some(Ordering::Equal) => {
                // The incoming node has the same version vector as one of the existing nodes.
                // If the hashes are also equal, there is no point inserting it but if the incoming
                // summary is potentially more up-to-date than the exising one, we still want to
                // request the children. Otherwise we discard it.
                if new_proof.hash == old_node.proof.hash {
                    action.insert = false;

                    // NOTE: `is_outdated` is not antisymmetric, so we can't replace this condition
                    // with `new_summary.is_outdated(&old_node.summary)`.
                    if !old_node
                        .summary
                        .block_presence
                        .is_outdated(new_block_presence)
                    {
                        action.request_children = false;
                    }
                } else {
                    // NOTE: Currently it's possible for two branches to have the same vv but
                    // different hash so we need to accept them.
                    // TODO: When https://github.com/equalitie/ouisync/issues/113 is fixed we can
                    // reject them.
                    tracing::trace!(
                        vv = ?old_node.proof.version_vector,
                        old_hash = ?old_node.proof.hash,
                        new_hash = ?new_proof.hash,
                        "received root node with same vv but different hash"
                    );
                }
            }
            Some(Ordering::Greater) => (),
            None => {
                if new_proof.writer_id == old_node.proof.writer_id {
                    tracing::warn!(
                        old_vv = ?old_node.proof.version_vector,
                        new_vv = ?new_proof.version_vector,
                        writer_id = ?new_proof.writer_id,
                        "received root node invalid: broken invariant - concurrency within branch is not allowed"
                    );

                    action.insert = false;
                    action.request_children = false;
                }
            }
        }

        if !action.insert && !action.request_children {
            break;
        }
    }

    Ok(action)
}
