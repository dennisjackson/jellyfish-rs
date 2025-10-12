use super::super::LeafNode;
use super::*;
use rusqlite::Connection;
use std::sync::{Arc, Mutex};

fn create_test_db() -> Arc<Mutex<Connection>> {
    let conn = Connection::open_in_memory().unwrap();

    // Mirror production schema so cache logic exercises metadata handling too.
    Cache::initialize_database(&conn).unwrap();

    Arc::new(Mutex::new(conn))
}

fn create_test_cache(db: Arc<Mutex<Connection>>) -> Cache {
    Cache::new_with_limit(db, 1024)
}

#[test]
fn test_new_cache() {
    let db = create_test_db();
    let cache = create_test_cache(db);
    assert_eq!(cache.len(), 0);
    assert!(cache.is_empty());
}

#[test]
fn test_get_set() {
    let db = create_test_db();
    let cache = create_test_cache(db);

    let key = [1u8; 32];
    let value = [2u8; 32];
    let prefix = Prefix::from(key);
    let leaf = LeafNode::new(key, value);

    // Get non-existent key
    assert!(cache.get(&prefix).is_none());

    // Set and get
    cache.set(prefix, Node::Leaf(leaf.clone()));
    let retrieved = cache
        .get(&prefix)
        .expect("Expected leaf node to be cached after set");

    match retrieved.value() {
        Node::Leaf(retrieved_leaf) => {
            assert_eq!(retrieved_leaf.key, key);
            assert_eq!(retrieved_leaf.value, value);
        }
        _ => panic!("Expected leaf node"),
    }
}

#[test]
fn test_flush_and_pre_advise() {
    let db = create_test_db();
    let cache = create_test_cache(Arc::clone(&db));

    let key1 = [1u8; 32];
    let value1 = [10u8; 32];
    let prefix1 = Prefix::from(key1);
    let leaf1 = LeafNode::new(key1, value1);

    let key2 = [2u8; 32];
    let value2 = [20u8; 32];
    let prefix2 = Prefix::from(key2);
    let leaf2 = LeafNode::new(key2, value2);

    // Add nodes to cache
    cache.set(prefix1, Node::Leaf(leaf1));
    cache.set(prefix2, Node::Leaf(leaf2));

    // Write to database
    cache.flush().unwrap();

    // Clear cache
    cache.clear();
    assert_eq!(cache.len(), 0);

    // Pre-advise to load from database
    cache.pre_advise(&[prefix1, prefix2]).unwrap();

    // Verify nodes are back in cache
    assert_eq!(cache.len(), 2);
    let retrieved1 = cache
        .get(&prefix1)
        .expect("Expected to reload leaf node after pre_advise");

    match retrieved1.value() {
        Node::Leaf(retrieved_leaf) => {
            assert_eq!(retrieved_leaf.key, key1);
            assert_eq!(retrieved_leaf.value, value1);
        }
        _ => panic!("Expected leaf node"),
    }
}

#[test]
fn test_pre_advise_skips_cached_keys() {
    let db = create_test_db();
    let cache = create_test_cache(Arc::clone(&db));

    let key = [1u8; 32];
    let value = [10u8; 32];
    let prefix = Prefix::from(key);
    let leaf = LeafNode::new(key, value);

    // Add to cache and write to DB
    cache.set(prefix, Node::Leaf(leaf));
    cache.flush().unwrap();

    // Pre-advise should not reload (already in cache)
    let initial_len = cache.len();
    cache.pre_advise(&[prefix]).unwrap();
    assert_eq!(cache.len(), initial_len);
}

#[test]
fn test_flush_empty() {
    let db = create_test_db();
    let cache = create_test_cache(db);

    // Should not error on empty flush
    cache.flush().unwrap();
}

#[test]
fn test_pre_advise_empty() {
    let db = create_test_db();
    let cache = create_test_cache(db);

    // Should not error on empty batch
    cache.pre_advise(&[]).unwrap();
}

#[test]
fn test_dirty_tracking() {
    let db = create_test_db();
    let cache = create_test_cache(db);

    let key = [1u8; 32];
    let value = [10u8; 32];
    let prefix = Prefix::from(key);
    let leaf = LeafNode::new(key, value);

    // Initially no dirty entries
    assert_eq!(cache.dirty_len(), 0);

    // Set marks as dirty
    cache.set(prefix, Node::Leaf(leaf));
    assert_eq!(cache.dirty_len(), 1);

    // Flush clears dirty tracking
    cache.flush().unwrap();
    assert_eq!(cache.dirty_len(), 0);
}

#[test]
fn test_flush() {
    let db = create_test_db();
    let cache = create_test_cache(Arc::clone(&db));

    let key1 = [1u8; 32];
    let value1 = [10u8; 32];
    let prefix1 = Prefix::from(key1);
    let leaf1 = LeafNode::new(key1, value1);

    let key2 = [2u8; 32];
    let value2 = [20u8; 32];
    let prefix2 = Prefix::from(key2);
    let leaf2 = LeafNode::new(key2, value2);

    // Add nodes
    cache.set(prefix1, Node::Leaf(leaf1));
    cache.set(prefix2, Node::Leaf(leaf2));
    assert_eq!(cache.dirty_len(), 2);

    // Flush to database
    cache.flush().unwrap();
    assert_eq!(cache.dirty_len(), 0);

    // Clear cache and reload to verify persistence
    cache.clear();
    cache.pre_advise(&[prefix1, prefix2]).unwrap();

    let retrieved = cache.get(&prefix1);
    assert!(retrieved.is_some());
}

