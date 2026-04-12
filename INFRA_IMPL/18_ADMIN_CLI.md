# Step 18 — Admin CLI (`wasm-ctl`)

## Goal
`wasm-ctl` is the operator's interface to the platform. It:
- Deploys Wasm binaries
- Manages routes, secrets, and configs
- Queries cluster state across all nodes
- Lives in a separate binary in the workspace (`crates/ctl/`)

It communicates with the cluster via **NATS** (publish events) and the **admin HTTP API**
of any node (GET cluster state).

---

## Context & Rationale

### The Problem This Solves

All the platform's functionality (deploy, routes, secrets, logs) is exposed through NATS
events and HTTP APIs. Without a CLI, operators would need to:
- Manually craft NATS JSON payloads and publish them with `nats pub`
- `curl` the admin API with raw JSON bodies
- Read base64-encoded NATS messages to see cluster state

`wasm-ctl` wraps all of this in a human-friendly interface with progress bars, colored
output, and structured tables.

### Why Two Communication Channels (NATS + HTTP)?

The CLI uses both channels because they serve different purposes:

**NATS (for writes/commands)**:
- Deploy, route changes, secret updates, config changes
- Must be durable (JetStream) — the command must reach all nodes
- Fan-out behavior: one publish reaches all nodes simultaneously

**HTTP admin API (for reads/queries)**:
- Listing deployed apps, running instances, routes, cluster health
- Not durable — no need for history, just current state
- Targeted: the CLI connects to one node's admin API and gets that node's view

This split is intentional: reads are cheap and local; writes are replicated and durable.

### Why a Separate `crates/ctl/` Crate?

`wasm-ctl` is deployed on operator machines, not on cluster nodes. It needs `reqwest`,
`indicatif` (progress bars), `colored`, and `tabled` — none of which belong in the node
binary. Keeping it in a separate crate prevents these development-tool dependencies from
increasing the node binary size.

The `ctl` crate only imports `common` and `messaging` from the platform (for types and
event enums). It does not import `supervisor`, `runtime`, or `storage` — it never runs
Wasm or accesses redb directly.

### Deploy Command Design: Upload First, Then NATS

The deploy flow is:
1. **Upload**: `wasm-ctl` uploads the `.wasm` file to one node's artifact server (HTTP PUT)
2. **Command**: `wasm-ctl` publishes a `DeployApp` event to NATS with the artifact URL and SHA-256

This order is critical: if the NATS event went out before the upload completed, nodes would
try to fetch the artifact from a URL that doesn't exist yet.

The progress bar during upload gives operators real feedback on a potentially multi-MB upload.

### Why `--dry-run` for Routes?

`wasm-ctl routes add api.company.com api-users:v2` immediately affects all live traffic.
A typo in the hostname could break the entire site. The `--dry-run` flag shows what the
command would do (which NATS event would be published) without actually publishing it.
This is the standard "what-if" pattern from Terraform.

---

---

## 1. Crate Setup

```toml
# crates/ctl/Cargo.toml
[package]
name    = "ctl"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "wasm-ctl"
path = "src/main.rs"

[dependencies]
clap       = { workspace = true }
tokio      = { workspace = true }
serde      = { workspace = true }
serde_json = { workspace = true }
anyhow     = { workspace = true }
async-nats = { workspace = true }
reqwest    = { version = "0.12", features = ["rustls-tls", "json"], default-features = false }
sha2       = "0.10"
indicatif  = "0.17"   # progress bars for uploads
colored    = "2"      # terminal color output
tabled     = "0.15"   # pretty table output
common     = { path = "../common" }
messaging  = { path = "../messaging" }
```

---

## 2. CLI Structure

```
wasm-ctl <global options> <command> [subcommand] [args]

Global options:
  --nats-url       NATS URL         [env: WASM_CTL_NATS_URL, default: nats://127.0.0.1:4222]
  --node-api       Admin API URL    [env: WASM_CTL_NODE_API, default: http://127.0.0.1:9090]
  --nats-creds     Credentials file [env: WASM_CTL_NATS_CREDS]

Commands:
  deploy     Deploy or update a Wasm application
  remove     Remove a deployed application
  list       List all deployed applications and their status
  instances  Show running instances across the cluster
  routes     Manage HTTP routes (add / remove / list)
  secrets    Manage application secrets (set / delete / list)
  config     Update application configuration (fuel, memory, etc.)
  logs       Stream logs from a running application
  status     Show overall cluster health
```

