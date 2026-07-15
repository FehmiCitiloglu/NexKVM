use crate::input_session::HandoffEdge;
use anyhow::Context as _;
use nexkvm_storage::Config;
use std::path::{Path, PathBuf};
use tokio::sync::watch;

const INPUT_CONFIG_RELOAD_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

pub(crate) fn spawn_input_handoff_reload(
    path: PathBuf,
    initial_edge: HandoffEdge,
) -> watch::Receiver<HandoffEdge> {
    let (sender, receiver) = watch::channel(initial_edge);
    tokio::spawn(watch_input_handoff_edge(path, sender));
    receiver
}

async fn watch_input_handoff_edge(path: PathBuf, sender: watch::Sender<HandoffEdge>) {
    let mut interval = tokio::time::interval(INPUT_CONFIG_RELOAD_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_error = None;

    loop {
        interval.tick().await;
        let reload_path = path.clone();
        let reload_sender = sender.clone();
        let reload = tokio::task::spawn_blocking(move || {
            reload_handoff_edge_once(&reload_path, &reload_sender)
        })
        .await
        .unwrap_or_else(|error| {
            Err(anyhow::anyhow!(
                "input topology reload task failed: {error}"
            ))
        });
        match reload {
            Ok(changed) => {
                last_error = None;
                if changed {
                    tracing::info!(
                        edge = ?*sender.borrow(),
                        "input handoff topology reloaded"
                    );
                }
            }
            Err(error) => {
                let message = error.to_string();
                if last_error.as_deref() != Some(message.as_str()) {
                    tracing::warn!(
                        %error,
                        path = %path.display(),
                        "input topology reload ignored; keeping last valid edge"
                    );
                    last_error = Some(message);
                }
            }
        }
    }
}

fn reload_handoff_edge_once(
    path: &Path,
    sender: &watch::Sender<HandoffEdge>,
) -> anyhow::Result<bool> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", path.display()));
        }
    };
    let config: Config =
        toml::from_str(&text).with_context(|| format!("parsing config from {}", path.display()))?;
    let next_edge = crate::input_handoff_edge(config.input.handoff_edge);
    if *sender.borrow() == next_edge {
        return Ok(false);
    }
    sender.send_replace(next_edge);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{reload_handoff_edge_once, spawn_input_handoff_reload};
    use crate::input_session::HandoffEdge;

    #[test]
    fn valid_reload_is_published_and_invalid_config_keeps_last_good_edge() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let (sender, receiver) = tokio::sync::watch::channel(HandoffEdge::Right);

        std::fs::write(&path, "[input]\nhandoff_edge = \"left\"\n").unwrap();
        assert!(reload_handoff_edge_once(&path, &sender).unwrap());
        assert_eq!(*receiver.borrow(), HandoffEdge::Left);

        std::fs::write(&path, "[input\nhandoff_edge = \"right\"\n").unwrap();
        assert!(reload_handoff_edge_once(&path, &sender).is_err());
        assert_eq!(*receiver.borrow(), HandoffEdge::Left);
    }

    #[tokio::test]
    async fn running_watcher_publishes_a_saved_edge_without_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[input]\nhandoff_edge = \"left\"\n").unwrap();

        let mut receiver = spawn_input_handoff_reload(path, HandoffEdge::Right);
        tokio::time::timeout(std::time::Duration::from_secs(1), receiver.changed())
            .await
            .expect("watcher did not publish the saved edge")
            .expect("watcher stopped unexpectedly");

        assert_eq!(*receiver.borrow_and_update(), HandoffEdge::Left);
    }
}