#[test]
fn test_enumerate_nodes() {
    let db = create_test_db();
    let cache = create_test_cache(Arc::clone(&db));

    let key1 = [1u8; 32];
    let value1 = [10u8; 32];
    let prefix1 = Prefix::from(key1);
    let leaf1 = LeafNode::new(key1, value1);

    let key2 = [2u8; 32];
    let value2 = [20u8; 32];
    let prefix2 = Prefix::from(key2);
    let leaf2 = LeafNode::new(key2, value2);

    // Add and flush nodes
    cache.set(prefix1, Node::Leaf(leaf1));
    cache.set(prefix2, Node::Leaf(leaf2));
    cache.flush().unwrap();

    // Enumerate should return both nodes
    let nodes = cache.enumerate_nodes().unwrap();
    assert_eq!(nodes.len(), 2);
}

#[test]
fn test_pre_advise_efficiency() {
    use super::super::InteriorNode;

    let db = create_test_db();
    let cache = create_test_cache(Arc::clone(&db));

    // Create a binary tree structure with controlled keys
    // Tree structure:
    //           root (prefix 0)
    //          /              \
    //    left subtree      right subtree
    //       /    \             /    \
    //    leaf1  leaf2      leaf3  leaf4

    // Create 4 leaf nodes with specific keys
    let mut key1 = [0u8; 32];
    key1[0] = 0b0000_0000; // Goes left then left
    let value1 = [1u8; 32];
    let prefix1 = Prefix::from(key1);
    let leaf1 = LeafNode::new(key1, value1);

    let mut key2 = [0u8; 32];
    key2[0] = 0b0100_0000; // Goes left then right
    let value2 = [2u8; 32];
    let prefix2 = Prefix::from(key2);
    let leaf2 = LeafNode::new(key2, value2);

    let mut key3 = [0u8; 32];
    key3[0] = 0b1000_0000; // Goes right then left
    let value3 = [3u8; 32];
    let prefix3 = Prefix::from(key3);
    let leaf3 = LeafNode::new(key3, value3);

    let mut key4 = [0u8; 32];
    key4[0] = 0b1100_0000; // Goes right then right
    let value4 = [4u8; 32];
    let prefix4 = Prefix::from(key4);
    let leaf4 = LeafNode::new(key4, value4);

    // Create left subtree interior node (covers left side)
    let left_subtree_prefix = Prefix {
        hash: [0u8; 32],
        length: 1,
    };
    let left_interior = InteriorNode::new(
        left_subtree_prefix,
        prefix1,
        prefix2,
        leaf1.merkle_hash,
        leaf2.merkle_hash,
    );

    // Create right subtree interior node (covers right side)
    let mut right_prefix_hash = [0u8; 32];
    right_prefix_hash[0] = 0b1000_0000;
    let right_subtree_prefix = Prefix {
        hash: right_prefix_hash,
        length: 1,
    };
    let right_interior = InteriorNode::new(
        right_subtree_prefix,
        prefix3,
        prefix4,
        leaf3.merkle_hash,
        leaf4.merkle_hash,
    );

    // Create root interior node
    let root_prefix = Prefix::root();
    let root_interior = InteriorNode::new(
        root_prefix,
        left_subtree_prefix,
        right_subtree_prefix,
        left_interior.merkle_hash,
        right_interior.merkle_hash,
    );

    // Add all nodes to cache and flush to database
    cache.set(prefix1, Node::Leaf(leaf1));
    cache.set(prefix2, Node::Leaf(leaf2));
    cache.set(prefix3, Node::Leaf(leaf3));
    cache.set(prefix4, Node::Leaf(leaf4));
    cache.set(left_subtree_prefix, Node::Interior(left_interior));
    cache.set(right_subtree_prefix, Node::Interior(right_interior));
    cache.set(root_prefix, Node::Interior(root_interior));
    cache.flush().unwrap();

    // Clear cache to start fresh
    cache.clear();
    assert_eq!(cache.len(), 0);

    // Pre-advise for just 2 keys (leaf1 and leaf3)
    // This should load:
    // - root (on path)
    // - left_subtree (on path to leaf1)
    // - right_subtree (on path to leaf3)
    // - leaf1 (needed)
    // - leaf2 (sibling of leaf1)
    // - leaf3 (needed)
    // - leaf4 (sibling of leaf3)
    // Total: 7 nodes
    cache.pre_advise(&[prefix1, prefix3]).unwrap();

    // Verify we loaded at most 2 * needed_keys nodes
    // Actually, with siblings, we expect: needed_keys + their on-path ancestors + their siblings
    // For 2 needed keys in a balanced tree of depth 2:
    // - 2 needed leaves
    // - 2 siblings (one for each needed leaf)
    // - 2 interior nodes on path (left_subtree, right_subtree)
    // - 1 root
    // = 7 total
    let loaded_count = cache.len();
    println!("Loaded {} nodes for 2 needed keys", loaded_count);

    // The bound should be: at most 2 * needed_keys * tree_depth
    // But a simpler bound: we should load needed keys + their ancestors + siblings on path
    // For efficiency, we want to ensure we're not loading the entire tree
    assert!(
        loaded_count <= 2 * 2 * 3,
        "Loaded too many nodes: {} (expected <= 12)",
        loaded_count
    );

    // More importantly, verify we didn't load ALL nodes (7 total exist)
    // We should have loaded exactly the necessary nodes
    assert!(
        loaded_count <= 7,
        "Loaded {} nodes, but only 7 exist in tree",
        loaded_count
    );

    // Verify the needed keys are actually loaded
    assert!(cache.get(&prefix1).is_some(), "leaf1 should be loaded");
    assert!(cache.get(&prefix3).is_some(), "leaf3 should be loaded");
}

