use crate::{Hash, Prefix};

use super::{InteriorNode, LeafNode, Node, NodeStore};

/// Recursively batch-upsert sorted, deduplicated entries into a tree backed by `store`.
/// Returns the prefix of the (possibly new) root of this subtree.
pub(crate) fn batch_upsert_recursive<S: NodeStore>(
    store: &S,
    current_prefix: Prefix,
    entries: Vec<(Hash, Hash)>,
) -> Prefix {
    if entries.is_empty() {
        return current_prefix;
    }

    let Some(node) = store.get_node(&current_prefix) else {
        return batch_insert_into_empty(store, entries);
    };

    match node {
        Node::Leaf(leaf) => batch_upsert_at_leaf(store, current_prefix, leaf, entries),
        Node::Interior(interior) => {
            batch_upsert_at_interior(store, current_prefix, interior, entries)
        }
    }
}

/// Insert all entries into an empty subtree.
fn batch_insert_into_empty<S: NodeStore>(
    store: &S,
    mut entries: Vec<(Hash, Hash)>,
) -> Prefix {
    if entries.is_empty() {
        return Prefix::root();
    }

    let (first_key, first_value) = entries.remove(0);
    let first_prefix = Prefix::from(first_key);
    let first_leaf = LeafNode::new(first_key, first_value);
    store.set_node(first_prefix, Node::Leaf(first_leaf));

    batch_upsert_recursive(store, first_prefix, entries)
}

/// Batch upsert at a leaf node.
fn batch_upsert_at_leaf<S: NodeStore>(
    store: &S,
    leaf_prefix: Prefix,
    leaf: LeafNode,
    mut entries: Vec<(Hash, Hash)>,
) -> Prefix {
    // Check if any entry updates this leaf (binary search since entries are sorted)
    if let Ok(idx) = entries.binary_search_by_key(&leaf_prefix.hash, |(k, _)| *k) {
        let (_, new_value) = entries.remove(idx);
        let updated_leaf = LeafNode::new(leaf_prefix.hash, new_value);
        store.set_node(leaf_prefix, Node::Leaf(updated_leaf));

        if entries.is_empty() {
            return leaf_prefix;
        }
        return batch_upsert_recursive(store, leaf_prefix, entries);
    }

    if entries.is_empty() {
        return leaf_prefix;
    }

    // Split: create interior node with existing leaf and first new entry
    let (first_key, first_value) = entries.remove(0);

    let new_leaf = LeafNode::new(first_key, first_value);
    let new_prefix = Prefix::from(first_key);
    let existing_prefix = leaf_prefix;
    let merged_prefix = Prefix::common_prefix(&existing_prefix, &new_prefix);

    let (left_prefix, right_prefix, left_hash, right_hash) = super::order_children(
        &merged_prefix,
        first_key,
        new_prefix,
        new_leaf.merkle_hash,
        existing_prefix,
        leaf.merkle_hash,
    );

    let new_interior = InteriorNode::new(
        merged_prefix,
        left_prefix,
        right_prefix,
        left_hash,
        right_hash,
    );

    store.set_node(merged_prefix, Node::Interior(new_interior));
    store.set_node(existing_prefix, Node::Leaf(leaf));
    store.set_node(new_prefix, Node::Leaf(new_leaf));

    batch_upsert_recursive(store, merged_prefix, entries)
}

/// Batch upsert at an interior node.
fn batch_upsert_at_interior<S: NodeStore>(
    store: &S,
    interior_prefix: Prefix,
    interior: InteriorNode,
    entries: Vec<(Hash, Hash)>,
) -> Prefix {
    // Partition entries: those that belong under this node vs. those that diverge
    let mut contained_entries = Vec::new();
    let mut divergent_entries = Vec::new();

    for &(key, value) in entries.iter() {
        if interior_prefix.contains(&key) {
            contained_entries.push((key, value));
        } else {
            divergent_entries.push((key, value));
        }
    }

    // Handle divergent entries first (they require creating a new parent)
    if !divergent_entries.is_empty() {
        let (first_key, first_value) = divergent_entries.remove(0);

        let new_leaf = LeafNode::new(first_key, first_value);
        let new_leaf_prefix = Prefix::from(first_key);
        let common = Prefix::common_prefix(&interior_prefix, &new_leaf_prefix);

        let (left_prefix, right_prefix, left_hash, right_hash) = super::order_children(
            &common,
            first_key,
            new_leaf_prefix,
            new_leaf.merkle_hash,
            interior_prefix,
            interior.merkle_hash,
        );

        let new_interior =
            InteriorNode::new(common, left_prefix, right_prefix, left_hash, right_hash);

        store.set_node(common, Node::Interior(new_interior));
        store.set_node(new_leaf_prefix, Node::Leaf(new_leaf));

        // Merge remaining entries and continue
        contained_entries.extend(divergent_entries);
        return batch_upsert_recursive(store, common, contained_entries);
    }

    // All entries belong under this interior node — partition by left/right
    let mut left_entries = Vec::new();
    let mut right_entries = Vec::new();

    for &(key, value) in contained_entries.iter() {
        if interior_prefix.key_goes_right(key) {
            right_entries.push((key, value));
        } else {
            left_entries.push((key, value));
        }
    }

    // Recursively process left and right subtrees in parallel
    let (new_left, new_right) = rayon::join(
        || {
            if !left_entries.is_empty() {
                batch_upsert_recursive(store, interior.left, left_entries)
            } else {
                interior.left
            }
        },
        || {
            if !right_entries.is_empty() {
                batch_upsert_recursive(store, interior.right, right_entries)
            } else {
                interior.right
            }
        },
    );

    // Recalculate this interior node's hash based on updated children
    let left_hash = store
        .get_node(&new_left)
        .expect("left child missing after batch upsert")
        .merkle_hash();
    let right_hash = store
        .get_node(&new_right)
        .expect("right child missing after batch upsert")
        .merkle_hash();

    let updated_interior =
        InteriorNode::new(interior_prefix, new_left, new_right, left_hash, right_hash);

    store.set_node(interior_prefix, Node::Interior(updated_interior));
    interior_prefix
}
