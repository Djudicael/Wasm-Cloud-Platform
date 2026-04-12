// crates/ctl/src/cmds/secrets.rs
use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use common::types::AppId;
use messaging::{events::Event, NatsBus};

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
            // NOTE: In production, encrypt `plaintext` with a cluster public key
            // before putting it in the NATS message. For now, send plaintext
            // (fine for development, not for production — see step 13 security).
            let (name, version) = app
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("app must be <name>:<version>"))?;
            let event = Event::SecretUpdate {
                app_id: AppId::new(name, version),
                key: key.clone(),
                encrypted_value: plaintext.into_bytes(), // TODO: encrypt with cluster key
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
            println!(
                "{} Secret delete for {}/{} — not yet implemented (add Event::SecretDelete)",
                "⚠".yellow(),
                app,
                key
            );
        }
    }
    Ok(())
}
