//! Storage backends for the item sequence.
//!
//! The CRDT logic (in `replica.rs`) only talks to the [`Storage`] trait, so the
//! data structure can be swapped without touching the algorithm:
//!
//! - [`VecStorage`]: a plain `Vec<Item>`. Every operation is O(n). Obviously
//!   correct, so it is used as the test oracle.
//! - [`BlockStorage`]: items split into blocks of <= 128, with Fenwick trees over
//!   block lengths and visible counts plus a hash index from id to item. Lookups
//!   are O(log n + B) instead of O(n).

use crate::types::{Item, OpId};
use std::collections::HashMap;

pub trait Storage: Default + Clone {
    /// Number of items, including tombstones.
    fn len(&self) -> usize;
    /// Number of visible (non-deleted) items.
    fn visible_len(&self) -> usize;
    fn contains(&self, id: OpId) -> bool;
    fn get(&self, id: OpId) -> Option<&Item>;
    /// Raw index (counting tombstones) of the item with `id`. Panics if absent.
    fn raw_pos(&self, id: OpId) -> usize;
    fn item_at(&self, raw: usize) -> &Item;
    fn insert(&mut self, raw: usize, item: Item);
    /// Tombstone an item. Returns true if it was visible before.
    fn mark_deleted(&mut self, id: OpId) -> bool;
    /// Raw index of the `vis`-th visible item.
    fn raw_of_visible(&self, vis: usize) -> usize;
    /// Number of visible items in raw range `[0, raw)`.
    fn visible_before(&self, raw: usize) -> usize;
    fn text(&self) -> String;
}

// ---------------------------------------------------------------- VecStorage

#[derive(Clone, Default)]
pub struct VecStorage {
    items: Vec<Item>,
}

impl Storage for VecStorage {
    fn len(&self) -> usize {
        self.items.len()
    }
    fn visible_len(&self) -> usize {
        self.items.iter().filter(|i| !i.deleted).count()
    }
    fn contains(&self, id: OpId) -> bool {
        self.items.iter().any(|i| i.id == id)
    }
    fn get(&self, id: OpId) -> Option<&Item> {
        self.items.iter().find(|i| i.id == id)
    }
    fn raw_pos(&self, id: OpId) -> usize {
        self.items.iter().position(|i| i.id == id).expect("id present")
    }
    fn item_at(&self, raw: usize) -> &Item {
        &self.items[raw]
    }
    fn insert(&mut self, raw: usize, item: Item) {
        self.items.insert(raw, item);
    }
    fn mark_deleted(&mut self, id: OpId) -> bool {
        let i = self.raw_pos(id);
        let was_visible = !self.items[i].deleted;
        self.items[i].deleted = true;
        was_visible
    }
    fn raw_of_visible(&self, vis: usize) -> usize {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, i)| !i.deleted)
            .nth(vis)
            .map(|(r, _)| r)
            .expect("visible index in bounds")
    }
    fn visible_before(&self, raw: usize) -> usize {
        self.items[..raw].iter().filter(|i| !i.deleted).count()
    }
    fn text(&self) -> String {
        self.items.iter().filter(|i| !i.deleted).map(|i| i.ch).collect()
    }
}

// ---------------------------------------------------------------- Fenwick tree

/// Binary indexed tree over non-negative counts: point update, prefix sum and
/// "which element holds unit k" search, all in O(log n).
#[derive(Clone, Default)]
struct Fenwick {
    t: Vec<i64>, // 1-based; t[0] unused
}

impl Fenwick {
    fn build(vals: impl Iterator<Item = i64>) -> Self {
        let mut t = vec![0];
        t.extend(vals);
        let n = t.len() - 1;
        for i in 1..=n {
            let j = i + (i & i.wrapping_neg());
            if j <= n {
                t[j] += t[i];
            }
        }
        Fenwick { t }
    }

    fn n(&self) -> usize {
        self.t.len().saturating_sub(1)
    }

    fn add(&mut self, i: usize, d: i64) {
        let n = self.n();
        let mut i = i + 1;
        while i <= n {
            self.t[i] += d;
            i += i & i.wrapping_neg();
        }
    }

    /// Sum of the first `i` elements.
    fn prefix(&self, i: usize) -> i64 {
        let mut i = i;
        let mut s = 0;
        while i > 0 {
            s += self.t[i];
            i -= i & i.wrapping_neg();
        }
        s
    }

    /// Index of the element containing unit `k` (0-based), and k's offset inside it.
    fn find(&self, k: i64) -> (usize, i64) {
        let n = self.n();
        let mut pos = 0;
        let mut rem = k;
        let mut step = n.next_power_of_two();
        while step > 0 {
            let nx = pos + step;
            if nx <= n && self.t[nx] <= rem {
                pos = nx;
                rem -= self.t[nx];
            }
            step >>= 1;
        }
        (pos, rem)
    }
}

// ---------------------------------------------------------------- BlockStorage

const MAX_BLOCK: usize = 128;

#[derive(Clone, Default)]
pub struct BlockStorage {
    /// Arena of all items ever inserted (never moves).
    items: Vec<Item>,
    /// id -> arena index.
    index: HashMap<OpId, u32>,
    /// arena index -> block id.
    block_of: Vec<u32>,
    /// block id -> arena indices in document order.
    blocks: Vec<Vec<u32>>,
    /// block id -> visible items in that block.
    block_vis: Vec<u32>,
    /// Block ids in document order.
    order: Vec<u32>,
    /// block id -> its position in `order`.
    rank: Vec<u32>,
    /// Fenwick trees indexed by rank: items per block, visible items per block.
    len_tree: Fenwick,
    vis_tree: Fenwick,
    visible: usize,
}

