use super::*;

// Macro to run a test against all MPT implementations
//
// To add a new implementation, simply add a new line like:
//     test_impl::<NewMPTImpl>();
// inside the macro definition below.
macro_rules! test_all_impls {
    ($test_name:ident, $test_body:block) => {
        #[test]
        fn $test_name() {
            fn test_impl<T: MerklePatriciaTree>() $test_body

            let _ = env_logger::builder().is_test(true).filter(None, log::LevelFilter::Debug).try_init();
            // Add new implementations here as they're created
            test_impl::<SimpleMPT>();
            test_impl::<BatchMPT>();
            test_impl::<DurableBatchMPT>();
            test_impl::<SledBatchMPT>();
            test_impl::<SledLeafMPT>();
            test_impl::<SledAllMPT>();
        }
    };
}

macro_rules! for_each_impl {
    ($macro:ident) => {
        $macro!(SimpleMPT);
        $macro!(BatchMPT);
        $macro!(DurableBatchMPT);
        $macro!(SledBatchMPT);
        $macro!(SledLeafMPT);
        $macro!(SledAllMPT);
    };
}

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
    let mut count = 0;
    for x in mpt.enumerate_nodes()
        .iter()
        .filter(|(_, node)| matches!(node, Node::Leaf(_)))
    {
        println!("Leaf node: {:?}", x);
        count += 1;
    }
    count
}

// Helper function to verify interior nodes have valid children
fn verify_interior_node_structure<T: MerklePatriciaTree>(mpt: &T) -> bool {
    let nodes: std::collections::HashMap<Prefix, Node> =
        mpt.enumerate_nodes().into_iter().collect();

    for (key, node) in nodes.iter() {
        if let Node::Interior(interior) = node {
            let missing_left = !nodes.contains_key(&interior.left);
            let missing_right = !nodes.contains_key(&interior.right);
            if missing_left || missing_right {
                log::error!("Interior node {:?} is missing children", key);
                if missing_left {
                    log::error!("  Missing left child: {:?}", interior.left);
                }
                if missing_right {
                    log::error!("  Missing right child: {:?}", interior.right);
                }
                return false;
            }
        }
    }
    true
}

test_all_impls!(test_empty_tree, {
    let mpt = T::new();
    assert_eq!(count_nodes(&mpt), 0);
    assert_eq!(mpt.get_root_hash(), None);
});

test_all_impls!(test_single_insert, {
    let mut mpt = T::new();
    let key = create_hash(1);
    let value = create_hash(101);

    mpt.upsert(key, value);

    assert_eq!(count_nodes(&mpt), 1);
    assert_eq!(mpt.get_leaf_value(key), Some(value));
});

test_all_impls!(test_update_existing_key, {
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
});

test_all_impls!(test_two_inserts, {
    let mut mpt = T::new();
    let key1 = create_hash(1);
    let key2 = create_hash(2);
    let value1 = create_hash(101);
    let value2 = create_hash(102);

    mpt.upsert(key1, value1);
    mpt.upsert(key2, value2);

    // Should have interior node + 2 leaf nodes = 3 nodes
    for (prefix, _node) in mpt.enumerate_nodes() {
        println!("Node prefix: {:?}", prefix);
    }
    assert_eq!(count_nodes(&mpt), 3);

    // Both values should be retrievable
    assert_eq!(mpt.get_leaf_value(key1), Some(value1));
    assert_eq!(mpt.get_leaf_value(key2), Some(value2));
});

test_all_impls!(test_insertion_order_independence, {
    // This test verifies that different insertion orders produce the same root hash
    // when the same set of keys and values are inserted.
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
});

test_all_impls!(test_larger_insertion_order_independence, {
    // This test verifies that different insertion orders produce the same root hash
    // for a larger set of keys.
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
});

test_all_impls!(test_merkle_hash_changes_with_data, {
    let mut mpt = T::new();
    let key = create_hash(1);
    let value1 = create_hash(101);
    let value2 = create_hash(102);

    mpt.upsert(key, value1);
    let hash1 = mpt.get_root_hash();

    mpt.upsert(key, value2);
    let hash2 = mpt.get_root_hash();

    assert_ne!(hash1, hash2, "Hash should change when value changes");
});

