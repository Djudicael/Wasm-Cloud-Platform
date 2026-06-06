//! Auth config reload support for the node admin surface.

use std::sync::Arc;

/// Reload admin auth config from disk on SIGHUP.
///
/// This keeps token rotation and auth policy changes out of the main startup
/// path while preserving the current signal-driven behavior.
pub(crate) fn setup_sighup_handler(
    auth_config: Arc<tokio::sync::RwLock<common::auth::AuthConfig>>,
    config_path: Option<String>,
) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        tokio::spawn(async move {
            let mut stream = match signal(SignalKind::hangup()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "failed to install SIGHUP handler");
                    return;
                }
            };
            loop {
                stream.recv().await;

                if let Some(ref path) = config_path {
                    tracing::info!("SIGHUP received - reloading auth config from file");

                    match std::fs::read_to_string(path) {
                        Ok(content) => {
                            match toml::from_str::<common::config::NodeConfig>(&content) {
                                Ok(new_config) => {
                                    let new_auth: common::auth::AuthConfig =
                                        new_config.auth.clone().into();
                                    if let Err(e) = new_auth.validate() {
                                        tracing::error!(
                                            error = %e,
                                            "auth config in file is invalid - keeping current config"
                                        );
                                    } else {
                                        let mut auth = auth_config.write().await;
                                        *auth = new_auth;
                                        tracing::info!("auth config reloaded from file");
                                    }
                                }
                                Err(e) => {
                                    tracing::error!(
                                        error = %e,
                                        "failed to parse config file on SIGHUP reload"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                error = %e,
                                path = %path,
                                "failed to read config file on SIGHUP reload"
                            );
                        }
                    }
                } else {
                    tracing::warn!("SIGHUP received but no config file path - cannot reload auth");
                }
            }
        });
    }
    #[cfg(not(unix))]
    {
        let _ = (auth_config, config_path);
    }
}