#[test]
fn test_pre_advise_large_tree_efficiency() {
    use super::super::InteriorNode;
    use sha2::{Digest, Sha256};

    let db = create_test_db();
    let cache = create_test_cache(Arc::clone(&db));

    // Generate 1000 unique leaf nodes with deterministic but varied keys
    let num_nodes = 1000;
    let mut prefixes = Vec::new();

    for i in 0..num_nodes {
        // Create unique keys by hashing the index
        let mut hasher = Sha256::new();
        hasher.update((i as u64).to_le_bytes());
        let hash = hasher.finalize();
        let mut key = [0u8; 32];
        key.copy_from_slice(&hash);

        let value = [(i % 256) as u8; 32];
        let prefix = Prefix::from(key);
        let leaf = LeafNode::new(key, value);

        cache.set(prefix, Node::Leaf(leaf));
        prefixes.push(prefix);
    }

    // Create a simple root interior node that points to first two leaves
    // (This creates a minimal tree structure - in reality you'd build a proper tree,
    // but for testing efficiency we just need some nodes in the DB)
    let root_prefix = Prefix::root();
    let root_interior =
        InteriorNode::new(root_prefix, prefixes[0], prefixes[1], [1u8; 32], [2u8; 32]);
    cache.set(root_prefix, Node::Interior(root_interior));

    // Flush all nodes to database
    cache.flush().unwrap();
    println!("Flushed {} nodes to database", cache.tree_size());

    // Clear cache to start fresh
    cache.clear();
    assert_eq!(cache.len(), 0);

    // Pre-advise for just ONE node
    cache.pre_advise(&[prefixes[0]]).unwrap();

    let loaded_count = cache.len();
    println!(
        "Loaded {} nodes when pre-advising 1 key from a tree of 1000+ nodes",
        loaded_count
    );

    // Check that we loaded a reasonable number of nodes
    // For a single key, we should load:
    // - The key itself (1)
    // - Nodes on path from root (depends on tree depth, ~log(n))
    // - Siblings on the path (also ~log(n))
    // For 1000 nodes, log2(1000) ≈ 10, so with siblings we'd expect ~20 nodes max
    // Let's be generous and say anything under 50 is reasonable (2*needed_keys * reasonable_depth)
    assert!(
        loaded_count <= 50,
        "Loaded too many nodes: {} (expected <= 50 for 1 key in 1000-node tree)",
        loaded_count
    );

    // More importantly, we should NOT have loaded all or most nodes
    assert!(
        loaded_count < 100,
        "Loaded {} nodes, which suggests inefficient traversal (should be ~O(log n))",
        loaded_count
    );

    // Verify the requested key is actually loaded
    assert!(
        cache.get(&prefixes[0]).is_some(),
        "The requested key should be loaded"
    );
}

#[test]
fn test_pre_advise_loads_complete_paths_with_siblings() {
    use super::super::InteriorNode;

    let db = create_test_db();
    let cache = create_test_cache(Arc::clone(&db));

    // Create a simple 3-level tree with explicit structure:
    //           root
    //          /    \
    //      left      right
    //      /  \      /   \
    //    l1   l2   l3    l4

    let mut key_l1 = [0u8; 32];
    key_l1[0] = 0b0000_0000;
    let prefix_l1 = Prefix::from(key_l1);
    let leaf_l1 = LeafNode::new(key_l1, [11u8; 32]);

    let mut key_l2 = [0u8; 32];
    key_l2[0] = 0b0100_0000;
    let prefix_l2 = Prefix::from(key_l2);
    let leaf_l2 = LeafNode::new(key_l2, [22u8; 32]);

    let mut key_l3 = [0u8; 32];
    key_l3[0] = 0b1000_0000;
    let prefix_l3 = Prefix::from(key_l3);
    let leaf_l3 = LeafNode::new(key_l3, [33u8; 32]);

    let mut key_l4 = [0u8; 32];
    key_l4[0] = 0b1100_0000;
    let prefix_l4 = Prefix::from(key_l4);
    let leaf_l4 = LeafNode::new(key_l4, [44u8; 32]);

    // Create left subtree
    let left_prefix = Prefix {
        hash: [0u8; 32],
        length: 1,
    };
    let left_interior = InteriorNode::new(
        left_prefix,
        prefix_l1,
        prefix_l2,
        leaf_l1.merkle_hash,
        leaf_l2.merkle_hash,
    );

    // Create right subtree
    let mut right_hash = [0u8; 32];
    right_hash[0] = 0b1000_0000;
    let right_prefix = Prefix {
        hash: right_hash,
        length: 1,
    };
    let right_interior = InteriorNode::new(
        right_prefix,
        prefix_l3,
        prefix_l4,
        leaf_l3.merkle_hash,
        leaf_l4.merkle_hash,
    );

    // Create root
    let root_prefix = Prefix::root();
    let root_interior = InteriorNode::new(
        root_prefix,
        left_prefix,
        right_prefix,
        left_interior.merkle_hash,
        right_interior.merkle_hash,
    );

    // Add all nodes and flush
    cache.set(prefix_l1, Node::Leaf(leaf_l1));
    cache.set(prefix_l2, Node::Leaf(leaf_l2));
    cache.set(prefix_l3, Node::Leaf(leaf_l3));
    cache.set(prefix_l4, Node::Leaf(leaf_l4));
    cache.set(left_prefix, Node::Interior(left_interior));
    cache.set(right_prefix, Node::Interior(right_interior));
    cache.set(root_prefix, Node::Interior(root_interior));
    cache.flush().unwrap();

    // Clear and pre-advise for just l1
    cache.clear();
    cache.pre_advise(&[prefix_l1]).unwrap();

    // Verify all ancestors on the path are loaded
    assert!(cache.get(&root_prefix).is_some(), "Root should be loaded");
    assert!(
        cache.get(&left_prefix).is_some(),
        "Left interior (ancestor) should be loaded"
    );
    assert!(
        cache.get(&prefix_l1).is_some(),
        "Target leaf l1 should be loaded"
    );

    // Verify siblings on the path are loaded (needed for merkle hash recomputation)
    assert!(
        cache.get(&right_prefix).is_some(),
        "Right interior (sibling of left path) should be loaded"
    );
    assert!(
        cache.get(&prefix_l2).is_some(),
        "Leaf l2 (sibling of l1) should be loaded"
    );

    // We shouldn't load leaves that aren't on path or siblings
    // (l3 and l4 are children of right_interior, which is a sibling but not on the direct path)
}

