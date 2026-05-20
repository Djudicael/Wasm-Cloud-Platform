// crates/ctl/src/cmds/secrets.rs
use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use common::types::AppId;
use messaging::{events::Event, NatsBus};
use secrets::SecretTransportEnvelope;

#[derive(Args)]
pub struct SecretsArgs {
    #[command(subcommand)]
    cmd: SecretsCmd,
}

#[derive(Subcommand)]
enum SecretsCmd {
    /// Set a secret value for an application
    Set {
        #[arg(long)]
        app: String,
        #[arg(long)]
        key: String,
        /// If not provided, reads from stdin (safe, not visible in shell history)
        #[arg(long)]
        value: Option<String>,
    },
    /// Delete a secret (not yet implemented)
    Delete {
        #[arg(long)]
        app: String,
        #[arg(long)]
        key: String,
    },
}

pub async fn run(args: SecretsArgs, bus: &NatsBus) -> Result<()> {
    match args.cmd {
        SecretsCmd::Set { app, key, value } => {
            let plaintext = match value {
                Some(v) => v,
                None => {
                    // Read from stdin without echoing
                    rpassword::prompt_password(format!("Value for {}: ", key.cyan()))?
                }
            };
            // NOTE: This remains plaintext-over-NATS for development compatibility,
            // but it now uses a canonical, versioned transport envelope so ctl ->
            // NATS -> node all agree on one explicit secret update format.
            let (name, version) = app
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("app must be <name>:<version>"))?;
            let event = Event::SecretUpdate {
                app_id: AppId::new(name, version),
                key: key.clone(),
                secret: SecretTransportEnvelope::plaintext_utf8(plaintext),
            };
            bus.publish(&event).await?;
            println!(
                "{} Secret '{}' set for {}",
                "✓".green(),
                key.cyan(),
                app.yellow()
            );
        }
        SecretsCmd::Delete { app, key } => {
            anyhow::bail!(
                "Secret delete for {}/{} — not yet implemented (add Event::SecretDelete)",
                app,
                key
            );
        }
    }
    Ok(())
}