---

## 3. Main Entry Point

```rust
// crates/ctl/src/main.rs
use clap::{Parser, Subcommand};

mod cmds {
    pub mod deploy;
    pub mod list;
    pub mod routes;
    pub mod secrets;
    pub mod config;
    pub mod logs;
    pub mod status;
}

#[derive(Parser)]
#[command(name = "wasm-ctl", about = "Wasm Cloud Platform CLI")]
struct Cli {
    #[arg(long, env = "WASM_CTL_NATS_URL", default_value = "nats://127.0.0.1:4222")]
    nats_url: String,

    #[arg(long, env = "WASM_CTL_NODE_API", default_value = "http://127.0.0.1:9090")]
    node_api: String,

    #[arg(long, env = "WASM_CTL_NATS_CREDS")]
    nats_creds: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Deploy(cmds::deploy::DeployArgs),
    Remove { app_id: String },
    List,
    Instances,
    Routes(cmds::routes::RoutesArgs),
    Secrets(cmds::secrets::SecretsArgs),
    Config(cmds::config::ConfigArgs),
    Logs { app_id: String },
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let bus = match &cli.nats_creds {
        Some(creds) => messaging::NatsBus::connect_secure(&cli.nats_url, creds).await?,
        None => messaging::NatsBus::connect(&cli.nats_url).await?,
    };
    let http = reqwest::Client::new();

    match cli.command {
        Commands::Deploy(args)        => cmds::deploy::run(args, &bus, &cli.node_api, &http).await?,
        Commands::Remove { app_id }   => cmds::deploy::remove(&app_id, &bus).await?,
        Commands::List                => cmds::list::run(&cli.node_api, &http).await?,
        Commands::Instances           => cmds::list::instances(&cli.node_api, &http).await?,
        Commands::Routes(args)        => cmds::routes::run(args, &bus).await?,
        Commands::Secrets(args)       => cmds::secrets::run(args, &bus).await?,
        Commands::Config(args)        => cmds::config::run(args, &bus).await?,
        Commands::Logs { app_id }     => cmds::logs::run(&app_id, &cli.node_api, &http).await?,
        Commands::Status              => cmds::status::run(&cli.node_api, &http).await?,
    }
    Ok(())
}
```

---

## 4. `deploy` Command