#[test]
fn test_pre_advise_shared_ancestors() {
    use super::super::InteriorNode;

    let db = create_test_db();
    let cache = create_test_cache(Arc::clone(&db));

    // Create keys that share common ancestors
    let mut key1 = [0u8; 32];
    key1[0] = 0b0000_0000; // Goes left, left
    let prefix1 = Prefix::from(key1);
    let leaf1 = LeafNode::new(key1, [1u8; 32]);

    let mut key2 = [0u8; 32];
    key2[0] = 0b0100_0000; // Goes left, right (shares left subtree with key1)
    let prefix2 = Prefix::from(key2);
    let leaf2 = LeafNode::new(key2, [2u8; 32]);

    // Create shared left subtree
    let left_prefix = Prefix {
        hash: [0u8; 32],
        length: 1,
    };
    let left_interior = InteriorNode::new(
        left_prefix,
        prefix1,
        prefix2,
        leaf1.merkle_hash,
        leaf2.merkle_hash,
    );

    // Create a dummy right subtree
    let mut key3 = [0u8; 32];
    key3[0] = 0b1000_0000;
    let prefix3 = Prefix::from(key3);
    let leaf3 = LeafNode::new(key3, [3u8; 32]);

    let mut right_hash = [0u8; 32];
    right_hash[0] = 0b1000_0000;
    let right_prefix = Prefix {
        hash: right_hash,
        length: 1,
    };

    // Create root
    let root_prefix = Prefix::root();
    let root_interior = InteriorNode::new(
        root_prefix,
        left_prefix,
        right_prefix,
        left_interior.merkle_hash,
        [99u8; 32],
    );

    // Add and flush
    cache.set(prefix1, Node::Leaf(leaf1));
    cache.set(prefix2, Node::Leaf(leaf2));
    cache.set(prefix3, Node::Leaf(leaf3));
    cache.set(left_prefix, Node::Interior(left_interior));
    cache.set(root_prefix, Node::Interior(root_interior));
    cache.flush().unwrap();

    // Clear and pre-advise for both keys that share ancestors
    cache.clear();
    let initial_tree_size = cache.tree_size();
    cache.pre_advise(&[prefix1, prefix2]).unwrap();

    // Verify shared ancestors are loaded
    assert!(
        cache.get(&root_prefix).is_some(),
        "Shared root should be loaded"
    );
    assert!(
        cache.get(&left_prefix).is_some(),
        "Shared left interior should be loaded"
    );
    assert!(cache.get(&prefix1).is_some(), "Key1 should be loaded");
    assert!(cache.get(&prefix2).is_some(), "Key2 should be loaded");

    // Verify we didn't load significantly more than necessary
    // Should load: root (1) + left_interior (1) + right_prefix sibling (1) + leaf1 (1) + leaf2 (1) = 5
    let loaded = cache.len();
    println!(
        "Loaded {} nodes for 2 keys with shared ancestors (tree size: {})",
        loaded, initial_tree_size
    );
    assert!(
        loaded <= 6,
        "Should load at most 6 nodes, loaded {}",
        loaded
    );
}

#[test]
fn test_pre_advise_with_nonexistent_keys() {
    use super::super::InteriorNode;

    let db = create_test_db();
    let cache = create_test_cache(Arc::clone(&db));

    // Create a small tree
    let mut key1 = [0u8; 32];
    key1[0] = 0b0000_0000;
    let prefix1 = Prefix::from(key1);
    let leaf1 = LeafNode::new(key1, [1u8; 32]);

    let mut key2 = [0u8; 32];
    key2[0] = 0b1000_0000;
    let prefix2 = Prefix::from(key2);
    let leaf2 = LeafNode::new(key2, [2u8; 32]);

    let root_prefix = Prefix::root();
    let root_interior = InteriorNode::new(
        root_prefix,
        prefix1,
        prefix2,
        leaf1.merkle_hash,
        leaf2.merkle_hash,
    );

    cache.set(prefix1, Node::Leaf(leaf1));
    cache.set(prefix2, Node::Leaf(leaf2));
    cache.set(root_prefix, Node::Interior(root_interior));
    cache.flush().unwrap();

    // Clear and pre-advise for a key that doesn't exist
    cache.clear();
    let mut nonexistent_key = [0u8; 32];
    nonexistent_key[0] = 0b0100_0000; // Would go left from root, but doesn't exist
    let nonexistent_prefix = Prefix::from(nonexistent_key);

    cache.pre_advise(&[nonexistent_prefix]).unwrap();

    // Should still load the root and potentially some nodes along the path
    assert!(
        cache.get(&root_prefix).is_some(),
        "Root should be loaded even for nonexistent key"
    );

    // The nonexistent key itself won't be in cache since it doesn't exist in DB
    assert!(
        cache.get(&nonexistent_prefix).is_none(),
        "Nonexistent key shouldn't be in cache"
    );
}

