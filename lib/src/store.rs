//! Operation that affect both the index and the block store.

use crate::{
    block::{self, BlockData, BlockNonce},
    db,
    error::Result,
    index::{self, Index},
    progress::Progress,
};
use sqlx::{Connection, Row};

/// Write a block received from a remote replica to the block store. The block must already be
/// referenced by the index, otherwise an `BlockNotReferenced` error is returned.
pub(crate) async fn write_received_block(
    index: &Index,
    data: &BlockData,
    nonce: &BlockNonce,
) -> Result<()> {
    let mut cx = index.pool.acquire().await?;
    let mut tx = cx.begin().await?;

    let writer_ids = index::receive_block(&mut tx, &data.id).await?;
    block::write(&mut tx, &data.id, &data.content, nonce).await?;
    tx.commit().await?;

    let branches = index.branches().await;

    // Reload root nodes of the affected branches and notify them.
    for writer_id in &writer_ids {
        if let Some(branch) = branches.get(writer_id) {
            branch.reload_root(&mut cx).await?;
            branch.notify();
        }
    }

    Ok(())
}

/// Returns the syncing progress as the number of downloaded blocks / number of total blocks.
pub(crate) async fn sync_progress(conn: &mut db::Connection) -> Result<Progress> {
    // TODO: should we use `COUNT(DISTINCT ... )` ?
    Ok(sqlx::query(
        "SELECT
             COUNT(blocks.id),
             COUNT(snapshot_leaf_nodes.block_id)
         FROM
             snapshot_leaf_nodes
             LEFT JOIN blocks ON blocks.id = snapshot_leaf_nodes.block_id
         ",
    )
    .fetch_optional(conn)
    .await?
    .map(|row| Progress {
        value: db::decode_u64(row.get(0)),
        total: db::decode_u64(row.get(1)),
    })
    .unwrap_or(Progress { value: 0, total: 0 }))
}

/// Initialize database objects to support operations that affect both the index and the block
/// store.
pub(crate) async fn init(conn: &mut db::Connection) -> Result<()> {
    // Create trigger to delete orphaned blocks.
    sqlx::query(
        "CREATE TRIGGER IF NOT EXISTS blocks_delete_on_leaf_node_deleted
         AFTER DELETE ON snapshot_leaf_nodes
         WHEN NOT EXISTS (SELECT 0 FROM snapshot_leaf_nodes WHERE block_id = old.block_id)
         BEGIN
             DELETE FROM blocks WHERE id = old.block_id;
         END;

         CREATE TEMPORARY TRIGGER IF NOT EXISTS blocks_delete_on_leaf_node_deleted
         AFTER DELETE ON main.snapshot_leaf_nodes
         WHEN NOT EXISTS (SELECT 0 FROM main.snapshot_leaf_nodes WHERE block_id = old.block_id)
         BEGIN
             DELETE FROM missing_blocks WHERE block_id = old.block_id;
         END;
         ",
    )
    .execute(conn)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        block::{self, BlockId, BlockNonce, BLOCK_SIZE},
        crypto::{
            cipher::SecretKey,
            sign::{Keypair, PublicKey},
        },
        db,
        error::Error,
        index::{
            self,
            node_test_utils::{receive_blocks, receive_nodes, Snapshot},
            BranchData,
        },
        locator::Locator,
        repository::RepositoryId,
        sync::broadcast,
        version_vector::VersionVector,
    };
    use assert_matches::assert_matches;
    use sqlx::Connection;

    #[tokio::test(flavor = "multi_thread")]
    async fn remove_block() {
        let mut conn = setup().await.acquire().await.unwrap().detach();

        let read_key = SecretKey::random();
        let write_keys = Keypair::random();
        let notify_tx = broadcast::Sender::new(1);

        let branch0 = BranchData::create(
            &mut conn,
            PublicKey::random(),
            &write_keys,
            notify_tx.clone(),
        )
        .await
        .unwrap();
        let branch1 = BranchData::create(&mut conn, PublicKey::random(), &write_keys, notify_tx)
            .await
            .unwrap();

        let block_id = rand::random();
        let buffer = vec![0; BLOCK_SIZE];

        let mut tx = conn.begin().await.unwrap();

        block::write(&mut tx, &block_id, &buffer, &BlockNonce::default())
            .await
            .unwrap();

        let locator0 = Locator::head(rand::random());
        let locator0 = locator0.encode(&read_key);
        branch0
            .insert(&mut tx, &block_id, &locator0, &write_keys)
            .await
            .unwrap();

        let locator1 = Locator::head(rand::random());
        let locator1 = locator1.encode(&read_key);
        branch1
            .insert(&mut tx, &block_id, &locator1, &write_keys)
            .await
            .unwrap();

        assert!(block::exists(&mut tx, &block_id).await.unwrap());

        branch0
            .remove(&mut tx, &locator0, &write_keys)
            .await
            .unwrap();
        assert!(block::exists(&mut tx, &block_id).await.unwrap());

        branch1
            .remove(&mut tx, &locator1, &write_keys)
            .await
            .unwrap();
        assert!(!block::exists(&mut tx, &block_id).await.unwrap(),);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn receive_valid_blocks() {
        let pool = setup().await;

        let branch_id = PublicKey::random();
        let write_keys = Keypair::random();
        let repository_id = RepositoryId::from(write_keys.public);
        let index = Index::load(pool, repository_id).await.unwrap();

        let snapshot = Snapshot::generate(&mut rand::thread_rng(), 5);

        receive_nodes(
            &index,
            &write_keys,
            branch_id,
            VersionVector::first(branch_id),
            &snapshot,
        )
        .await;
        receive_blocks(&index, &snapshot).await;

        let mut conn = index.pool.acquire().await.unwrap();

        for (id, block) in snapshot.blocks() {
            let mut content = vec![0; BLOCK_SIZE];
            let nonce = block::read(&mut conn, id, &mut content).await.unwrap();

            assert_eq!(&content[..], &block.data.content[..]);
            assert_eq!(nonce, block.nonce);
            assert_eq!(BlockId::from_content(&content), *id);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn receive_orphaned_block() {
        let pool = setup().await;

        let repository_id = RepositoryId::random();
        let index = Index::load(pool, repository_id).await.unwrap();

        let snapshot = Snapshot::generate(&mut rand::thread_rng(), 1);

        for block in snapshot.blocks().values() {
            assert_matches!(
                write_received_block(&index, &block.data, &block.nonce).await,
                Err(Error::BlockNotReferenced)
            );
        }

        let mut conn = index.pool.acquire().await.unwrap();
        for id in snapshot.blocks().keys() {
            assert!(!block::exists(&mut conn, id).await.unwrap());
        }
    }

    async fn setup() -> db::Pool {
        let pool = db::open_or_create(&db::Store::Temporary).await.unwrap();
        let mut conn = pool.acquire().await.unwrap();

        index::init(&mut conn).await.unwrap();
        block::init(&mut conn).await.unwrap();
        super::init(&mut conn).await.unwrap();

        pool
    }
}
