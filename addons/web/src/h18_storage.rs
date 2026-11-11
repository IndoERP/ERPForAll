//! BTree storage envelope for the pinned Rust 1.98.1 insertion implementation.
//!
//! Source audit: `library/alloc/src/collections/btree/node.rs` (B, LeafNode,
//! InternalNode, splitpoint, insert, insert_recursing, push_internal_level) and
//! `map/entry.rs::VacantEntry::insert_entry`, from the installed 1.98.1 rust-src.
//! Re-audit on a toolchain change; the unit guard and allocation safety harness enforce it.
//!
//! No node is replaced during insert: splits retain the old left node and allocate a right
//! node, populate it, then ascend. Root promotion allocates one new empty root and immediately
//! inserts the promoted pair. Thus only one fresh empty allocation is pending at a time.
//! Completed nonroot nodes hold at least B-1=5 pairs. A split's transient four-pair side is
//! accompanied by an extracted promoted pair and the not-yet-inserted new pair; charging for
//! the final attempted entry count covers that temporary deficit. Consequently the nonempty
//! node count is at most 1+floor((N-1)/5); one additional node covers pending initialization.
//! For N<=11 no split can occur and one leaf suffices. Map::new and empty FromIterator
//! allocate no nodes.
//!
//! The same envelope also covers `BTreeMap::from_iter`'s new-tree bulk path. Audited
//! `map.rs::from_iter`/`bulk_build_from_sorted_iter`, `append.rs::bulk_push` and
//! `fix.rs::fix_right_border_of_plentiful`: before the final no-allocation steals, all nodes
//! off the right border are full (11 pairs); only its h+1 nodes can be underfull, with
//! h=floor(log_12(N)). Detached empty right subtrees replace a full right-spine suffix,
//! so this also counts their simultaneously live nodes during initialization. Therefore
//! nodes <= floor(N/11)+h+1. Since 12^h >= 11h+1, h <= (N-1)/11; this bound is no larger
//! than 2+floor((N-1)/5) for N>=12. For smaller nonempty inputs the bulk path uses one leaf.
//! The empty FromIterator path returns Map::new before allocating; direct private bulk-build
//! or public zero-length array construction is outside this empty-map guarantee.
//! FromIterator's input Vec and stable-sort scratch are separate reservations, not node bytes.
//!
//! These independent repr(C) envelopes include all key/value slots, parent header and edges,
//! rather than charging only initialized logical entries. Node-owned key/value heap contents
//! are separate reservations. The pinned compiler's node storage is checked on actual host
//! allocations, including split boundaries; this is admission safety, not a performance pin.

use std::{
    mem::{MaybeUninit, size_of},
    ptr::NonNull,
};

#[repr(C)]
struct LeafEnvelope<K, V> {
    parent: Option<NonNull<()>>,
    parent_index: MaybeUninit<u16>,
    length: u16,
    keys: [MaybeUninit<K>; 11],
    values: [MaybeUninit<V>; 11],
}
#[repr(C)]
struct InternalEnvelope<K, V> {
    leaf: LeafEnvelope<K, V>,
    edges: [MaybeUninit<NonNull<()>>; 12],
}

/// Heap node envelope for insert-only or nonempty FromIterator/new-tree bulk construction.
/// `entries` bounds attempted inserts, or all input pairs for FromIterator.
/// Duplicate replacements may be counted as additional attempts, conservatively. Contents of
/// owned keys/values are not included. Overflow fails before a caller allocates node storage.
pub(crate) fn btree_storage_bytes<K, V>(entries: usize) -> erp_sdk::Result<usize> {
    if entries == 0 {
        return Ok(0);
    }
    if entries <= 11 {
        return Ok(size_of::<LeafEnvelope<K, V>>());
    }
    let nodes = ((entries - 1) / 5)
        .checked_add(2)
        .ok_or_else(|| erp_sdk::Error::validation("H18 BTree storage size overflow"))?;
    nodes
        .checked_mul(size_of::<InternalEnvelope<K, V>>())
        .ok_or_else(|| erp_sdk::Error::validation("H18 BTree storage size overflow"))
}