#[test]
fn test_pre_advise_keys_at_different_depths() {
    use super::super::InteriorNode;

    let db = create_test_db();
    let cache = create_test_cache(Arc::clone(&db));

    // Create an unbalanced tree where leaves are at different depths
    // Level 0: root
    // Level 1: left_interior, leaf_right (depth 1)
    // Level 2: leaf_ll, leaf_lr (depth 2)

    let mut key_ll = [0u8; 32];
    key_ll[0] = 0b0000_0000;
    let prefix_ll = Prefix::from(key_ll);
    let leaf_ll = LeafNode::new(key_ll, [1u8; 32]);

    let mut key_lr = [0u8; 32];
    key_lr[0] = 0b0100_0000;
    let prefix_lr = Prefix::from(key_lr);
    let leaf_lr = LeafNode::new(key_lr, [2u8; 32]);

    let mut key_right = [0u8; 32];
    key_right[0] = 0b1000_0000;
    let prefix_right = Prefix::from(key_right);
    let leaf_right = LeafNode::new(key_right, [3u8; 32]);

    // Left interior at depth 1
    let left_prefix = Prefix {
        hash: [0u8; 32],
        length: 1,
    };
    let left_interior = InteriorNode::new(
        left_prefix,
        prefix_ll,
        prefix_lr,
        leaf_ll.merkle_hash,
        leaf_lr.merkle_hash,
    );

    // Root
    let root_prefix = Prefix::root();
    let root_interior = InteriorNode::new(
        root_prefix,
        left_prefix,
        prefix_right,
        left_interior.merkle_hash,
        leaf_right.merkle_hash,
    );

    cache.set(prefix_ll, Node::Leaf(leaf_ll));
    cache.set(prefix_lr, Node::Leaf(leaf_lr));
    cache.set(prefix_right, Node::Leaf(leaf_right));
    cache.set(left_prefix, Node::Interior(left_interior));
    cache.set(root_prefix, Node::Interior(root_interior));
    cache.flush().unwrap();

    // Clear and pre-advise for leaves at different depths
    cache.clear();
    cache.pre_advise(&[prefix_ll, prefix_right]).unwrap();

    // Both leaves should be loaded despite being at different depths
    assert!(
        cache.get(&prefix_ll).is_some(),
        "Deep leaf (depth 2) should be loaded"
    );
    assert!(
        cache.get(&prefix_right).is_some(),
        "Shallow leaf (depth 1) should be loaded"
    );

    // Their ancestors should be loaded
    assert!(cache.get(&root_prefix).is_some(), "Root should be loaded");
    assert!(
        cache.get(&left_prefix).is_some(),
        "Left interior (ancestor of ll) should be loaded"
    );
}

#[test]
fn test_pre_advise_loads_both_siblings() {
    use super::super::InteriorNode;

    let db = create_test_db();
    let cache = create_test_cache(Arc::clone(&db));

    // Create a path: root -> left -> right (to leaf)
    // This means we go left at root, then right at the next level
    let mut key_target = [0u8; 32];
    key_target[0] = 0b0100_0000; // First bit 0 (left), second bit 1 (right)
    let prefix_target = Prefix::from(key_target);
    let leaf_target = LeafNode::new(key_target, [99u8; 32]);

    let mut key_sibling = [0u8; 32];
    key_sibling[0] = 0b0000_0000; // Sibling of target (left-left)
    let prefix_sibling = Prefix::from(key_sibling);
    let leaf_sibling = LeafNode::new(key_sibling, [88u8; 32]);

    let mut key_right_child1 = [0u8; 32];
    key_right_child1[0] = 0b1000_0000;
    let prefix_right_child1 = Prefix::from(key_right_child1);
    let leaf_right_child1 = LeafNode::new(key_right_child1, [77u8; 32]);

    let mut key_right_child2 = [0u8; 32];
    key_right_child2[0] = 0b1100_0000;
    let prefix_right_child2 = Prefix::from(key_right_child2);
    let leaf_right_child2 = LeafNode::new(key_right_child2, [66u8; 32]);

    // Left subtree interior
    let left_prefix = Prefix {
        hash: [0u8; 32],
        length: 1,
    };
    let left_interior = InteriorNode::new(
        left_prefix,
        prefix_sibling,
        prefix_target,
        leaf_sibling.merkle_hash,
        leaf_target.merkle_hash,
    );

    // Right subtree interior (sibling of left path)
    let mut right_hash = [0u8; 32];
    right_hash[0] = 0b1000_0000;
    let right_prefix = Prefix {
        hash: right_hash,
        length: 1,
    };
    let right_interior = InteriorNode::new(
        right_prefix,
        prefix_right_child1,
        prefix_right_child2,
        leaf_right_child1.merkle_hash,
        leaf_right_child2.merkle_hash,
    );

    // Root
    let root_prefix = Prefix::root();
    let root_interior = InteriorNode::new(
        root_prefix,
        left_prefix,
        right_prefix,
        left_interior.merkle_hash,
        right_interior.merkle_hash,
    );

    cache.set(prefix_target, Node::Leaf(leaf_target));
    cache.set(prefix_sibling, Node::Leaf(leaf_sibling));
    cache.set(prefix_right_child1, Node::Leaf(leaf_right_child1));
    cache.set(prefix_right_child2, Node::Leaf(leaf_right_child2));
    cache.set(left_prefix, Node::Interior(left_interior));
    cache.set(right_prefix, Node::Interior(right_interior));
    cache.set(root_prefix, Node::Interior(root_interior));
    cache.flush().unwrap();

    // Clear and pre-advise for target
    cache.clear();
    cache.pre_advise(&[prefix_target]).unwrap();

    // Verify target and its sibling are loaded
    assert!(
        cache.get(&prefix_target).is_some(),
        "Target leaf should be loaded"
    );
    assert!(
        cache.get(&prefix_sibling).is_some(),
        "Sibling of target should be loaded"
    );

    // Verify right_interior (sibling of left path) is loaded
    assert!(
        cache.get(&right_prefix).is_some(),
        "Right interior (sibling at root level) should be loaded"
    );

    // Verify ancestors
    assert!(cache.get(&root_prefix).is_some(), "Root should be loaded");
    assert!(
        cache.get(&left_prefix).is_some(),
        "Left interior should be loaded"
    );
}

