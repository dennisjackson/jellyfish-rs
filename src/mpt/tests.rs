use super::*;

fn create_hash(value: u8) -> Hash {
    let mut hash = [0u8; 32];
    hash[0] = value;
    hash
}

#[test]
fn test_empty_tree() {
    let mpt = SimpleMPT::new();
    assert_eq!(mpt.store.len(), 0);
    assert_eq!(mpt.root, Prefix::root());
}

#[test]
fn test_single_insert() {
    let mut mpt = SimpleMPT::new();
    let key = create_hash(1);
    let value = create_hash(101);

    mpt.upsert(key, value);

    assert_eq!(mpt.store.len(), 1);
    let node = mpt.store.get(&Prefix::from_hash(key)).unwrap();
    match node {
        Node::Leaf(leaf) => {
            assert_eq!(leaf.key, key);
            assert_eq!(leaf.value, value);
        }
        _ => panic!("Expected leaf node"),
    }
}

#[test]
fn test_update_existing_key() {
    let mut mpt = SimpleMPT::new();
    let key = create_hash(1);
    let value1 = create_hash(101);
    let value2 = create_hash(102);

    mpt.upsert(key, value1);
    let hash1 = mpt.store.get(&mpt.root).unwrap().merkle_hash();

    mpt.upsert(key, value2);
    let hash2 = mpt.store.get(&mpt.root).unwrap().merkle_hash();

    // Hash should change when value changes
    assert_ne!(hash1, hash2);

    // Should still have only one leaf node
    assert_eq!(mpt.store.len(), 1);
    let node = mpt.store.get(&Prefix::from_hash(key)).unwrap();
    match node {
        Node::Leaf(leaf) => {
            assert_eq!(leaf.key, key);
            assert_eq!(leaf.value, value2);
        }
        _ => panic!("Expected leaf node"),
    }
}

#[test]
fn test_two_inserts() {
    let mut mpt = SimpleMPT::new();
    let key1 = create_hash(1);
    let key2 = create_hash(2);
    let value1 = create_hash(101);
    let value2 = create_hash(102);

    mpt.upsert(key1, value1);
    mpt.upsert(key2, value2);

    // Should have interior node + 2 leaf nodes = 3 nodes
    assert_eq!(mpt.store.len(), 3);

    // Root should be an interior node
    let root_node = mpt.store.get(&mpt.root).unwrap();
    match root_node {
        Node::Interior(_) => {}
        _ => panic!("Expected interior node at root"),
    }
}

#[test]
fn test_insertion_order_independence() {
    // This test verifies that different insertion orders produce the same root hash
    // when the same set of keys and values are inserted.

    let key1 = create_hash(1);
    let key2 = create_hash(2);
    let key3 = create_hash(3);
    let value1 = create_hash(101);
    let value2 = create_hash(102);
    let value3 = create_hash(103);

    // First MPT: insert in order 1, 2, 3
    let mut mpt1 = SimpleMPT::new();
    mpt1.upsert(key1, value1);
    mpt1.upsert(key2, value2);
    mpt1.upsert(key3, value3);

    // Second MPT: insert in order 3, 1, 2
    let mut mpt2 = SimpleMPT::new();
    mpt2.upsert(key3, value3);
    mpt2.upsert(key1, value1);
    mpt2.upsert(key2, value2);

    // Third MPT: insert in order 2, 3, 1
    let mut mpt3 = SimpleMPT::new();
    mpt3.upsert(key2, value2);
    mpt3.upsert(key3, value3);
    mpt3.upsert(key1, value1);

    // Get root hashes
    let hash1 = mpt1.store.get(&mpt1.root).unwrap().merkle_hash();
    let hash2 = mpt2.store.get(&mpt2.root).unwrap().merkle_hash();
    let hash3 = mpt3.store.get(&mpt3.root).unwrap().merkle_hash();

    // All should have the same root hash
    assert_eq!(hash1, hash2, "MPT1 and MPT2 root hashes should match");
    assert_eq!(hash2, hash3, "MPT2 and MPT3 root hashes should match");
    assert_eq!(hash1, hash3, "MPT1 and MPT3 root hashes should match");
}

#[test]
fn test_larger_insertion_order_independence() {
    // This test verifies that different insertion orders produce the same root hash
    // for a larger set of keys.

    let keys_values: Vec<(Hash, Hash)> = (0..10)
        .map(|i| (create_hash(i), create_hash(i + 100)))
        .collect();

    // Create first MPT with keys in original order
    let mut mpt1 = SimpleMPT::new();
    for (key, value) in &keys_values {
        mpt1.upsert(*key, *value);
    }

    // Create second MPT with keys in reverse order
    let mut mpt2 = SimpleMPT::new();
    for (key, value) in keys_values.iter().rev() {
        mpt2.upsert(*key, *value);
    }

    // Get root hashes
    let hash1 = mpt1.store.get(&mpt1.root).unwrap().merkle_hash();
    let hash2 = mpt2.store.get(&mpt2.root).unwrap().merkle_hash();

    // Should produce the same hash
    assert_eq!(
        hash1, hash2,
        "Forward and reverse insertion should produce same hash"
    );
}