// These tests don't use the trait, so they stay as regular #[test] functions
#[test]
fn test_cross_impl_root_hash_consistency() {
    let entries: Vec<(Hash, Hash)> = (0..10)
        .map(|i| (create_hash(i), create_hash(i + 100)))
        .collect();

    fn compute_root_hash<T: MerklePatriciaTree>(entries: &[(Hash, Hash)]) -> Option<Hash> {
        let mut tree = T::new();
        for (key, value) in entries {
            tree.upsert(*key, *value);
        }
        tree.get_root_hash()
    }

    fn compute_batch_root<T: MerklePatriciaTree>(entries: &[(Hash, Hash)]) -> Option<Hash> {
        let mut tree = T::new();
        tree.batch_upsert(entries);
        tree.get_root_hash()
    }

    let mut sequential_roots = std::collections::BTreeMap::new();

    macro_rules! collect_sequential_roots {
        ($ty:ty) => {
            let name = std::any::type_name::<$ty>();
            let root = compute_root_hash::<$ty>(&entries)
                .expect("each implementation should produce a root hash after inserts");
            sequential_roots.insert(name, root);
        };
    }

    for_each_impl!(collect_sequential_roots);

    let mut sequential_iter = sequential_roots.iter();
    let (first_name, first_root) = sequential_iter
        .next()
        .expect("at least one implementation should be tested");
    for (name, root) in sequential_iter {
        assert_eq!(
            first_root, root,
            "Root mismatch between {} and {} for sequential inserts",
            first_name, name
        );
    }

    let mut batch_roots = std::collections::BTreeMap::new();

    macro_rules! collect_batch_roots {
        ($ty:ty) => {
            let name = std::any::type_name::<$ty>();
            let root = compute_batch_root::<$ty>(&entries)
                .expect("each implementation should produce a root hash after batch inserts");
            batch_roots.insert(name, root);
        };
    }

    for_each_impl!(collect_batch_roots);

    for (name, batch_root) in batch_roots.iter() {
        let sequential_root = sequential_roots
            .get(name)
            .expect("batch root should have a corresponding sequential root");
        assert_eq!(
            sequential_root, batch_root,
            "Batch root mismatch for implementation {}",
            name
        );
    }
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

test_all_impls!(test_multiple_updates_same_key, {
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
});

test_all_impls!(test_same_insertion_order_produces_same_hash, {
    // This test verifies that the same insertion order produces the same hash
    // which is expected behavior - the tree is deterministic for a given order.
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
});

test_all_impls!(test_tree_structure_consistency, {
    // Verify that the tree maintains proper structure
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
});

test_all_impls!(test_merkle_hash_propagation, {
    // Test that changes to leaf values properly propagate to root hash
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
});

test_all_impls!(test_batch_upsert_empty, {
    let mut mpt = T::new();
    let entries: Vec<(Hash, Hash)> = vec![];

    mpt.batch_upsert(&entries);

    assert_eq!(count_nodes(&mpt), 0);
    assert_eq!(mpt.get_root_hash(), None);
});

test_all_impls!(test_batch_upsert_single, {
    let mut mpt = T::new();
    let entries = vec![(create_hash(1), create_hash(101))];

    mpt.batch_upsert(&entries);

    assert_eq!(count_nodes(&mpt), 1);
    assert_eq!(mpt.get_leaf_value(create_hash(1)), Some(create_hash(101)));
});

test_all_impls!(test_batch_upsert_multiple, {
    let mut mpt = T::new();
    let entries = vec![
        (create_hash(1), create_hash(101)),
        (create_hash(2), create_hash(102)),
        (create_hash(3), create_hash(103)),
    ];

    mpt.batch_upsert(&entries);

    assert_eq!(count_leaf_nodes(&mpt), 3);
    assert_eq!(mpt.get_leaf_value(create_hash(1)), Some(create_hash(101)));
    assert_eq!(mpt.get_leaf_value(create_hash(2)), Some(create_hash(102)));
    assert_eq!(mpt.get_leaf_value(create_hash(3)), Some(create_hash(103)));
});

test_all_impls!(test_batch_upsert_vs_individual, {
    // Verify that batch_upsert produces the same result as individual upserts
    let entries: Vec<(Hash, Hash)> = (0..10)
        .map(|i| (create_hash(i), create_hash(i + 100)))
        .collect();

    // Use batch_upsert
    let mut mpt_batch = T::new();
    mpt_batch.batch_upsert(&entries);

    // Use individual upserts
    let mut mpt_individual = T::new();
    for (key, value) in &entries {
        mpt_individual.upsert(*key, *value);
    }

    // Should produce the same root hash
    assert_eq!(
        mpt_batch.get_root_hash(),
        mpt_individual.get_root_hash(),
        "Batch and individual upserts should produce same hash"
    );

    // All values should be retrievable
    for (key, value) in &entries {
        assert_eq!(mpt_batch.get_leaf_value(*key), Some(*value));
        assert_eq!(mpt_individual.get_leaf_value(*key), Some(*value));
    }
});

test_all_impls!(test_batch_upsert_with_updates, {
    let mut mpt = T::new();

    // Initial batch insert
    let initial_entries = vec![
        (create_hash(1), create_hash(101)),
        (create_hash(2), create_hash(102)),
        (create_hash(3), create_hash(103)),
    ];
    mpt.batch_upsert(&initial_entries);

    let hash_before = mpt.get_root_hash();

    // Batch update with some new keys and some existing keys
    let update_entries = vec![
        (create_hash(1), create_hash(201)), // Update existing
        (create_hash(3), create_hash(203)), // Update existing
        (create_hash(4), create_hash(104)), // New key
        (create_hash(5), create_hash(105)), // New key
    ];
    mpt.batch_upsert(&update_entries);

    let hash_after = mpt.get_root_hash();

    // Hash should change
    assert_ne!(hash_before, hash_after);

    // Should have 5 leaf nodes total
    assert_eq!(count_leaf_nodes(&mpt), 5);

    // Verify all values
    assert_eq!(mpt.get_leaf_value(create_hash(1)), Some(create_hash(201)));
    assert_eq!(mpt.get_leaf_value(create_hash(2)), Some(create_hash(102)));
    assert_eq!(mpt.get_leaf_value(create_hash(3)), Some(create_hash(203)));
    assert_eq!(mpt.get_leaf_value(create_hash(4)), Some(create_hash(104)));
    assert_eq!(mpt.get_leaf_value(create_hash(5)), Some(create_hash(105)));
});

test_all_impls!(test_batch_upsert_larger_set, {
    let mut mpt = T::new();
    let entries: Vec<(Hash, Hash)> = (0..50)
        .map(|i| (create_hash(i), create_hash(i + 100)))
        .collect();

    mpt.batch_upsert(&entries);

    // Should have exactly 50 leaf nodes
    assert_eq!(count_leaf_nodes(&mpt), 50);

    // Tree structure should be consistent
    assert!(
        verify_interior_node_structure(&mpt),
        "All interior nodes should have valid children"
    );

    // All values should be retrievable
    for i in 0..50 {
        assert_eq!(
            mpt.get_leaf_value(create_hash(i)),
            Some(create_hash(i + 100))
        );
    }
});

test_all_impls!(test_batch_upsert_duplicate_keys_in_batch, {
    // Test that if the same key appears multiple times in a batch,
    // the last value wins
    let mut mpt = T::new();
    let entries = vec![
        (create_hash(1), create_hash(101)),
        (create_hash(1), create_hash(201)), // Duplicate key
        (create_hash(2), create_hash(102)),
        (create_hash(1), create_hash(111)), // Another duplicate
    ];

    mpt.batch_upsert(&entries);

    // The last value for key 1 should be retained (HashMap behavior)
    // Either way, there should be only 2 leaf nodes
    assert_eq!(count_leaf_nodes(&mpt), 2);

    // Key 1 should have one of the values (HashMap will keep the last insert)
    let value = mpt.get_leaf_value(create_hash(1));
    assert!(value.is_some());

    // Key 2 should have its value
    assert_eq!(mpt.get_leaf_value(create_hash(2)), Some(create_hash(102)));
});

test_all_impls!(test_batch_upsert_incremental, {
    // Test that multiple batch upserts work correctly
    let mut mpt = T::new();

    // First batch
    let batch1 = vec![
        (create_hash(1), create_hash(101)),
        (create_hash(2), create_hash(102)),
    ];
    mpt.batch_upsert(&batch1);
    assert_eq!(count_leaf_nodes(&mpt), 2);

    // Second batch
    let batch2 = vec![
        (create_hash(3), create_hash(103)),
        (create_hash(4), create_hash(104)),
    ];
    mpt.batch_upsert(&batch2);
    assert_eq!(count_leaf_nodes(&mpt), 4);

    // Third batch with updates
    let batch3 = vec![
        (create_hash(1), create_hash(201)),
        (create_hash(5), create_hash(105)),
    ];
    mpt.batch_upsert(&batch3);
    assert_eq!(count_leaf_nodes(&mpt), 5);

    // Verify final values
    assert_eq!(mpt.get_leaf_value(create_hash(1)), Some(create_hash(201)));
    assert_eq!(mpt.get_leaf_value(create_hash(2)), Some(create_hash(102)));
    assert_eq!(mpt.get_leaf_value(create_hash(3)), Some(create_hash(103)));
    assert_eq!(mpt.get_leaf_value(create_hash(4)), Some(create_hash(104)));
    assert_eq!(mpt.get_leaf_value(create_hash(5)), Some(create_hash(105)));
});