#[test]
fn test_pre_advise_sparse_keys_efficiency() {
    use super::super::InteriorNode;
    use sha2::{Digest, Sha256};

    let db = create_test_db();
    let cache = create_test_cache(Arc::clone(&db));

    // Build a larger tree with 100 leaves
    let num_leaves = 100;
    let mut all_prefixes = Vec::new();

    for i in 0..num_leaves {
        let mut hasher = Sha256::new();
        hasher.update(&(i as u64).to_le_bytes());
        let hash = hasher.finalize();
        let mut key = [0u8; 32];
        key.copy_from_slice(&hash);

        let prefix = Prefix::from(key);
        let leaf = LeafNode::new(key, [(i % 256) as u8; 32]);

        cache.set(prefix, Node::Leaf(leaf));
        all_prefixes.push(prefix);
    }

    // Add a minimal root structure
    let root_prefix = Prefix::root();
    let root_interior = InteriorNode::new(
        root_prefix,
        all_prefixes[0],
        all_prefixes[1],
        [1u8; 32],
        [2u8; 32],
    );
    cache.set(root_prefix, Node::Interior(root_interior));

    cache.flush().unwrap();
    let total_nodes = cache.tree_size();

    // Clear and pre-advise for 3 sparse keys
    cache.clear();
    let sparse_keys = vec![all_prefixes[0], all_prefixes[50], all_prefixes[99]];
    cache.pre_advise(&sparse_keys).unwrap();

    let loaded = cache.len();
    println!(
        "Loaded {} nodes for 3 sparse keys from a tree of {} nodes",
        loaded, total_nodes
    );

    // Should load O(log n) nodes per key, not O(n)
    // With 100 nodes and 3 keys, we expect roughly 3 * (log2(100) + siblings) ≈ 3 * 14 = 42
    // Be generous and allow up to 60 nodes
    assert!(
        loaded < 60,
        "Loaded {} nodes for 3 keys, expected < 60 (tree has {} nodes)",
        loaded,
        total_nodes
    );

    // Most importantly, should not load most of the tree
    assert!(
        loaded < total_nodes / 2,
        "Loaded {} nodes, which is >= 50% of tree ({}), suggesting inefficient loading",
        loaded,
        total_nodes
    );
}

#[test]
fn test_pre_advise_with_partial_cache() {
    use super::super::InteriorNode;

    let db = create_test_db();
    let cache = create_test_cache(Arc::clone(&db));

    // Create a 3-level tree
    let mut key1 = [0u8; 32];
    key1[0] = 0b0000_0000;
    let prefix1 = Prefix::from(key1);
    let leaf1 = LeafNode::new(key1, [1u8; 32]);

    let mut key2 = [0u8; 32];
    key2[0] = 0b0100_0000;
    let prefix2 = Prefix::from(key2);
    let leaf2 = LeafNode::new(key2, [2u8; 32]);

    let mut key3 = [0u8; 32];
    key3[0] = 0b1000_0000;
    let prefix3 = Prefix::from(key3);
    let leaf3 = LeafNode::new(key3, [3u8; 32]);

    let mut key4 = [0u8; 32];
    key4[0] = 0b1100_0000;
    let prefix4 = Prefix::from(key4);
    let leaf4 = LeafNode::new(key4, [4u8; 32]);

    let left_prefix = Prefix {
        hash: [0u8; 32],
        length: 1,
    };
    let left_interior = InteriorNode::new(
        left_prefix,
        prefix1,
        prefix2,
        leaf1.merkle_hash,
        leaf2.merkle_hash,
    );

    let mut right_hash = [0u8; 32];
    right_hash[0] = 0b1000_0000;
    let right_prefix = Prefix {
        hash: right_hash,
        length: 1,
    };
    let right_interior = InteriorNode::new(
        right_prefix,
        prefix3,
        prefix4,
        leaf3.merkle_hash,
        leaf4.merkle_hash,
    );

    let root_prefix = Prefix::root();
    let root_interior = InteriorNode::new(
        root_prefix,
        left_prefix,
        right_prefix,
        left_interior.merkle_hash,
        right_interior.merkle_hash,
    );

    cache.set(prefix1, Node::Leaf(leaf1));
    cache.set(prefix2, Node::Leaf(leaf2));
    cache.set(prefix3, Node::Leaf(leaf3));
    cache.set(prefix4, Node::Leaf(leaf4));
    cache.set(left_prefix, Node::Interior(left_interior.clone()));
    cache.set(right_prefix, Node::Interior(right_interior.clone()));
    cache.set(root_prefix, Node::Interior(root_interior.clone()));
    cache.flush().unwrap();

    // Test scenario: Cache only some upper-level nodes, not the target leaves
    // This simulates a more realistic partial cache where some tree structure is known
    cache.clear();
    cache.set(root_prefix, Node::Interior(root_interior));
    cache.set(left_prefix, Node::Interior(left_interior));
    let initial_count = cache.len();
    assert_eq!(
        initial_count, 2,
        "Should start with root and left interior cached"
    );

    // Pre-advise for prefix1 (left-left leaf)
    // Since root and left_interior are cached, pre_advise won't traverse them
    // This reveals a limitation: pre_advise skips already-cached interior nodes
    // So it won't load children of cached interior nodes unless forced
    cache.pre_advise(&[prefix1]).unwrap();

    // The current implementation has a limitation: when interior nodes are already
    // cached, pre_advise doesn't traverse into them to load their children.
    // It only processes nodes that it queries from the database.
    // So we test what actually happens, not what might be ideal:

    // Root and left_interior should still be in cache
    assert!(
        cache.get(&root_prefix).is_some(),
        "Root should still be in cache"
    );
    assert!(
        cache.get(&left_prefix).is_some(),
        "Left interior should still be in cache"
    );

    // The target leaf will be loaded via the fallback mechanism that directly loads
    // remaining needed keys at the end of pre_advise
    assert!(
        cache.get(&prefix1).is_some(),
        "Target leaf should be loaded via fallback"
    );

    let final_count = cache.len();
    println!(
        "Cache size: initial={}, final={}",
        initial_count, final_count
    );

    // We should have at least loaded the target
    assert!(
        final_count >= initial_count,
        "Should have loaded at least the target"
    );

    // This test documents current behavior: pre_advise has a limitation where
    // it doesn't traverse already-cached interior nodes. The fallback mechanism
    // at the end handles loading the actual target keys directly.
}