#[test]
fn test_merkle_hash_changes_with_data() {
    let mut mpt = SimpleMPT::new();
    let key = create_hash(1);
    let value1 = create_hash(101);
    let value2 = create_hash(102);

    mpt.upsert(key, value1);
    let hash1 = mpt.store.get(&mpt.root).unwrap().merkle_hash();

    mpt.upsert(key, value2);
    let hash2 = mpt.store.get(&mpt.root).unwrap().merkle_hash();

    assert_ne!(hash1, hash2, "Hash should change when value changes");
}

#[test]
fn test_leaf_node_hash_calculation() {
    let key = create_hash(1);
    let value = create_hash(101);

    let leaf1 = LeafNode::new(key, value);
    let leaf2 = LeafNode::new(key, value);

    // Same inputs should produce same hash
    assert_eq!(leaf1.merkle_hash, leaf2.merkle_hash);

    // Different value should produce different hash
    let leaf3 = LeafNode::new(key, create_hash(102));
    assert_ne!(leaf1.merkle_hash, leaf3.merkle_hash);
}

#[test]
fn test_interior_node_hash_calculation() {
    let prefix = Prefix::from_hash(create_hash(0));
    let left = Prefix::from_hash(create_hash(1));
    let right = Prefix::from_hash(create_hash(2));
    let left_hash = create_hash(101);
    let right_hash = create_hash(102);

    let interior1 = InteriorNode::new(prefix, left, right, left_hash, right_hash);
    let interior2 = InteriorNode::new(prefix, left, right, left_hash, right_hash);

    // Same inputs should produce same hash
    assert_eq!(interior1.merkle_hash, interior2.merkle_hash);

    // Different child hash should produce different hash
    let interior3 = InteriorNode::new(prefix, left, right, create_hash(103), right_hash);
    assert_ne!(interior1.merkle_hash, interior3.merkle_hash);
}

#[test]
fn test_multiple_updates_same_key() {
    let mut mpt = SimpleMPT::new();
    let key = create_hash(1);

    // Insert and update multiple times
    for i in 0..5 {
        mpt.upsert(key, create_hash(101 + i));
    }

    // Should still have only one node
    assert_eq!(mpt.store.len(), 1);

    let node = mpt.store.get(&Prefix::from_hash(key)).unwrap();
    match node {
        Node::Leaf(leaf) => {
            assert_eq!(leaf.key, key);
            assert_eq!(leaf.value, create_hash(105)); // Last value
        }
        _ => panic!("Expected leaf node"),
    }
}

#[test]
fn test_same_insertion_order_produces_same_hash() {
    // This test verifies that the same insertion order produces the same hash
    // which is expected behavior - the tree is deterministic for a given order.

    let key1 = create_hash(1);
    let key2 = create_hash(2);
    let key3 = create_hash(3);
    let value1 = create_hash(101);
    let value2 = create_hash(102);
    let value3 = create_hash(103);

    // First MPT: insert in order 1, 2, 3
    let mut mpt1 = SimpleMPT::new();
    mpt1.upsert(key1, value1);
    mpt1.upsert(key2, value2);
    mpt1.upsert(key3, value3);

    // Second MPT: same order 1, 2, 3
    let mut mpt2 = SimpleMPT::new();
    mpt2.upsert(key1, value1);
    mpt2.upsert(key2, value2);
    mpt2.upsert(key3, value3);

    let hash1 = mpt1.store.get(&mpt1.root).unwrap().merkle_hash();
    let hash2 = mpt2.store.get(&mpt2.root).unwrap().merkle_hash();

    assert_eq!(
        hash1, hash2,
        "Same insertion order should produce same hash"
    );
}

#[test]
fn test_tree_structure_consistency() {
    // Verify that the tree maintains proper structure
    let mut mpt = SimpleMPT::new();

    // Insert 5 keys
    for i in 0..5 {
        mpt.upsert(create_hash(i), create_hash(i + 100));
    }

    // Should have exactly 5 leaf nodes
    let leaf_count = mpt
        .store
        .values()
        .filter(|node| matches!(node, Node::Leaf(_)))
        .count();
    assert_eq!(leaf_count, 5);

    // Interior nodes should always have two children
    for node in mpt.store.values() {
        if let Node::Interior(interior) = node {
            // Both children should exist in the store
            assert!(
                mpt.store.contains_key(&interior.left),
                "Left child should exist in store"
            );
            assert!(
                mpt.store.contains_key(&interior.right),
                "Right child should exist in store"
            );
        }
    }
}

#[test]
fn test_merkle_hash_propagation() {
    // Test that changes to leaf values properly propagate to root hash
    let mut mpt = SimpleMPT::new();
    let key1 = create_hash(1);
    let key2 = create_hash(2);

    mpt.upsert(key1, create_hash(101));
    mpt.upsert(key2, create_hash(102));
    let hash_before = mpt.store.get(&mpt.root).unwrap().merkle_hash();

    // Update one value
    mpt.upsert(key1, create_hash(111));
    let hash_after = mpt.store.get(&mpt.root).unwrap().merkle_hash();

    assert_ne!(
        hash_before, hash_after,
        "Root hash should change when leaf value changes"
    );
}
