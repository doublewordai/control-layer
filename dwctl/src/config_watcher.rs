use std::path::PathBuf;

use notify::{Config as NotifyConfig, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::{Config, SharedConfig};

pub async fn watch_config_file(
    config_path: PathBuf,
    shared_config: SharedConfig,
    shutdown_token: CancellationToken,
) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::channel(32);

    let mut watcher = RecommendedWatcher::new(
        move |event| {
            let _ = tx.blocking_send(event);
        },
        NotifyConfig::default(),
    )?;

    watcher.watch(&config_path, RecursiveMode::NonRecursive)?;
    info!(path = %config_path.display(), "Watching config file for changes");

    loop {
        tokio::select! {
            _ = shutdown_token.cancelled() => {
                info!(path = %config_path.display(), "Stopping config watcher");
                break;
            }
            maybe_event = rx.recv() => {
                let Some(event) = maybe_event else {
                    break;
                };

                match event {
                    Ok(event) if event.kind.is_modify() || event.kind.is_create() => {
                        info!(path = %config_path.display(), kind = ?event.kind, "Config file changed, reloading");
                        match Config::load_from_path(config_path.to_string_lossy().into_owned()) {
                            Ok(config) => {
                                shared_config.store(config);
                                info!(path = %config_path.display(), "Reloaded config from disk");
                            }
                            Err(error) => {
                                warn!(path = %config_path.display(), error = %error, "Config reload failed; continuing with previous config");
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        error!(path = %config_path.display(), error = %error, "Config watch error");
                    }
                }
            }
        }
    }

    Ok(())
}
