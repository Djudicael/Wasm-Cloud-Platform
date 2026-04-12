use secrets::crypto::SymmetricKey;

pub struct NodeConfig {
    pub key_source: String,
    pub key_file: Option<String>,
}

pub fn load_master_key(config: &NodeConfig) -> SymmetricKey {
    match config.key_source.as_str() {
        "env" => {
            // Dev mode: read from environment variable
            let hex = std::env::var("NODE_MASTER_KEY")
                .expect("NODE_MASTER_KEY must be set in env key mode");
            let bytes = hex::decode(hex).expect("invalid hex key");
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes[..32]);
            SymmetricKey::from_bytes(arr)
        }
        "file" => {
            // Read from a key file with restricted permissions (chmod 600)
            let path = config.key_file.as_deref().expect("key_file required");
            let content = std::fs::read(path).expect("cannot read key file");
            if content.len() < 32 {
                panic!("Key file must be at least 32 bytes long");
            }
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&content[..32]);
            SymmetricKey::from_bytes(arr)
        }
        "generate" => {
            // First-run: generate and persist the key to disk
            let key = SymmetricKey::generate();
            let path = config
                .key_file
                .as_deref()
                .unwrap_or("/etc/wasm-node/master.key");
            std::fs::write(path, key.as_bytes()).expect("cannot write key file");
            tracing::warn!("Generated new master key and saved to {path}. Back it up!");
            key
        }
        _ => panic!("Unknown key source: {}", config.key_source),
    }
}

fn main() {
    println!("Node started");
}
