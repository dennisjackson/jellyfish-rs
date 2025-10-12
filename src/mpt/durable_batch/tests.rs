use super::*;
use std::env;

#[test]
fn test_basic_insert_and_retrieve() {
    let mut mpt = DurableBatchMPT::new_in_memory_with_small_cache().unwrap();

    let key = [1u8; 32];
    let value = [2u8; 32];

    mpt.upsert(key, value);

    assert_eq!(mpt.get_leaf_value(key), Some(value));
}

#[test]
fn test_batch_upsert_persistence() {
    let mut mpt = DurableBatchMPT::new_in_memory_with_small_cache().unwrap();

    let entries: Vec<(Hash, Hash)> = vec![
        ([1u8; 32], [10u8; 32]),
        ([2u8; 32], [20u8; 32]),
        ([3u8; 32], [30u8; 32]),
    ];

    mpt.batch_upsert(&entries);

    // Clear cache to force reload from database
    mpt.clear_cache();

    for (key, value) in entries {
        assert_eq!(mpt.get_leaf_value(key), Some(value));
    }
}

#[test]
fn test_batch_upsert_with_cache_clearing() {
    let mut mpt = DurableBatchMPT::new_in_memory_with_small_cache().unwrap();

    // Insert enough entries to create a tree with multiple levels
    // Use diverse keys to spread across the tree
    let entries: Vec<(Hash, Hash)> = (0..100)
        .map(|i| {
            let mut key = [0u8; 32];
            let mut value = [0u8; 32];
            // Spread keys across the hash space
            key[0] = i;
            key[1] = (i.wrapping_mul(3)) as u8;
            value[0] = i * 2;
            (key, value)
        })
        .collect();

    mpt.batch_upsert(&entries);

    // Verify tree size is tracked
    let tree_size = mpt.cache.tree_size();
    assert!(tree_size > 0);
    assert!(tree_size > 100);

    // Clear cache to test loading from disk
    mpt.clear_cache();
    assert_eq!(mpt.cache.len(), 0);

    // Perform a small batch upsert with just a few keys
    let new_entries: Vec<(Hash, Hash)> =
        vec![([200u8; 32], [250u8; 32]), ([201u8; 32], [251u8; 32])];
    mpt.batch_upsert(&new_entries);

    // Verify all original values are still retrievable (from disk)
    for (key, value) in entries.iter() {
        assert_eq!(mpt.get_leaf_value(*key), Some(*value));
    }

    // Verify new values are correct
    for (key, value) in new_entries.iter() {
        assert_eq!(mpt.get_leaf_value(*key), Some(*value));
    }
}

#[test]
fn test_release_pre_advise_then_batch_upsert() {
    let _ = env_logger::builder()
        .is_test(true)
        .filter(None, log::LevelFilter::Debug)
        .try_init();
    let mut mpt = DurableBatchMPT::new();

    let key1 = [1u8; 32];
    let key2 = [2u8; 32];
    let key3 = [3u8; 32];

    let mut initial_entries: Vec<(Hash, Hash)> = Vec::new();
    for i in 0..1000 {
        let mut key = [0u8; 32];
        let mut value = [0u8; 32];
        key[0] = (i / 256) as u8;
        key[1] = (i % 256) as u8;
        value[0] = (i % 128) as u8;
        initial_entries.push((key, value));
    }

    initial_entries.push((key1, [10u8; 32]));
    initial_entries.push((key2, [20u8; 32]));
    initial_entries.push((key3, [30u8; 32]));
    mpt.batch_upsert(&initial_entries);

    mpt.cache.release_keys(&[]);

    let updated_value_for_key2 = [200u8; 32];
    let key4 = [4u8; 32];
    let value4 = [40u8; 32];

    let second_batch: Vec<(Hash, Hash)> = vec![(key2, updated_value_for_key2), (key4, value4)];
    mpt.batch_upsert(&second_batch);

    assert_eq!(mpt.get_leaf_value(key1), Some([10u8; 32]));
    assert_eq!(mpt.get_leaf_value(key2), Some(updated_value_for_key2));
    assert_eq!(mpt.get_leaf_value(key3), Some([30u8; 32]));
    assert_eq!(mpt.get_leaf_value(key4), Some(value4));
}

#[test]
fn test_root_persisted_across_restarts() {
    let temp_dir = env::temp_dir();
    let pid = std::process::id();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = temp_dir.join(format!("durable_batch_root_test_{}_{}.db", pid, timestamp));
    let db_path_str = db_path.to_string_lossy().to_string();

    let key = [42u8; 32];
    let value = [99u8; 32];

    let expected_root = {
        let mut first = DurableBatchMPT::new_with_path(&db_path_str).unwrap();
        first.upsert(key, value);
        let expected_root = first.get_root_hash();
        assert!(expected_root.is_some());
        expected_root
    };

    {
        let second = DurableBatchMPT::new_with_path(&db_path_str).unwrap();
        let persisted_root = second.get_root_hash();
        assert!(persisted_root.is_some());
        assert_eq!(persisted_root, expected_root);
        assert_eq!(second.get_leaf_value(key), Some(value));
    }

    let _ = std::fs::remove_file(&db_path);
    let wal_path = db_path.with_extension("db-wal");
    let shm_path = db_path.with_extension("db-shm");
    let _ = std::fs::remove_file(wal_path);
    let _ = std::fs::remove_file(shm_path);
}

#[test]
fn test_new_existing_with_path_requires_initialized_database() {
    let temp_dir = env::temp_dir();
    let pid = std::process::id();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = temp_dir.join(format!(
        "durable_batch_existing_missing_{}_{}.db",
        pid, timestamp
    ));
    let db_path_str = db_path.to_string_lossy().to_string();

    let result = DurableBatchMPT::new_existing_with_path(&db_path_str);
    assert!(result.is_err());
}

#[test]
fn test_new_existing_with_path_rejects_uninitialized_file() {
    let temp_dir = env::temp_dir();
    let pid = std::process::id();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = temp_dir.join(format!(
        "durable_batch_existing_uninit_{}_{}.db",
        pid, timestamp
    ));
    let db_path_str = db_path.to_string_lossy().to_string();

    std::fs::File::create(&db_path).unwrap();

    let result = DurableBatchMPT::new_existing_with_path(&db_path_str);
    assert!(result.is_err());

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_new_existing_with_path_succeeds_for_seeded_database() {
    let temp_dir = env::temp_dir();
    let pid = std::process::id();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = temp_dir.join(format!(
        "durable_batch_existing_seeded_{}_{}.db",
        pid, timestamp
    ));
    let db_path_str = db_path.to_string_lossy().to_string();

    let key = [7u8; 32];
    let value = [9u8; 32];

    {
        let mut builder = DurableBatchMPT::new_with_path(&db_path_str).unwrap();
        builder.upsert(key, value);
        assert_eq!(builder.get_leaf_value(key), Some(value));
    }

    let loaded = DurableBatchMPT::new_existing_with_path(&db_path_str).unwrap();
    assert_eq!(loaded.get_leaf_value(key), Some(value));
    drop(loaded);

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
}