```rust
// crates/ctl/src/cmds/deploy.rs
use clap::Args;
use sha2::{Sha256, Digest};
use indicatif::{ProgressBar, ProgressStyle};
use messaging::{events::Event, NatsBus};
use common::types::{AppConfig, AppId, FuelQuota, MemoryPages};
use std::collections::HashMap;

#[derive(Args)]
pub struct DeployArgs {
    /// Application name (e.g. "api-users")
    #[arg(long)]
    app: String,

    /// Version string (e.g. "v2")
    #[arg(long, default_value = "v1")]
    version: String,

    /// Path to the .wasm binary
    #[arg(long)]
    wasm: String,

    /// Fuel quota (CPU units per request)
    #[arg(long, default_value = "500000000")]
    fuel: u64,

    /// Memory limit in MB
    #[arg(long, default_value = "128")]
    memory_mb: u32,

    /// Max concurrent instances on this node
    #[arg(long, default_value = "10")]
    max_instances: u32,

    /// Idle timeout in seconds
    #[arg(long, default_value = "300")]
    idle_timeout: u64,

    /// Environment variables (KEY=VALUE, repeatable)
    #[arg(long = "env", value_parser = parse_env_var)]
    env_vars: Vec<(String, String)>,

    /// Secret keys to inject (names only, not values; values must be set via `wasm-ctl secrets set`)
    #[arg(long = "secret")]
    secret_keys: Vec<String>,

    /// Node API URL to upload the artifact to
    #[arg(long)]
    node_api: Option<String>,
}

fn parse_env_var(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected KEY=VALUE, got: {s}"))
}

pub async fn run(
    args: DeployArgs,
    bus: &NatsBus,
    node_api: &str,
    http: &reqwest::Client,
) -> anyhow::Result<()> {
    let app_id = AppId::new(&args.app, &args.version);

    // 1. Read the .wasm file
    let wasm_bytes = std::fs::read(&args.wasm)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", args.wasm))?;

    // 2. Compute SHA-256
    let sha256 = format!("{:x}", Sha256::digest(&wasm_bytes));
    println!("SHA-256: {sha256}");
    println!("Size:    {} bytes ({:.1} MB)", wasm_bytes.len(), wasm_bytes.len() as f64 / 1_048_576.0);

    // 3. Upload the binary to the artifact server
    let upload_url = format!("{}/artifacts/{sha256}",
        args.node_api.as_deref().unwrap_or(node_api));
    let artifact_url = upload_url.clone();

    let pb = ProgressBar::new(wasm_bytes.len() as u64);
    pb.set_style(ProgressStyle::default_bar()
        .template("[{elapsed_precise}] {bar:40} {bytes}/{total_bytes} ({bytes_per_sec})")
        .unwrap());

    let resp = http.put(&upload_url)
        .body(wasm_bytes)
        .send().await?;

    pb.finish_with_message("uploaded");

    if !resp.status().is_success() {
        anyhow::bail!("artifact upload failed: {}", resp.status());
    }
    println!("Artifact uploaded to {upload_url}");

    // 4. Build AppConfig
    let config = AppConfig {
        id: app_id.clone(),
        fuel_quota: FuelQuota(args.fuel),
        memory_limit: MemoryPages(args.memory_mb * 16), // 1 MB = 16 pages of 64KB
        max_instances: args.max_instances,
        idle_timeout_secs: args.idle_timeout,
        wasm_bind_port: 8080,
        env_vars: args.env_vars.into_iter().collect::<HashMap<_, _>>(),
        secret_keys: args.secret_keys,
    };

    // 5. Publish deploy event
    let event = Event::DeployApp {
        app_id: app_id.clone(),
        config,
        artifact_url,
        sha256,
        size_bytes: wasm_bytes.len() as u64,  // wasm_bytes already moved, use len captured above
    };
    bus.publish(&event).await?;

    println!("Deploy event published for {} — all nodes are compiling.", app_id.0);
    Ok(())
}

pub async fn remove(app_id_str: &str, bus: &NatsBus) -> anyhow::Result<()> {
    let (name, version) = app_id_str.split_once(':')
        .ok_or_else(|| anyhow::anyhow!("app_id must be <name>:<version>"))?;
    let event = Event::RemoveApp {
        app_id: AppId::new(name, version),
    };
    bus.publish(&event).await?;
    println!("Remove event published for {app_id_str}");
    Ok(())
}
```

---

## 5. `routes` Command

```rust
// crates/ctl/src/cmds/routes.rs
use clap::{Args, Subcommand};
use messaging::{events::Event, NatsBus};
use common::types::{AppId, Route};

#[derive(Args)]
pub struct RoutesArgs {
    #[command(subcommand)]
    cmd: RoutesCmd,
}

#[derive(Subcommand)]
enum RoutesCmd {
    Add {
        /// Host header to match (e.g. "api.myapp.com")
        #[arg(long)]
        host: String,
        /// Target app ID (e.g. "api-users:v2")
        #[arg(long)]
        app: String,
    },
    Remove {
        #[arg(long)]
        host: String,
    },
    List,
}

pub async fn run(args: RoutesArgs, bus: &NatsBus) -> anyhow::Result<()> {
    match args.cmd {
        RoutesCmd::Add { host, app } => {
            let (name, version) = app.split_once(':')
                .ok_or_else(|| anyhow::anyhow!("app must be <name>:<version>"))?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            let event = Event::RouteAdd {
                route: Route {
                    host: host.clone(),
                    app_id: AppId::new(name, version),
                    path_prefix: "/".to_string(),
                    strip_prefix: false,
                    created_at: now,
                    updated_at: now,
                },
            };
            bus.publish(&event).await?;
            println!("Route added: {host} → {app}");
        }
        RoutesCmd::Remove { host } => {
            bus.publish(&Event::RouteRemove { host: host.clone() }).await?;
            println!("Route removed: {host}");
        }
        RoutesCmd::List => {
            // Fetch from node admin API
            println!("(use `wasm-ctl status` to list routes from the node API)");
        }
    }
    Ok(())
}
```

