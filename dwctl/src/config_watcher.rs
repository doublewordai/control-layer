use std::path::{Component, Path, PathBuf};

use anyhow::{Context, anyhow};
use notify::{Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::{Config, SharedConfig};

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }

    normalized
}

fn resolve_watch_paths(config_path: &Path, current_dir: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let config_path = if config_path.is_absolute() {
        config_path.to_path_buf()
    } else {
        current_dir.join(config_path)
    };

    let config_path =
        std::fs::canonicalize(&config_path).with_context(|| format!("failed to canonicalize config path {}", config_path.display()))?;
    let watch_dir = config_path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("config path {} has no parent directory", config_path.display()))?;

    Ok((config_path, watch_dir))
}

fn event_touches_config_file(event: &Event, watch_dir: &Path, config_path: &Path) -> bool {
    event.paths.iter().any(|path| {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            watch_dir.join(path)
        };

        normalize_path(&path) == config_path
    })
}

pub async fn watch_config_file(config_path: PathBuf, shared_config: SharedConfig, shutdown_token: CancellationToken) -> anyhow::Result<()> {
    let current_dir = std::env::current_dir().context("failed to determine current directory for config watcher")?;
    let (config_path, watch_dir) = resolve_watch_paths(&config_path, &current_dir)?;
    let callback_config_path = config_path.clone();
    let callback_watch_dir = watch_dir.clone();
    let (tx, mut rx) = mpsc::channel(1);

    let mut watcher = RecommendedWatcher::new(
        move |event: notify::Result<Event>| match event {
            Ok(event) if event.kind.is_modify() || event.kind.is_create() => {
                if event_touches_config_file(&event, &callback_watch_dir, &callback_config_path) {
                    let _ = tx.try_send(());
                }
            }
            Ok(_) => {}
            Err(error) => {
                error!(path = %callback_config_path.display(), error = %error, "Config watch error");
            }
        },
        NotifyConfig::default(),
    )?;

    watcher.watch(&watch_dir, RecursiveMode::NonRecursive)?;
    info!(path = %config_path.display(), watch_dir = %watch_dir.display(), "Watching config file for changes");

    loop {
        tokio::select! {
            _ = shutdown_token.cancelled() => {
                info!(path = %config_path.display(), "Stopping config watcher");
                break;
            }
            maybe_reload = rx.recv() => {
                let Some(()) = maybe_reload else {
                    break;
                };

                info!(path = %config_path.display(), "Config file changed, reloading");
                let load_path = config_path.clone();
                match tokio::task::spawn_blocking(move || {
                    Config::load_from_path(load_path.to_string_lossy().into_owned()).map_err(anyhow::Error::from)
                })
                .await
                {
                    Ok(Ok(config)) => {
                        shared_config.store(config);
                        info!(path = %config_path.display(), "Reloaded config from disk");
                    }
                    Ok(Err(error)) => {
                        warn!(path = %config_path.display(), error = %error, "Config reload failed; continuing with previous config");
                    }
                    Err(error) => {
                        error!(path = %config_path.display(), error = %error, "Config reload task failed");
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{event_touches_config_file, resolve_watch_paths};
    use notify::{Event, EventKind, event::CreateKind};
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    #[test]
    fn resolve_watch_paths_canonicalizes_relative_path_and_parent_directory() {
        let tempdir = tempdir().expect("failed to create temp dir");
        let config_dir = tempdir.path().join("configs");
        std::fs::create_dir_all(&config_dir).expect("failed to create config dir");
        let config_path = config_dir.join("dwctl.toml");
        std::fs::write(&config_path, "port = 8080\n").expect("failed to write config file");

        let (resolved_config_path, watch_dir) =
            resolve_watch_paths(Path::new("configs/./dwctl.toml"), tempdir.path()).expect("failed to resolve watch paths");

        assert_eq!(
            resolved_config_path,
            std::fs::canonicalize(&config_path).expect("failed to canonicalize config path")
        );
        assert_eq!(
            watch_dir,
            std::fs::canonicalize(&config_dir).expect("failed to canonicalize watch dir")
        );
    }

    #[test]
    fn event_touches_config_file_handles_relative_event_paths() {
        let tempdir = tempdir().expect("failed to create temp dir");
        let config_dir = tempdir.path().join("configs");
        std::fs::create_dir_all(&config_dir).expect("failed to create config dir");
        let config_path = config_dir.join("dwctl.toml");
        std::fs::write(&config_path, "port = 8080\n").expect("failed to write config file");
        let config_path = std::fs::canonicalize(&config_path).expect("failed to canonicalize config path");

        let event = Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![PathBuf::from("dwctl.toml")],
            attrs: Default::default(),
        };

        assert!(event_touches_config_file(&event, &config_dir, &config_path));
    }

    #[test]
    fn event_touches_config_file_ignores_other_files() {
        let tempdir = tempdir().expect("failed to create temp dir");
        let config_dir = tempdir.path().join("configs");
        std::fs::create_dir_all(&config_dir).expect("failed to create config dir");
        let config_path = config_dir.join("dwctl.toml");
        std::fs::write(&config_path, "port = 8080\n").expect("failed to write config file");
        let config_path = std::fs::canonicalize(&config_path).expect("failed to canonicalize config path");

        let event = Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![PathBuf::from("dwctl.toml.tmp")],
            attrs: Default::default(),
        };

        assert!(!event_touches_config_file(&event, &config_dir, &config_path));
    }
}
