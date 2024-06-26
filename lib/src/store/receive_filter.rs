use super::error::Error;
use crate::{crypto::Hash, db, protocol::MultiBlockPresence};
use sqlx::Row;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::task;

/// Filter for received nodes to avoid processing a node that doesn't contain any new information
/// compared to the last time we received that same node.
pub(crate) struct ReceiveFilter {
    id: u64,
    db: db::Pool,
}

impl ReceiveFilter {
    pub(super) fn new(db: db::Pool) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);

        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            db,
        }
    }

    pub async fn reset(&self) -> Result<(), Error> {
        try_remove_all(&self.db, self.id).await
    }

    pub async fn check(
        &self,
        tx: &mut db::WriteTransaction,
        hash: &Hash,
        new_presence: &MultiBlockPresence,
    ) -> Result<bool, Error> {
        if let Some((row_id, old_presence)) = load(tx, self.id, hash).await? {
            if !old_presence.is_outdated(new_presence) {
                return Ok(false);
            }

            update(tx, row_id, new_presence).await?;
        } else {
            insert(tx, self.id, hash, new_presence).await?;
        }

        Ok(true)
    }

    pub async fn remove(&self, tx: &mut db::WriteTransaction, hash: &Hash) -> Result<(), Error> {
        sqlx::query("DELETE FROM received_nodes WHERE hash = ? AND client_id = ?")
            .bind(hash)
            .bind(db::encode_u64(self.id))
            .execute(tx)
            .await?;
        Ok(())
    }
}

impl Drop for ReceiveFilter {
    fn drop(&mut self) {
        task::spawn(remove_all(self.db.clone(), self.id));
    }
}

pub(super) async fn remove(tx: &mut db::WriteTransaction, hash: &Hash) -> Result<(), Error> {
    sqlx::query("DELETE FROM received_nodes WHERE hash = ?")
        .bind(hash)
        .execute(tx)
        .await?;
    Ok(())
}

async fn load(
    conn: &mut db::Connection,
    client_id: u64,
    hash: &Hash,
) -> Result<Option<(u64, MultiBlockPresence)>, Error> {
    let row = sqlx::query(
        "SELECT rowid, block_presence
         FROM received_nodes
         WHERE client_id = ? AND hash = ?",
    )
    .bind(db::encode_u64(client_id))
    .bind(hash)
    .fetch_optional(conn)
    .await?;

    let row = if let Some(row) = row {
        row
    } else {
        return Ok(None);
    };

    let id = db::decode_u64(row.get(0));
    let presence = row.get(1);

    Ok(Some((id, presence)))
}

async fn insert(
    conn: &mut db::Connection,
    client_id: u64,
    hash: &Hash,
    presence: &MultiBlockPresence,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO received_nodes
         (client_id, hash, block_presence)
         VALUES (?, ?, ?)",
    )
    .bind(db::encode_u64(client_id))
    .bind(hash)
    .bind(presence)
    .execute(conn)
    .await?;

    Ok(())
}

async fn update(
    conn: &mut db::Connection,
    row_id: u64,
    presence: &MultiBlockPresence,
) -> Result<(), Error> {
    sqlx::query(
        "UPDATE received_nodes
         SET block_presence = ?
         WHERE rowid = ?",
    )
    .bind(presence)
    .bind(db::encode_u64(row_id))
    .execute(conn)
    .await?;

    Ok(())
}

async fn remove_all(pool: db::Pool, client_id: u64) {
    if let Err(error) = try_remove_all(&pool, client_id).await {
        tracing::error!(
            "Failed to cleanup ReceiveFilter(client_id: {}): {:?}",
            client_id,
            error
        );
    }
}

async fn try_remove_all(pool: &db::Pool, client_id: u64) -> Result<(), Error> {
    let mut tx = pool.begin_write().await?;
    sqlx::query("DELETE FROM received_nodes WHERE client_id = ?")
        .bind(db::encode_u64(client_id))
        .execute(&mut tx)
        .await?;
    tx.commit().await?;

    Ok(())
}
