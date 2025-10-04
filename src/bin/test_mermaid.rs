use jellyfish_rs::{Hash, Node, Prefix, SimpleMPT};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;

fn hash_key(key: &str) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hasher.finalize().into()
}

fn hash_value(value: &str) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hasher.finalize().into()
}

fn append_mermaid_to_file(mpt: &SimpleMPT, path: &str, step: usize, key: &str, value: &str) {
    let mut buffer = Vec::new();
    buffer.extend_from_slice(format!("\n## Step {}: Insert ('{}', '{}')\n\n", step, key, value).as_bytes());
    buffer.extend_from_slice(b"```mermaid\n");
    buffer.extend_from_slice(b"graph TD\n");
    buffer.extend_from_slice(collect_mermaid_node(mpt).as_bytes());
    buffer.extend_from_slice(b"```\n");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(path)
        .unwrap();
    file.write_all(&buffer).unwrap();
}

fn collect_mermaid_node(mpt: &SimpleMPT) -> String {
    use std::collections::HashMap;
    let mut lines = String::new();
    let mut name_cache: HashMap<Prefix, String> = HashMap::new();
    // Helper to get or create a friendly name for a prefix
    let mut get_name = |prefix: &Prefix, node: &Node| -> String {
        if let Some(name) = name_cache.get(prefix) {
            return name.clone();
        }
        let name = match node {
            Node::Leaf(leaf) => format!("leaf_{}", hex::encode(&leaf.key[..4])),
            Node::Interior(interior) => format!("int_{}", hex::encode(&interior.prefix.hash[..4])),
        };
        name_cache.insert(prefix.clone(), name.clone());
        name
    };
    for (prefix, node) in mpt.store.iter() {
        let this_name = get_name(prefix, node);
        match node {
            Node::Leaf(leaf) => {
                lines.push_str(&format!(
                    "  {}[\"Leaf\\nkey:{}\"]\n",
                    this_name,
                    hex::encode(&leaf.key[..4])
                ));
            }
            Node::Interior(interior) => {
                lines.push_str(&format!(
                    "  {}[\"Interior\\nprefix:{}\"]\n",
                    this_name,
                    hex::encode(&interior.prefix.hash[..4])
                ));
                // Edges to children
                if let Some(left_node) = mpt.store.get(&interior.left) {
                    let left_name = get_name(&interior.left, left_node);
                    lines.push_str(&format!("  {} --> {}\n", this_name, left_name));
                }
                if let Some(right_node) = mpt.store.get(&interior.right) {
                    let right_name = get_name(&interior.right, right_node);
                    lines.push_str(&format!("  {} --> {}\n", this_name, right_name));
                }
            }
        }
    }
    lines
}

fn main() {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Debug)
        .init();

    let output_file = "mpt_diagrams.md";

    // Create/truncate the file and write the title
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(output_file)
        .unwrap();
    file.write_all(b"# MPT Evolution Diagrams\n").unwrap();
    drop(file); // Close the file so we can append to it later

    let mut mpt = SimpleMPT::new();
    let items = vec![
        ("apple", "red"),
        ("banana", "yellow"),
        ("grape", "purple"),
        ("lemon", "yellow"),
        // ("lime", "green"),
        // ("orange", "orange"),
        // ("blueberry", "blue"),
        // ("strawberry", "red"),
        // ("lemon", "pink")
    ];
    for (i, (k, v)) in items.iter().enumerate() {
        let key = hash_key(k);
        let value = hash_value(v);
        mpt.upsert(key, value);
        append_mermaid_to_file(&mpt, output_file, i + 1, k, v);
        println!(
            "Appended step {} diagram with {} nodes to {}",
            i + 1,
            mpt.store.len(),
            output_file
        );
    }
    println!("\nAll diagrams written to {}", output_file);
}