impl BlockStorage {
    /// (rank, offset) of raw index `raw` (must be < len).
    fn locate(&self, raw: usize) -> (usize, usize) {
        let (rk, off) = self.len_tree.find(raw as i64);
        (rk, off as usize)
    }

    fn block_at_rank(&self, rk: usize) -> &[u32] {
        &self.blocks[self.order[rk] as usize]
    }

    fn new_block(&mut self, ids: Vec<u32>) -> u32 {
        let b = self.blocks.len() as u32;
        let vis = ids
            .iter()
            .filter(|&&a| !self.items[a as usize].deleted)
            .count() as u32;
        for &a in &ids {
            self.block_of[a as usize] = b;
        }
        self.blocks.push(ids);
        self.block_vis.push(vis);
        self.rank.push(0);
        b
    }

    /// Recompute ranks and Fenwick trees after the block order changed. O(#blocks).
    fn rebuild(&mut self) {
        for (rk, &b) in self.order.iter().enumerate() {
            self.rank[b as usize] = rk as u32;
        }
        self.len_tree = Fenwick::build(
            self.order
                .iter()
                .map(|&b| self.blocks[b as usize].len() as i64),
        );
        self.vis_tree =
            Fenwick::build(self.order.iter().map(|&b| self.block_vis[b as usize] as i64));
    }
}

impl Storage for BlockStorage {
    fn len(&self) -> usize {
        self.items.len()
    }

    fn visible_len(&self) -> usize {
        self.visible
    }

    fn contains(&self, id: OpId) -> bool {
        self.index.contains_key(&id)
    }

    fn get(&self, id: OpId) -> Option<&Item> {
        self.index.get(&id).map(|&a| &self.items[a as usize])
    }

    fn raw_pos(&self, id: OpId) -> usize {
        let a = self.index[&id];
        let b = self.block_of[a as usize] as usize;
        let rk = self.rank[b] as usize;
        let off = self.blocks[b]
            .iter()
            .position(|&x| x == a)
            .expect("item in its block");
        self.len_tree.prefix(rk) as usize + off
    }

    fn item_at(&self, raw: usize) -> &Item {
        let (rk, off) = self.locate(raw);
        &self.items[self.block_at_rank(rk)[off] as usize]
    }

    fn insert(&mut self, raw: usize, item: Item) {
        let total = self.items.len();
        let a = total as u32;
        let vis = !item.deleted;
        self.index.insert(item.id, a);
        self.items.push(item);
        self.block_of.push(0);
        if vis {
            self.visible += 1;
        }

        if self.order.is_empty() {
            let b = self.new_block(vec![a]);
            self.order.push(b);
            self.rebuild();
            return;
        }

        let (rk, off) = if raw >= total {
            let rk = self.order.len() - 1;
            (rk, self.block_at_rank(rk).len())
        } else {
            self.locate(raw)
        };
        let b = self.order[rk] as usize;
        self.blocks[b].insert(off, a);
        self.block_of[a as usize] = b as u32;
        self.len_tree.add(rk, 1);
        if vis {
            self.block_vis[b] += 1;
            self.vis_tree.add(rk, 1);
        }

        if self.blocks[b].len() > MAX_BLOCK {
            let half = self.blocks[b].len() / 2;
            let tail = self.blocks[b].split_off(half);
            let nb = self.new_block(tail);
            self.block_vis[b] -= self.block_vis[nb as usize];
            self.order.insert(rk + 1, nb);
            self.rebuild();
        }
    }

    fn mark_deleted(&mut self, id: OpId) -> bool {
        let a = self.index[&id] as usize;
        if self.items[a].deleted {
            return false;
        }
        self.items[a].deleted = true;
        let b = self.block_of[a] as usize;
        self.block_vis[b] -= 1;
        self.vis_tree.add(self.rank[b] as usize, -1);
        self.visible -= 1;
        true
    }

    fn raw_of_visible(&self, vis: usize) -> usize {
        assert!(vis < self.visible, "visible index in bounds");
        let (rk, rem) = self.vis_tree.find(vis as i64);
        let mut rem = rem;
        let block = self.block_at_rank(rk);
        for (off, &a) in block.iter().enumerate() {
            if !self.items[a as usize].deleted {
                if rem == 0 {
                    return self.len_tree.prefix(rk) as usize + off;
                }
                rem -= 1;
            }
        }
        unreachable!("vis_tree out of sync with block contents")
    }

    fn visible_before(&self, raw: usize) -> usize {
        if raw >= self.items.len() {
            return self.visible;
        }
        let (rk, off) = self.locate(raw);
        let in_block = self.block_at_rank(rk)[..off]
            .iter()
            .filter(|&&a| !self.items[a as usize].deleted)
            .count();
        self.vis_tree.prefix(rk) as usize + in_block
    }

    fn text(&self) -> String {
        let mut s = String::with_capacity(self.visible);
        for &b in &self.order {
            for &a in &self.blocks[b as usize] {
                let it = &self.items[a as usize];
                if !it.deleted {
                    s.push(it.ch);
                }
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fenwick_find_and_prefix() {
        let f = Fenwick::build([3, 0, 2, 5].into_iter());
        assert_eq!(f.prefix(0), 0);
        assert_eq!(f.prefix(4), 10);
        assert_eq!(f.find(0), (0, 0));
        assert_eq!(f.find(2), (0, 2));
        assert_eq!(f.find(3), (2, 0)); // skips the empty element
        assert_eq!(f.find(5), (3, 0));
        assert_eq!(f.find(9), (3, 4));
    }
}
