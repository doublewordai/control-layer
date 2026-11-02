use crate::metrics::errors::component::CONFIG_WATCHER;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, anyhow};
use notify::{Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{Config, SharedConfig};

struct WatcherState {
    /// Each config file (canonical) with the directory watched for it, in merge order.
    files: Vec<(PathBuf, PathBuf)>,
    rx: mpsc::Receiver<()>,
    _watcher: RecommendedWatcher,
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    let is_absolute = path.is_absolute();
    let mut depth = 0usize;

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if depth > 0 {
                    normalized.pop();
                    depth -= 1;
                } else if !is_absolute {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Normal(part) => {
                normalized.push(part);
                depth += 1;
            }
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

        std::fs::canonicalize(&path).unwrap_or_else(|_| normalize_path(&path)) == config_path
    })
}

fn event_touches_any_config_file(event: &Event, files: &[(PathBuf, PathBuf)]) -> bool {
    files
        .iter()
        .any(|(config_path, watch_dir)| event_touches_config_file(event, watch_dir, config_path))
}

fn display_paths(files: &[(PathBuf, PathBuf)]) -> String {
    files
        .iter()
        .map(|(path, _)| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn initialize_watcher(config_paths: &[PathBuf]) -> anyhow::Result<WatcherState> {
    let current_dir = std::env::current_dir().context("failed to determine current directory for config watcher")?;
    let files = config_paths
        .iter()
        .map(|path| resolve_watch_paths(path, &current_dir))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let callback_files = files.clone();
    let (tx, rx) = mpsc::channel(1);

    let mut watcher = RecommendedWatcher::new(
        move |event: notify::Result<Event>| match event {
            Ok(event) if event.kind.is_modify() || event.kind.is_create() => {
                if event_touches_any_config_file(&event, &callback_files) {
                    let _ = tx.try_send(());
                }
            }
            Ok(_) => {}
            Err(error) => {
                crate::background_error!(CONFIG_WATCHER, "watch_error", Error, paths = %display_paths(&callback_files), error = %error, "Config watch error");
            }
        },
        NotifyConfig::default(),
    )?;

    // Several files may share a directory; watch each directory once.
    let mut watched: Vec<&PathBuf> = Vec::new();
    for (_, watch_dir) in &files {
        if !watched.contains(&watch_dir) {
            watcher.watch(watch_dir, RecursiveMode::NonRecursive)?;
            watched.push(watch_dir);
        }
    }

    Ok(WatcherState {
        files,
        rx,
        _watcher: watcher,
    })
}

/// Watch the config files and, when any of them changes, reload all of them (merged in order,
/// as at startup) into `shared_config`.
pub async fn watch_config_files(
    config_paths: Vec<PathBuf>,
    shared_config: SharedConfig,
    key_policy_cache: crate::sync::key_policy::KeyPolicyCache,
    shutdown_token: CancellationToken,
) -> anyhow::Result<()> {
    let Some(WatcherState { files, mut rx, _watcher }) = initialize_watcher(&config_paths).map(Some).unwrap_or_else(|error| {
        crate::background_error!(
            CONFIG_WATCHER, "setup_failed", Critical,
            paths = %config_paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join(", "),
            error = %error,
            "Config watcher setup failed; live config reload disabled"
        );
        None
    }) else {
        shutdown_token.cancelled().await;
        return Ok(());
    };

    let paths = display_paths(&files);
    let load_paths: Vec<PathBuf> = files.iter().map(|(path, _)| path.clone()).collect();
    info!(paths = %paths, "Watching config files for changes");

    loop {
        tokio::select! {
            _ = shutdown_token.cancelled() => {
                info!(paths = %paths, "Stopping config watcher");
                break;
            }
            maybe_reload = rx.recv() => {
                let Some(()) = maybe_reload else {
                    break;
                };

                info!(paths = %paths, "Config file changed, reloading");
                let load_paths = load_paths.clone();
                match tokio::task::spawn_blocking(move || {
                    Config::load_from_paths(&load_paths).map_err(anyhow::Error::from)
                })
                .await
                {
                    Ok(Ok(config)) => {
                        shared_config.store(config.clone());
                        // Re-resolve the toleration presets so a reload can add,
                        // change or drop them without a restart; a dropped id
                        // then fails closed on the next request instead of
                        // resolving to a stale value. The per-key map is left
                        // alone (its own trigger-driven refresh reloads it).
                        key_policy_cache.replace_presets(crate::scheduling::TolerationPresets::new(&config.scheduling));
                        info!(paths = %paths, "Reloaded config from disk");
                    }
                    Ok(Err(error)) => {
                        crate::background_error!(CONFIG_WATCHER, "reload_invalid", Warning, paths = %paths, error = %error, "Config reload failed; continuing with previous config");
                    }
                    Err(error) => {
                        crate::background_error!(CONFIG_WATCHER, "reload_join", Error, paths = %paths, error = %error, "Config reload task failed");
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{event_touches_any_config_file, event_touches_config_file, resolve_watch_paths, watch_config_files};
    use notify::{Event, EventKind, event::CreateKind};
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    use crate::{Config, SharedConfig};

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

    #[test]
    fn an_event_on_any_of_several_config_files_triggers_a_reload() {
        let tempdir = tempdir().expect("failed to create temp dir");
        let base_dir = tempdir.path().join("app");
        let secrets_dir = tempdir.path().join("secrets");
        std::fs::create_dir_all(&base_dir).expect("failed to create base dir");
        std::fs::create_dir_all(&secrets_dir).expect("failed to create secrets dir");
        std::fs::write(base_dir.join("config.yaml"), "port = 8080\n").expect("failed to write base config");
        std::fs::write(secrets_dir.join("dwctl.yaml"), "secret_key: x\n").expect("failed to write secrets config");

        let files: Vec<(PathBuf, PathBuf)> = [base_dir.join("config.yaml"), secrets_dir.join("dwctl.yaml")]
            .iter()
            .map(|path| resolve_watch_paths(path, tempdir.path()).expect("failed to resolve watch paths"))
            .collect();

        let event_on = |path: PathBuf| Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![path],
            attrs: Default::default(),
        };

        assert!(event_touches_any_config_file(&event_on(secrets_dir.join("dwctl.yaml")), &files));
        assert!(event_touches_any_config_file(&event_on(base_dir.join("config.yaml")), &files));
        assert!(!event_touches_any_config_file(&event_on(secrets_dir.join("other.yaml")), &files));
    }

    #[test]
    fn normalize_path_does_not_pop_past_root() {
        let normalized = super::normalize_path(Path::new("/../configs/./dwctl.toml"));
        assert_eq!(normalized, PathBuf::from("/configs/dwctl.toml"));
    }

    #[cfg(unix)]
    #[test]
    fn event_touches_config_file_resolves_symlinked_paths() {
        let tempdir = tempdir().expect("failed to create temp dir");
        let real_dir = tempdir.path().join("real");
        std::fs::create_dir_all(&real_dir).expect("failed to create real dir");
        let config_path = real_dir.join("dwctl.toml");
        std::fs::write(&config_path, "port = 8080\n").expect("failed to write config file");

        let symlink_dir = tempdir.path().join("symlinked");
        std::os::unix::fs::symlink(&real_dir, &symlink_dir).expect("failed to create symlink");

        let canonical_config = std::fs::canonicalize(&config_path).expect("failed to canonicalize config path");
        let event = Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![symlink_dir.join("dwctl.toml")],
            attrs: Default::default(),
        };

        assert!(event_touches_config_file(&event, tempdir.path(), &canonical_config));
    }

    #[tokio::test]
    async fn watch_config_file_ignores_setup_errors_and_returns_on_shutdown() {
        let shutdown = CancellationToken::new();
        shutdown.cancel();

        let result = watch_config_files(
            vec![PathBuf::from("/definitely/missing/dwctl.toml")],
            SharedConfig::new(Config::default()),
            crate::sync::key_policy::KeyPolicyCache::empty(),
            shutdown,
        )
        .await;

        assert!(result.is_ok());
    }
}
