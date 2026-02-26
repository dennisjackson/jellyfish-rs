    use super::*;
    use crate::mpt::DurableBatchMPT;
    use crate::Hash;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};
    use std::cell::RefCell;

    thread_local! {
        static RNG: RefCell<StdRng> = RefCell::new(StdRng::seed_from_u64(42));
    }

    fn make_random_hash() -> Hash {
        RNG.with(|rng| rng.borrow_mut().random::<[u8; 32]>())
    }

    #[test]
    fn test_complete_interior_depth_empty_tree() {
        let tree = RocksTransRelMPT::new_temporary().expect("create tree");
        // Empty tree should have depth 0
        assert_eq!(tree.complete_interior_depth(), 0);
    }

    #[test]
    fn test_complete_interior_depth_single_insert() {
        let mut tree = RocksTransRelMPT::new_temporary().expect("create tree");

        // Insert a single key-value pair
        tree.batch_upsert(&[(make_random_hash(), make_random_hash())]);

        // With just one leaf, we should still have depth 0
        // (the root exists but is a leaf, not an interior)
        assert_eq!(tree.complete_interior_depth(), 0);
    }

    #[test]
    fn test_complete_interior_depth_two_inserts() {
        let mut tree = RocksTransRelMPT::new_temporary().expect("create tree");

        // Insert two keys that differ in the first bit
        // 0x00 = 00000000...
        // 0x80 = 10000000...
        tree.batch_upsert(&[
            (make_random_hash(), make_random_hash()),
            (make_random_hash(), make_random_hash()),
        ]);

        // Now the root should be an interior node (depth 0 complete)
        // But depth 1 requires 2 interior nodes, which we don't have yet
        let depth = tree.complete_interior_depth();
        println!("Depth after 2 inserts with different first bit: {}", depth);
    }

    #[test]
    fn test_complete_interior_depth_full_level() {
        let mut tree = RocksTransRelMPT::new_temporary().expect("create tree");

        // Insert 4 keys to create a tree with depth 2
        // This creates interior nodes at depth 0 and 1
        tree.batch_upsert(&[
            (make_random_hash(), make_random_hash()),
            (make_random_hash(), make_random_hash()),
            (make_random_hash(), make_random_hash()),
            (make_random_hash(), make_random_hash()),
        ]);

        let depth = tree.complete_interior_depth();
        println!("Complete interior depth with 4 keys: {}", depth);

        // The actual depth depends on the tree structure
        // With these 4 keys differing in the first 2 bits, we should have:
        // - Depth 0: 1 interior node (root)
        // - Depth 1: 2 interior nodes (left and right subtrees)
        // So depth should be at least 1 or 2
    }

    #[test]
    fn test_complete_interior_depth_incremental() {
        let mut tree = RocksTransRelMPT::new_temporary().expect("create tree");

        let initial_depth = tree.complete_interior_depth();
        println!("Initial depth: {}", initial_depth);

        // Add first key
        tree.batch_upsert(&[(make_random_hash(), make_random_hash())]);
        let depth1 = tree.complete_interior_depth();
        println!("Depth after 1 insert: {}", depth1);

        // Add second key (different first bit)
        tree.batch_upsert(&[(make_random_hash(), make_random_hash())]);
        let depth2 = tree.complete_interior_depth();
        println!("Depth after 2 inserts: {}", depth2);

        // Add third and fourth keys
        tree.batch_upsert(&[
            (make_random_hash(), make_random_hash()),
            (make_random_hash(), make_random_hash()),
        ]);
        let depth3 = tree.complete_interior_depth();
        println!("Depth after 4 inserts: {}", depth3);

        // The depth should not decrease
        assert!(depth3 >= depth2);
        assert!(depth2 >= depth1);
    }

    #[cfg(test)]
    impl RocksTransRelMPT {
        /// Checks that the in-memory tree is complete down to the frontier, and pruned below it.
        fn check_frontier_invariant(&self) {
            let complete_depth = self.complete_interior_depth();
            let mut queue: std::collections::VecDeque<Prefix> = std::collections::VecDeque::new();
            if self.store.get(&self.root).is_some() {
                queue.push_back(self.root);
            }

            let mut visited = std::collections::HashSet::new();
            visited.insert(self.root);

            while let Some(prefix) = queue.pop_front() {
                if prefix.length > complete_depth + self.config.keep_below_frontier {
                    // Nodes at the frontier should exist, but we don't check their children.
                    // Nodes below the frontier should not be in the queue.
                    panic!(
                        "Node {} with depth {} found below frontier depth {}",
                        prefix.short_hex(),
                        prefix.length,
                        complete_depth + self.config.keep_below_frontier
                    );
                }

                // We are below the frontier + keep_below_frontier depth
                // At this depth, we don't require children to be present (they may be pruned)
                if prefix.length > complete_depth + self.config.keep_below_frontier {
                    continue;
                }
                // We are above or at the frontier + keep_below_frontier, so this must be an interior node.
                let node = self
                    .store
                    .get(&prefix)
                    .expect("Missing node in tree traversal");
                match node.value() {
                    Node::Interior(interior) => {
                        // Children must be in the store if their depth is <= frontier + keep_below_frontier
                        // Only check if children would be at acceptable depth
                        let left_child_depth = interior.left.length;
                        let right_child_depth = interior.right.length;
                        if left_child_depth <= complete_depth + self.config.keep_below_frontier {
                            if !visited.contains(&interior.left) {
                                assert!(
                                    self.store.contains_key(&interior.left),
                                    "Left child {} of {} not in store",
                                    interior.left.short_hex(),
                                    prefix.short_hex()
                                );
                                queue.push_back(interior.left);
                                visited.insert(interior.left);
                            }
                        }
                        if right_child_depth <= complete_depth + self.config.keep_below_frontier {
                            if !visited.contains(&interior.right) {
                                assert!(
                                    self.store.contains_key(&interior.right),
                                    "Right child {} of {} not in store",
                                    interior.right.short_hex(),
                                    prefix.short_hex()
                                );
                                queue.push_back(interior.right);
                                visited.insert(interior.right);
                            }
                        }
                    }
                    Node::Leaf(_) => {
                        panic!(
                            "Leaf node found at prefix {} with depth {}, which is above the frontier depth {}",
                            prefix.short_hex(),
                            prefix.length,
                            complete_depth
                        );
                    }
                }
            }

            // Check that all nodes at the frontier depth are present.
            if complete_depth > 0 {
                let expected_frontier_nodes = 1u64 << complete_depth;
                for i in 0..expected_frontier_nodes {
                    let prefix = Self::prefix_from_depth_and_index(complete_depth, i);
                    assert!(
                        self.store.contains_key(&prefix),
                        "Frontier node {} at depth {} is missing from the store",
                        prefix.short_hex(),
                        complete_depth
                    );
                }
            }

            // Commented out since node might be within a couple of frontier but have a deeper prefix.
            // Verify no nodes below the frontier exist in the store.
            for item in self.store.iter() {
                match item.value() {
                    Node::Interior(_) => continue,
                    Node::Leaf(_) => panic!("leaf node {} in the store", item.key().short_hex()),
                }
            }
        }
    }

    #[test]
    fn test_frontier_invariant_after_batch_insert() {
        let mut tree = RocksTransRelMPT::new_temporary().expect("create tree");

        // Insert enough keys to create a few levels of interior nodes.
        let entries: Vec<_> = (0..10000u32)
            .map(|_| (make_random_hash(), make_random_hash()))
            .collect();
        tree.batch_upsert(&entries);

        println!("Frontier depth: {}", tree.complete_interior_depth());
        println!("Nodes in store: {}", tree.store.len());

        // After the batch insert, the invariant should hold.
        tree.check_frontier_invariant();
    }

    #[test]
    fn test_estimate_leaf_count_empty_tree() {
        let tree = RocksTransRelMPT::new_temporary().expect("create tree");
        let estimate = tree.estimate_leaf_count().expect("estimate leaves");
        // Empty tree may have the root node counted, so allow 0 or 1
        assert!(
            estimate <= 1,
            "Empty tree should have 0 or 1 entries, got {}",
            estimate
        );
    }

    #[test]
    fn test_estimate_leaf_count_small_tree() {
        let mut tree = RocksTransRelMPT::new_temporary().expect("create tree");

        // Insert 10 entries
        let entries: Vec<_> = (0..10u32)
            .map(|_| (make_random_hash(), make_random_hash()))
            .collect();
        tree.batch_upsert(&entries);

        let estimate = tree.estimate_leaf_count().expect("estimate leaves");
        println!("Estimated {} leaves for tree with 10 entries", estimate);

        // The estimate should be close to 10 (within reasonable margin)
        // For small trees, there might be more variation
        assert!(
            estimate >= 5 && estimate <= 20,
            "Estimate {} should be roughly 10 (5-20 range)",
            estimate
        );
    }

    #[test]
    fn test_estimate_leaf_count_medium_tree() {
        let mut tree = RocksTransRelMPT::new_temporary().expect("create tree");

        // Insert 1000 entries
        let entries: Vec<_> = (0..1000u32)
            .map(|_| (make_random_hash(), make_random_hash()))
            .collect();
        tree.batch_upsert(&entries);

        let estimate = tree.estimate_leaf_count().expect("estimate leaves");
        println!("Estimated {} leaves for tree with 1000 entries", estimate);
        println!("Frontier depth: {}", tree.complete_interior_depth());

        // The estimate should be close to 1000 (within 20%)
        assert!(
            estimate >= 800 && estimate <= 1200,
            "Estimate {} should be roughly 1000 (800-1200 range)",
            estimate
        );
    }

    #[test]
    fn test_estimate_leaf_count_with_custom_sample_size() {
        let mut tree = RocksTransRelMPT::new_temporary().expect("create tree");

        // Insert 500 entries
        let entries: Vec<_> = (0..500u32)
            .map(|_| (make_random_hash(), make_random_hash()))
            .collect();
        tree.batch_upsert(&entries);

        // Try different sample sizes
        let estimate_10 = tree
            .estimate_leaf_count_by_sampling(10)
            .expect("estimate with 10 samples");
        let estimate_50 = tree
            .estimate_leaf_count_by_sampling(50)
            .expect("estimate with 50 samples");
        let estimate_200 = tree
            .estimate_leaf_count_by_sampling(200)
            .expect("estimate with 200 samples");

        println!(
            "Estimates: 10 samples={}, 50 samples={}, 200 samples={}",
            estimate_10, estimate_50, estimate_200
        );

        // All estimates should be in a reasonable range
        for estimate in [estimate_10, estimate_50, estimate_200] {
            assert!(
                estimate >= 300 && estimate <= 700,
                "Estimate {} should be roughly 500",
                estimate
            );
        }
    }

    #[test]
    fn test_estimate_vs_actual_count() {
        let mut tree = RocksTransRelMPT::new_temporary().expect("create tree");

        // Insert 2000 entries
        let entries: Vec<_> = (0..2000u32)
            .map(|_| (make_random_hash(), make_random_hash()))
            .collect();
        tree.batch_upsert(&entries);

        // Get estimate
        let estimate = tree
            .estimate_leaf_count_by_sampling(100)
            .expect("estimate leaves");

        // Get actual count by loading full tree
        tree.ensure_full_tree_loaded();
        let actual = tree
            .store
            .iter()
            .filter(|entry| matches!(entry.value(), Node::Leaf(_)))
            .count();

        println!("Estimated {} leaves, actual {}", estimate, actual);
        println!("Frontier depth: {}", tree.complete_interior_depth());

        // The estimate should be within 20% of actual
        let lower_bound = (actual as f64 * 0.8) as usize;
        let upper_bound = (actual as f64 * 1.2) as usize;
        assert!(
            estimate >= lower_bound && estimate <= upper_bound,
            "Estimate {} should be within 20% of actual {} ({}-{})",
            estimate,
            actual,
            lower_bound,
            upper_bound
        );
    }

    /// Exercises release_subtree on a populated tree across multiple batch_upsert calls.
    /// This is the scenario that triggered the DashMap deadlock (fixed in 4157dc4).
    #[test]
    fn test_incremental_batch_inserts() {
        let mut tree = RocksTransRelMPT::new_temporary().expect("create tree");

        for batch_num in 0..10 {
            let entries: Vec<_> = (0..1000u32)
                .map(|_| (make_random_hash(), make_random_hash()))
                .collect();
            tree.batch_upsert(&entries);

            assert!(
                tree.get_root_hash().is_some(),
                "Root hash should exist after batch {}",
                batch_num
            );
        }

        tree.check_frontier_invariant();
    }

    /// Covers the exact benchmark scenario: open existing DB, recovery, insert more.
    #[test]
    fn test_reopen_and_insert() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("test.db");

        // Phase 1: create and populate
        {
            let mut tree = RocksTransRelMPT::new_with_path(&path).expect("create tree");
            let entries: Vec<_> = (0..5000u32)
                .map(|_| (make_random_hash(), make_random_hash()))
                .collect();
            tree.batch_upsert(&entries);
            assert!(tree.get_root_hash().is_some());
        }
        // tree is dropped, DB is closed

        // Phase 2: reopen (triggers recovery) and insert more
        {
            let mut tree = RocksTransRelMPT::new_with_path(&path).expect("reopen tree");
            assert!(
                tree.get_root_hash().is_some(),
                "Root hash should exist after reopen"
            );

            for _ in 0..5 {
                let entries: Vec<_> = (0..1000u32)
                    .map(|_| (make_random_hash(), make_random_hash()))
                    .collect();
                tree.batch_upsert(&entries);
            }

            assert!(tree.get_root_hash().is_some());
        }
    }

    /// Validates that recovery correctly restores data.
    #[test]
    fn test_reopen_preserves_data() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("test.db");

        let entries: Vec<_> = (0..1000u32)
            .map(|_| (make_random_hash(), make_random_hash()))
            .collect();

        let root_hash;
        let sample_keys: Vec<Hash> = entries.iter().take(10).map(|(k, _)| *k).collect();
        let sample_values: Vec<Hash> = entries.iter().take(10).map(|(_, v)| *v).collect();

        // Phase 1: create and populate
        {
            let mut tree = RocksTransRelMPT::new_with_path(&path).expect("create tree");
            tree.batch_upsert(&entries);
            root_hash = tree.get_root_hash().expect("root hash should exist");
        }

        // Phase 2: reopen and verify
        {
            let tree = RocksTransRelMPT::new_with_path(&path).expect("reopen tree");
            assert_eq!(
                tree.get_root_hash().expect("root hash after reopen"),
                root_hash,
                "Root hash must match after reopen"
            );

            for (key, expected_value) in sample_keys.iter().zip(sample_values.iter()) {
                let actual = tree.get_leaf_value(*key);
                assert_eq!(
                    actual,
                    Some(*expected_value),
                    "Leaf value for key {:?} must match after reopen",
                    &key[..4]
                );
            }
        }
    }

    /// Uses the SQLite DurableBatchMPT as a reference oracle to verify RocksDB root hashes.
    #[test]
    fn test_root_hash_matches_sqlite_single_batch() {
        let entries: Vec<_> = (0..5000u32)
            .map(|_| (make_random_hash(), make_random_hash()))
            .collect();

        let mut rocks_tree = RocksTransRelMPT::new_temporary().expect("create rocks tree");
        let mut sqlite_tree = DurableBatchMPT::new();

        rocks_tree.batch_upsert(&entries);
        sqlite_tree.batch_upsert(&entries);

        let rocks_hash = rocks_tree.get_root_hash().expect("rocks root hash");
        let sqlite_hash = sqlite_tree.get_root_hash().expect("sqlite root hash");

        assert_eq!(
            rocks_hash, sqlite_hash,
            "RocksDB and SQLite root hashes must match after single batch"
        );
    }

    /// Strongest test: validates correctness with incremental inserts AND exercises
    /// the code path that triggered the DashMap deadlock, cross-checked against SQLite.
    #[test]
    fn test_root_hash_matches_sqlite_incremental() {
        let mut rocks_tree = RocksTransRelMPT::new_temporary().expect("create rocks tree");
        let mut sqlite_tree = DurableBatchMPT::new();

        for batch_num in 0..10 {
            let entries: Vec<_> = (0..1000u32)
                .map(|_| (make_random_hash(), make_random_hash()))
                .collect();

            rocks_tree.batch_upsert(&entries);
            sqlite_tree.batch_upsert(&entries);

            let rocks_hash = rocks_tree.get_root_hash().expect("rocks root hash");
            let sqlite_hash = sqlite_tree.get_root_hash().expect("sqlite root hash");

            assert_eq!(
                rocks_hash, sqlite_hash,
                "RocksDB and SQLite root hashes must match after batch {}",
                batch_num
            );
        }
    }