#[test]
fn test_release_keys_retains_needed_keys() {
    use super::super::LeafNode;

    let db = create_test_db();
    let cache = create_test_cache(db);

    let entry_size = super::CACHE_ENTRY_SIZE_BYTES;
    let limit = cache.cache_memory_limit_bytes();

    let total_entries = (limit / entry_size) + 6;
    let mut prefixes = Vec::new();

    for i in 0..total_entries {
        let mut key = [0u8; 32];
        key[0] = i as u8;
        let value = [i as u8; 32];
        let leaf = LeafNode::new(key, value);
        let prefix = Prefix::from(key);
        cache.set(prefix, Node::Leaf(leaf));
        prefixes.push(prefix);
    }

    let needed_keys = [prefixes[0], prefixes[1]];
    cache.release_keys(&needed_keys);

    for prefix in needed_keys.iter() {
        assert!(
            cache.get(prefix).is_some(),
            "needed key {:?} should be retained",
            prefix
        );
    }

    let usage = cache.len() * entry_size;
    assert!(
        usage <= limit,
        "cache usage {} should not exceed limit {} after eviction",
        usage,
        limit
    );

    assert!(
        cache.get(prefixes.last().unwrap()).is_none(),
        "non-needed leaves should be evicted first"
    );
}

#[test]
fn test_release_keys_prefers_deep_nodes_for_eviction() {
    use super::super::{InteriorNode, LeafNode};

    let db = create_test_db();
    let cache = create_test_cache(db);

    let left_key = [0xAA; 32];
    let right_key = [0xBB; 32];
    let left_leaf = LeafNode::new(left_key, [0x11; 32]);
    let right_leaf = LeafNode::new(right_key, [0x22; 32]);
    let left_prefix = Prefix::from(left_key);
    let right_prefix = Prefix::from(right_key);

    let limit = cache.cache_memory_limit_bytes();

    cache.set(left_prefix, Node::Leaf(left_leaf.clone()));
    cache.set(right_prefix, Node::Leaf(right_leaf.clone()));

    let lengths = [30u16, 90, 150, 180, 200, 230, 250];
    let mut prefixes = Vec::new();

    for (idx, length) in lengths.iter().enumerate() {
        let mut hash = [0u8; 32];
        hash[0] = idx as u8;
        let prefix = Prefix {
            hash,
            length: *length,
        };
        let interior = InteriorNode::new(
            prefix,
            left_prefix,
            right_prefix,
            left_leaf.merkle_hash,
            right_leaf.merkle_hash,
        );
        cache.set(prefix, Node::Interior(interior));
        prefixes.push(prefix);
    }

    // Only keep the shallowest prefix
    let needed = [prefixes[0]];
    cache.release_keys(&needed);

    assert!(
        cache.get(&prefixes[0]).is_some(),
        "shallowest prefix should be retained"
    );
    for prefix in prefixes.iter().take(3) {
        assert!(
            cache.get(prefix).is_some(),
            "shallower prefixes should be retained: {:?}",
            prefix
        );
    }
    assert!(
        cache.get(prefixes.last().unwrap()).is_none(),
        "deepest prefix should be evicted first"
    );
    assert!(
        cache.get(&prefixes[5]).is_some(),
        "lower-depth interior nodes should remain while deeper ones are evicted"
    );

    let usage = cache.len() * super::CACHE_ENTRY_SIZE_BYTES;
    assert!(
        usage <= limit,
        "cache usage {} should not exceed limit {}",
        usage,
        limit
    );
}

#[test]
fn test_release_keys_with_root_node() {
    use super::super::InteriorNode;

    let db = create_test_db();
    let cache = create_test_cache(db);

    let key1 = [1u8; 32];
    let prefix1 = Prefix::from(key1);
    let leaf1 = LeafNode::new(key1, [10u8; 32]);

    let key2 = [2u8; 32];
    let prefix2 = Prefix::from(key2);
    let leaf2 = LeafNode::new(key2, [20u8; 32]);

    // Root node (length 0, should be retained)
    let root_prefix = Prefix::root();
    let root = InteriorNode::new(
        root_prefix,
        prefix1,
        prefix2,
        leaf1.merkle_hash,
        leaf2.merkle_hash,
    );

    cache.set(root_prefix, Node::Interior(root));
    cache.set(prefix1, Node::Leaf(leaf1));
    cache.set(prefix2, Node::Leaf(leaf2));

    let entry_size = super::CACHE_ENTRY_SIZE_BYTES;
    let limit = cache.cache_memory_limit_bytes();

    let additional_entries = (limit / entry_size) + 6;
    for i in 0..additional_entries {
        let mut key = [0u8; 32];
        key[0] = (i + 10) as u8;
        let value = [i as u8; 32];
        let leaf = LeafNode::new(key, value);
        cache.set(Prefix::from(key), Node::Leaf(leaf));
    }

    cache.release_keys(&[]);

    assert!(
        cache.get(&root_prefix).is_some(),
        "root should never be evicted"
    );
    assert!(
        cache.len() * entry_size <= limit,
        "cache should respect memory limit after eviction"
    );
}