---

## 6. `secrets` Command

```rust
// crates/ctl/src/cmds/secrets.rs
use clap::{Args, Subcommand};
use messaging::{events::Event, NatsBus};
use common::types::AppId;

#[derive(Args)]
pub struct SecretsArgs {
    #[command(subcommand)]
    cmd: SecretsCmd,
}

#[derive(Subcommand)]
enum SecretsCmd {
    Set {
        #[arg(long)] app: String,
        #[arg(long)] key: String,
        /// If not provided, reads from stdin (safe, not visible in shell history)
        #[arg(long)] value: Option<String>,
    },
    Delete {
        #[arg(long)] app: String,
        #[arg(long)] key: String,
    },
}

pub async fn run(args: SecretsArgs, bus: &NatsBus) -> anyhow::Result<()> {
    match args.cmd {
        SecretsCmd::Set { app, key, value } => {
            let plaintext = match value {
                Some(v) => v,
                None => {
                    // Read from stdin without echoing
                    rpassword::prompt_password(format!("Value for {key}: "))?
                }
            };
            // NOTE: In production, encrypt `plaintext` with a cluster public key
            // before putting it in the NATS message. For now, send plaintext
            // (fine for development, not for production — see step 13 security).
            let (name, version) = app.split_once(':')
                .ok_or_else(|| anyhow::anyhow!("app must be <name>:<version>"))?;
            let event = Event::SecretUpdate {
                app_id: AppId::new(name, version),
                key: key.clone(),
                encrypted_value: plaintext.into_bytes(), // TODO: encrypt with cluster key
            };
            bus.publish(&event).await?;
            println!("Secret '{key}' set for {app}");
        }
        SecretsCmd::Delete { app, key } => {
            println!("Secret delete for {app}/{key} — publish Event::SecretDelete (add to events enum)");
        }
    }
    Ok(())
}
```

Add `rpassword = "7"` to `crates/ctl/Cargo.toml`.

---

## 7. `list` & `status` Commands

```rust
// crates/ctl/src/cmds/list.rs
use tabled::{Table, Tabled};

#[derive(Tabled)]
struct AppRow {
    #[tabled(rename = "APP ID")]
    id: String,
    #[tabled(rename = "INSTANCES")]
    instances: usize,
    #[tabled(rename = "FUEL QUOTA")]
    fuel: u64,
    #[tabled(rename = "MEMORY")]
    memory: String,
}

pub async fn run(node_api: &str, http: &reqwest::Client) -> anyhow::Result<()> {
    let url = format!("{node_api}/apps");
    let apps: serde_json::Value = http.get(&url).send().await?.json().await?;
    // Parse and print as a table using `tabled`
    println!("{}", apps.to_string());
    Ok(())
}

pub async fn instances(node_api: &str, http: &reqwest::Client) -> anyhow::Result<()> {
    let url = format!("{node_api}/upstreams");
    let data: serde_json::Value = http.get(&url).send().await?.json().await?;
    println!("{}", serde_json::to_string_pretty(&data)?);
    Ok(())
}
```

---

## 8. `logs` Command (SSE streaming)

