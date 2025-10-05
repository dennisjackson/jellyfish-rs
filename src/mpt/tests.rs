use super::*;

fn create_hash(value: u8) -> Hash {
    let mut hash = [0u8; 32];
    hash[0] = value;
    hash
}

// Helper function to count nodes in an MPT
fn count_nodes<T: MerklePatriciaTree>(mpt: &T) -> usize {
    mpt.enumerate_nodes().len()
}

// Helper function to count leaf nodes
fn count_leaf_nodes<T: MerklePatriciaTree>(mpt: &T) -> usize {
    mpt.enumerate_nodes()
        .iter()
        .filter(|(_, node)| matches!(node, Node::Leaf(_)))
        .count()
}

// Helper function to verify interior nodes have valid children
fn verify_interior_node_structure<T: MerklePatriciaTree>(mpt: &T) -> bool {
    let nodes: std::collections::HashMap<Prefix, Node> =
        mpt.enumerate_nodes().into_iter().collect();

    for (_, node) in &nodes {
        if let Node::Interior(interior) = node {
            if !nodes.contains_key(&interior.left) || !nodes.contains_key(&interior.right) {
                return false;
            }
        }
    }
    true
}

#[test]
fn test_empty_tree() {
    fn test_impl<T: MerklePatriciaTree>() {
        let mpt = T::new();
        assert_eq!(count_nodes(&mpt), 0);
        assert_eq!(mpt.get_root_hash(), None);
    }

    test_impl::<SimpleMPT>();
}

#[test]
fn test_single_insert() {
    fn test_impl<T: MerklePatriciaTree>() {
        let mut mpt = T::new();
        let key = create_hash(1);
        let value = create_hash(101);

        mpt.upsert(key, value);

        assert_eq!(count_nodes(&mpt), 1);
        assert_eq!(mpt.get_leaf_value(key), Some(value));
    }

    test_impl::<SimpleMPT>();
}

#[test]
fn test_update_existing_key() {
    fn test_impl<T: MerklePatriciaTree>() {
        let mut mpt = T::new();
        let key = create_hash(1);
        let value1 = create_hash(101);
        let value2 = create_hash(102);

        mpt.upsert(key, value1);
        let hash1 = mpt.get_root_hash();

        mpt.upsert(key, value2);
        let hash2 = mpt.get_root_hash();

        // Hash should change when value changes
        assert_ne!(hash1, hash2);

        // Should still have only one leaf node
        assert_eq!(count_nodes(&mpt), 1);
        assert_eq!(mpt.get_leaf_value(key), Some(value2));
    }

    test_impl::<SimpleMPT>();
}

#[test]
fn test_two_inserts() {
    fn test_impl<T: MerklePatriciaTree>() {
        let mut mpt = T::new();
        let key1 = create_hash(1);
        let key2 = create_hash(2);
        let value1 = create_hash(101);
        let value2 = create_hash(102);

        mpt.upsert(key1, value1);
        mpt.upsert(key2, value2);

        // Should have interior node + 2 leaf nodes = 3 nodes
        assert_eq!(count_nodes(&mpt), 3);

        // Both values should be retrievable
        assert_eq!(mpt.get_leaf_value(key1), Some(value1));
        assert_eq!(mpt.get_leaf_value(key2), Some(value2));
    }

    test_impl::<SimpleMPT>();
}

#[test]
fn test_insertion_order_independence() {
    // This test verifies that different insertion orders produce the same root hash
    // when the same set of keys and values are inserted.
    fn test_impl<T: MerklePatriciaTree>() {
        let key1 = create_hash(1);
        let key2 = create_hash(2);
        let key3 = create_hash(3);
        let value1 = create_hash(101);
        let value2 = create_hash(102);
        let value3 = create_hash(103);

        // First MPT: insert in order 1, 2, 3
        let mut mpt1 = T::new();
        mpt1.upsert(key1, value1);
        mpt1.upsert(key2, value2);
        mpt1.upsert(key3, value3);

        // Second MPT: insert in order 3, 1, 2
        let mut mpt2 = T::new();
        mpt2.upsert(key3, value3);
        mpt2.upsert(key1, value1);
        mpt2.upsert(key2, value2);

        // Third MPT: insert in order 2, 3, 1
        let mut mpt3 = T::new();
        mpt3.upsert(key2, value2);
        mpt3.upsert(key3, value3);
        mpt3.upsert(key1, value1);

        // Get root hashes
        let hash1 = mpt1.get_root_hash();
        let hash2 = mpt2.get_root_hash();
        let hash3 = mpt3.get_root_hash();

        // All should have the same root hash
        assert_eq!(hash1, hash2, "MPT1 and MPT2 root hashes should match");
        assert_eq!(hash2, hash3, "MPT2 and MPT3 root hashes should match");
        assert_eq!(hash1, hash3, "MPT1 and MPT3 root hashes should match");
    }

    test_impl::<SimpleMPT>();
}

