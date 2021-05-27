use super::{
    node::{InnerNode, LeafNode, LeafNodeSet, ModifyStatus},
    Crc, INNER_LAYER_COUNT, MAX_INNER_NODE_CHILD_COUNT,
};
use crate::{block::BlockId, crypto::Hash};
use crc::{crc32, Hasher32};
use sha3::{Digest, Sha3_256};

type InnerChildren = [InnerNode; MAX_INNER_NODE_CHILD_COUNT];

///
/// Path represents a (possibly incomplete) path in a snapshot from the root to the leaf.
/// Unlike a traditional tree path with only the relevant nodes, this one also contains for each
/// inner layer all siblings of the inner node that would be in the traditional path.
///
/// //                    root
/// //                    /  \
/// //                   a0   a1     |
/// //                  /  \         | inner: [[a0, a1], [b0, b1]]
/// //                 b0   b1       |
/// //                     /  \
/// //                    c0   c1    | leaves: [c0, c1]
///
/// The purpose of this is to be able to modify the path (complete it if it's incomplete, modify
/// and/or remove the leaf) and then recalculate all hashes.
///
#[derive(Debug)]
pub struct Path {
    locator: Hash,
    /// Count of the number of layers found where a locator has a corresponding bucket. Including
    /// the root and leaf layers.  (e.g. 0 -> root wasn't found; 1 -> root was found but no inner
    /// nor leaf layers was; 2 -> root and one inner (possibly leaf if INNER_LAYER_COUNT == 0)
    /// layers were found; ...)
    pub layers_found: usize,
    pub root: Hash,
    pub missing_blocks_crc: Crc,
    pub missing_blocks_count: usize,
    pub inner: Vec<InnerChildren>,
    pub leaves: LeafNodeSet,
}

impl Path {
    pub fn new(locator: Hash) -> Self {
        let null_hash = Hash::null();

        let inner = vec![[InnerNode::empty(); MAX_INNER_NODE_CHILD_COUNT]; INNER_LAYER_COUNT];

        Self {
            locator,
            layers_found: 0,
            root: null_hash,
            missing_blocks_crc: 0,
            missing_blocks_count: 0,
            inner,
            leaves: LeafNodeSet::default(),
        }
    }

    pub fn get_leaf(&self) -> Option<BlockId> {
        self.leaves.get(&self.locator).map(|node| node.block_id)
    }

    pub fn has_leaf(&self, block_id: &BlockId) -> bool {
        self.leaves.iter().any(|l| &l.block_id == block_id)
    }

    pub fn total_layer_count() -> usize {
        1 /* root */ + INNER_LAYER_COUNT + 1 /* leaves */
    }

    pub fn hash_at_layer(&self, layer: usize) -> Hash {
        if layer == 0 {
            return self.root;
        }
        let inner_layer = layer - 1;
        self.inner[inner_layer][self.get_bucket(inner_layer)].hash
    }

    // Sets the leaf node to the given block id. Returns the previous block id, if any.
    pub fn set_leaf(&mut self, block_id: &BlockId) -> Option<BlockId> {
        match self.leaves.modify(&self.locator, block_id) {
            ModifyStatus::Updated(old_block_id) => {
                self.recalculate(INNER_LAYER_COUNT);
                Some(old_block_id)
            }
            ModifyStatus::Inserted => {
                self.recalculate(INNER_LAYER_COUNT);
                None
            }
            ModifyStatus::Unchanged => None,
        }
    }

    pub fn remove_leaf(&mut self, locator: &Hash) -> Option<BlockId> {
        let block_id = self.leaves.remove(locator)?.block_id;

        if !self.leaves.is_empty() {
            self.recalculate(INNER_LAYER_COUNT);
        } else if INNER_LAYER_COUNT > 0 {
            self.remove_from_inner_layer(INNER_LAYER_COUNT - 1);
        } else {
            self.remove_root_layer();
        }

        Some(block_id)
    }

    pub fn get_bucket(&self, inner_layer: usize) -> usize {
        self.locator.as_ref()[inner_layer] as usize
    }

    fn remove_from_inner_layer(&mut self, inner_layer: usize) {
        let null = Hash::null();
        let bucket = self.get_bucket(inner_layer);

        self.inner[inner_layer][bucket] = InnerNode::empty();

        let is_empty = self.inner[inner_layer].iter().all(|x| x.hash == null);

        if !is_empty {
            self.recalculate(inner_layer);
            return;
        }

        if inner_layer > 0 {
            self.remove_from_inner_layer(inner_layer - 1);
        } else {
            self.remove_root_layer();
        }
    }

    fn remove_root_layer(&mut self) {
        self.root = Hash::null();
    }

    /// Recalculate layers from start_layer all the way to the root.
    fn recalculate(&mut self, start_layer: usize) {
        for inner_layer in (0..start_layer).rev() {
            let (hash, crc, cnt) = self.compute_hash_for_layer(inner_layer + 1);
            let bucket = self.get_bucket(inner_layer);
            self.inner[inner_layer][bucket] = InnerNode {
                hash,
                is_complete: true,
                missing_blocks_crc: crc,
                missing_blocks_count: cnt,
            };
        }

        let (hash, crc, cnt) = self.compute_hash_for_layer(0);
        self.root = hash;
        self.missing_blocks_crc = crc;
        self.missing_blocks_count = cnt;
    }

    // Assumes layers higher than `layer` have their hashes/BlockVersions already
    // computed/assigned.
    fn compute_hash_for_layer(&self, layer: usize) -> (Hash, Crc, usize) {
        if layer == INNER_LAYER_COUNT {
            let (crc, cnt) = calculate_missing_blocks_crc_from_leaves(self.leaves.as_slice());
            (hash_leaves(self.leaves.as_slice()), crc, cnt)
        } else {
            let (crc, cnt) = calculate_missing_blocks_crc_from_inner(&self.inner[layer]);
            (hash_inner(&self.inner[layer]), crc, cnt)
        }
    }
}

fn hash_leaves(leaves: &[LeafNode]) -> Hash {
    let mut hash = Sha3_256::new();
    // XXX: Is updating with length enough to prevent attaks?
    hash.update((leaves.len() as u32).to_le_bytes());
    for l in leaves {
        hash.update(l.locator());
        hash.update(l.block_id);
    }
    hash.finalize().into()
}

fn hash_inner(siblings: &[InnerNode]) -> Hash {
    // XXX: Have some cryptographer check this whether there are no attacks.
    let mut hash = Sha3_256::new();
    for (k, s) in siblings.iter().enumerate() {
        if !s.hash.is_null() {
            hash.update((k as u16).to_le_bytes());
            hash.update(s.hash);
        }
    }
    hash.finalize().into()
}

fn calculate_missing_blocks_crc_from_leaves(leaves: &[LeafNode]) -> (Crc, usize) {
    let mut cnt = 0;

    if leaves.is_empty() {
        return (0, cnt);
    }

    let mut digest = crc32::Digest::new(crc32::IEEE);

    for l in leaves {
        if l.is_block_missing {
            cnt += 1;
            digest.write(&[1]);
        }
    }

    (digest.sum32(), cnt)
}

fn calculate_missing_blocks_crc_from_inner(inner: &[InnerNode]) -> (Crc, usize) {
    let mut cnt = 0;

    if inner.is_empty() {
        return (0, cnt);
    }

    let mut digest = crc32::Digest::new(crc32::IEEE);

    for n in inner {
        if n.missing_blocks_crc != 0 {
            cnt += 1;
            digest.write(n.missing_blocks_crc.to_le_bytes().as_ref());
        }
    }

    (digest.sum32(), cnt)
}