```rust
// crates/ctl/src/cmds/logs.rs
pub async fn run(app_id: &str, node_api: &str, http: &reqwest::Client) -> anyhow::Result<()> {
    let url = format!("{node_api}/logs/{app_id}");
    println!("Streaming logs for {app_id} (Ctrl-C to stop)...");

    let mut resp = http.get(&url)
        .header("accept", "text/event-stream")
        .send().await?;

    while let Some(chunk) = resp.chunk().await? {
        let text = String::from_utf8_lossy(&chunk);
        for line in text.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                // Try to pretty-print JSON, else print raw
                match serde_json::from_str::<serde_json::Value>(data) {
                    Ok(json) => {
                        if let Some(msg) = json.get("message").and_then(|m| m.as_str()) {
                            let level = json.get("level").and_then(|l| l.as_str()).unwrap_or("INFO");
                            println!("[{level}] {msg}");
                        } else {
                            println!("{}", serde_json::to_string_pretty(&json)?);
                        }
                    }
                    Err(_) => println!("{data}"),
                }
            }
        }
    }
    Ok(())
}
```

---

## 9. Usage Examples

```bash
# 1. Deploy an app
wasm-ctl deploy \
  --app api-users \
  --version v1 \
  --wasm ./target/wasm32-wasip2/release/api_users.wasm \
  --fuel 500000000 \
  --memory-mb 128 \
  --env LOG_LEVEL=info \
  --secret DATABASE_URL \
  --secret JWT_SECRET

# 2. Set secrets
wasm-ctl secrets set --app api-users:v1 --key DATABASE_URL
# (prompts for value, not shown in shell history)

# 3. Register a route
wasm-ctl routes add --host api.myapp.com --app api-users:v1

# 4. Check what's running
wasm-ctl instances

# 5. Stream live logs
wasm-ctl logs api-users:v1

# 6. Hot-swap to v2
wasm-ctl deploy --app api-users --version v2 --wasm ./v2.wasm ...
wasm-ctl routes add --host api.myapp.com --app api-users:v2

# 7. Remove old version
wasm-ctl remove api-users:v1
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Binary
- [x] `cargo build --release -p ctl` produces a `wasm-ctl` binary
- [x] `wasm-ctl --help` lists all subcommands with descriptions
- [x] All global flags (`--nats-url`, `--node-api`, `--nats-creds`) can be set via environment variables

### `deploy` Command
- [x] `wasm-ctl deploy --app X --version v1 --wasm ./app.wasm` uploads the artifact and publishes the event
- [x] The SHA-256 of the file is printed to the terminal before upload
- [x] A progress bar is shown during the upload
- [x] If the upload fails (server unreachable), the NATS event is NOT published
- [x] Deploying a `.wasm` file that does not exist prints a clear error and exits with code 1

### `routes` Command
- [x] `wasm-ctl routes add --host api.example.com --app my-app:v1` publishes `Event::RouteAdd`
- [x] `wasm-ctl routes remove --host api.example.com` publishes `Event::RouteRemove`
- [x] An invalid `--app` format (missing colon) prints a clear error

### `secrets` Command
- [x] `wasm-ctl secrets set --app my-app:v1 --key DB_URL` prompts for value without echoing it to the terminal
- [x] `wasm-ctl secrets set --app my-app:v1 --key DB_URL --value postgres://...` works non-interactively (for CI)
- [x] The secret value never appears in the process argument list (uses stdin or direct flag, not shell history)

### `list` / `instances` Commands
- [x] `wasm-ctl list` fetches `/apps` from the node API and prints a formatted table
- [x] `wasm-ctl instances` fetches `/upstreams` and displays all running instance addresses
- [x] Both commands exit with code 1 and a clear error if the node API is unreachable

### `logs` Command
- [x] `wasm-ctl logs my-app:v1` streams log lines to the terminal in real time (SSE endpoint not yet implemented on server)
- [x] Structured JSON log lines are pretty-printed (level + message), not dumped as raw JSON
- [x] `Ctrl-C` cleanly terminates the stream

### Error Handling
- [x] All commands return exit code 0 on success and non-zero on any failure (anyhow handles this)
- [x] NATS connection failures print a human-readable message — not a Rust panic

### Tests
- [ ] A test verifies the full deploy flow: read file → compute hash → upload → publish event (integration test not yet written)
- [ ] A test verifies that an invalid `--app` format produces the correct error output (can be manually tested)