#[test]
fn test_larger_insertion_order_independence() {
    // This test verifies that different insertion orders produce the same root hash
    // for a larger set of keys.
    fn test_impl<T: MerklePatriciaTree>() {
        let keys_values: Vec<(Hash, Hash)> = (0..10)
            .map(|i| (create_hash(i), create_hash(i + 100)))
            .collect();

        // Create first MPT with keys in original order
        let mut mpt1 = T::new();
        for (key, value) in &keys_values {
            mpt1.upsert(*key, *value);
        }

        // Create second MPT with keys in reverse order
        let mut mpt2 = T::new();
        for (key, value) in keys_values.iter().rev() {
            mpt2.upsert(*key, *value);
        }

        // Get root hashes
        let hash1 = mpt1.get_root_hash();
        let hash2 = mpt2.get_root_hash();

        // Should produce the same hash
        assert_eq!(
            hash1, hash2,
            "Forward and reverse insertion should produce same hash"
        );
    }

    test_impl::<SimpleMPT>();
}

#[test]
fn test_merkle_hash_changes_with_data() {
    fn test_impl<T: MerklePatriciaTree>() {
        let mut mpt = T::new();
        let key = create_hash(1);
        let value1 = create_hash(101);
        let value2 = create_hash(102);

        mpt.upsert(key, value1);
        let hash1 = mpt.get_root_hash();

        mpt.upsert(key, value2);
        let hash2 = mpt.get_root_hash();

        assert_ne!(hash1, hash2, "Hash should change when value changes");
    }

    test_impl::<SimpleMPT>();
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
    let prefix = Prefix::from(create_hash(0));
    let left = Prefix::from(create_hash(1));
    let right = Prefix::from(create_hash(2));
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
    fn test_impl<T: MerklePatriciaTree>() {
        let mut mpt = T::new();
        let key = create_hash(1);

        // Insert and update multiple times
        for i in 0..5 {
            mpt.upsert(key, create_hash(101 + i));
        }

        // Should still have only one node
        assert_eq!(count_nodes(&mpt), 1);

        // Should have the last value
        assert_eq!(mpt.get_leaf_value(key), Some(create_hash(105)));
    }

    test_impl::<SimpleMPT>();
}

#[test]
fn test_same_insertion_order_produces_same_hash() {
    // This test verifies that the same insertion order produces the same hash
    // which is expected behavior - the tree is deterministic for a given order.
    fn test_impl<T: MerklePatriciaTree>() {
        let key1 = create_hash(1);
        let key2 = create_hash(2);
        let key3 = create_hash(3);
        let value1 = create_hash(101);
        let value2 = create_hash(102);
        let value3 = create_hash(103);

        // First MPT: insert in order 1, 2, 3
        let mut mpt1 = T::new();
        mpt1.upsert(key1, value1);
        mpt1.upsert(key2, value2);
        mpt1.upsert(key3, value3);

        // Second MPT: same order 1, 2, 3
        let mut mpt2 = T::new();
        mpt2.upsert(key1, value1);
        mpt2.upsert(key2, value2);
        mpt2.upsert(key3, value3);

        let hash1 = mpt1.get_root_hash();
        let hash2 = mpt2.get_root_hash();

        assert_eq!(
            hash1, hash2,
            "Same insertion order should produce same hash"
        );
    }

    test_impl::<SimpleMPT>();
}

#[test]
fn test_tree_structure_consistency() {
    // Verify that the tree maintains proper structure
    fn test_impl<T: MerklePatriciaTree>() {
        let mut mpt = T::new();

        // Insert 5 keys
        for i in 0..5 {
            mpt.upsert(create_hash(i), create_hash(i + 100));
        }

        // Should have exactly 5 leaf nodes
        let leaf_count = count_leaf_nodes(&mpt);
        assert_eq!(leaf_count, 5);

        // Interior nodes should always have two children
        assert!(
            verify_interior_node_structure(&mpt),
            "All interior nodes should have valid children"
        );
    }

    test_impl::<SimpleMPT>();
}

#[test]
fn test_merkle_hash_propagation() {
    // Test that changes to leaf values properly propagate to root hash
    fn test_impl<T: MerklePatriciaTree>() {
        let mut mpt = T::new();
        let key1 = create_hash(1);
        let key2 = create_hash(2);

        mpt.upsert(key1, create_hash(101));
        mpt.upsert(key2, create_hash(102));
        let hash_before = mpt.get_root_hash();

        // Update one value
        mpt.upsert(key1, create_hash(111));
        let hash_after = mpt.get_root_hash();

        assert_ne!(
            hash_before, hash_after,
            "Root hash should change when leaf value changes"
        );
    }

    test_impl::<SimpleMPT>();
}