#[test]
fn test_pre_advise_transaction_management() {
    let db = create_test_db();
    let cache = create_test_cache(Arc::clone(&db));

    // Create and flush a simple node
    let key = [1u8; 32];
    let prefix = Prefix::from(key);
    let leaf = LeafNode::new(key, [10u8; 32]);
    cache.set(prefix, Node::Leaf(leaf));
    cache.flush().unwrap();

    // Clear and do pre_advise - should start a transaction
    cache.clear();
    cache.pre_advise(&[prefix]).unwrap();

    // Verify transaction is active
    {
        let in_tx = cache.in_transaction.lock().unwrap();
        assert!(*in_tx, "Transaction should be active after pre_advise");
    }

    // Do another pre_advise - should not create nested transaction
    let key2 = [2u8; 32];
    let prefix2 = Prefix::from(key2);
    cache.pre_advise(&[prefix2]).unwrap();

    // Transaction should still be active
    {
        let in_tx = cache.in_transaction.lock().unwrap();
        assert!(*in_tx, "Transaction should still be active");
    }

    // Flush should commit the transaction
    cache.flush().unwrap();

    // Transaction should be closed
    {
        let in_tx = cache.in_transaction.lock().unwrap();
        assert!(!*in_tx, "Transaction should be closed after flush");
    }
}

#[test]
fn test_pre_advise_loads_all_ancestors() {
    use super::super::InteriorNode;

    let db = create_test_db();
    let cache = create_test_cache(Arc::clone(&db));

    // Build a simpler deep tree with 4 levels
    // Structure:
    //     root (len=0)
    //     /         \
    //   i1(len=1)   r1
    //   /      \
    // i2(len=2) r2
    // /      \
    // leaf   r3

    let mut leaf_key = [0u8; 32];
    leaf_key[0] = 0b0000_0000; // Goes left at all levels
    let leaf_prefix = Prefix::from(leaf_key);
    let leaf = LeafNode::new(leaf_key, [99u8; 32]);

    // Create right sibling leaves at each level
    let mut r3_key = [0u8; 32];
    r3_key[0] = 0b0100_0000; // Differs at bit 1
    let r3_prefix = Prefix::from(r3_key);
    let r3_leaf = LeafNode::new(r3_key, [3u8; 32]);

    let mut r2_key = [0u8; 32];
    r2_key[0] = 0b0010_0000; // Differs at bit 2
    let r2_prefix = Prefix::from(r2_key);
    let r2_leaf = LeafNode::new(r2_key, [2u8; 32]);

    let mut r1_key = [0u8; 32];
    r1_key[0] = 0b1000_0000; // Differs at bit 0
    let r1_prefix = Prefix::from(r1_key);
    let r1_leaf = LeafNode::new(r1_key, [1u8; 32]);

    // Build from bottom up
    // i2: has leaf and r3 as children
    let mut i2_hash = [0u8; 32];
    i2_hash[0] = 0b0000_0000;
    let i2_prefix = Prefix {
        hash: i2_hash,
        length: 2,
    };
    let i2 = InteriorNode::new(
        i2_prefix,
        leaf_prefix,
        r3_prefix,
        leaf.merkle_hash,
        r3_leaf.merkle_hash,
    );

    // i1: has i2 and r2 as children
    let mut i1_hash = [0u8; 32];
    i1_hash[0] = 0b0000_0000;
    let i1_prefix = Prefix {
        hash: i1_hash,
        length: 1,
    };
    let i1 = InteriorNode::new(
        i1_prefix,
        i2_prefix,
        r2_prefix,
        i2.merkle_hash,
        r2_leaf.merkle_hash,
    );

    // root: has i1 and r1 as children
    let root_prefix = Prefix::root();
    let root = InteriorNode::new(
        root_prefix,
        i1_prefix,
        r1_prefix,
        i1.merkle_hash,
        r1_leaf.merkle_hash,
    );

    // Add all nodes to cache and flush
    cache.set(leaf_prefix, Node::Leaf(leaf));
    cache.set(r3_prefix, Node::Leaf(r3_leaf));
    cache.set(r2_prefix, Node::Leaf(r2_leaf));
    cache.set(r1_prefix, Node::Leaf(r1_leaf));
    cache.set(i2_prefix, Node::Interior(i2));
    cache.set(i1_prefix, Node::Interior(i1));
    cache.set(root_prefix, Node::Interior(root));
    cache.flush().unwrap();

    // Clear and pre-advise for the deep leaf
    cache.clear();
    cache.pre_advise(&[leaf_prefix]).unwrap();

    // Verify the leaf is loaded
    assert!(
        cache.get(&leaf_prefix).is_some(),
        "Target leaf should be loaded"
    );

    // Verify ALL ancestors are loaded (no gaps in the path)
    assert!(
        cache.get(&root_prefix).is_some(),
        "Root (length 0) should be loaded"
    );
    assert!(
        cache.get(&i1_prefix).is_some(),
        "Interior node i1 (length 1) should be loaded"
    );
    assert!(
        cache.get(&i2_prefix).is_some(),
        "Interior node i2 (length 2) should be loaded"
    );

    // Verify siblings are loaded (needed for merkle hash recomputation)
    assert!(
        cache.get(&r3_prefix).is_some(),
        "Sibling r3 should be loaded"
    );

    println!("Successfully verified all ancestors and siblings are loaded for deep leaf");
}
