use std::io;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::select;
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};
use tracing_subscriber::EnvFilter;
use zfsio_collector::{CollectorConfig, LinuxOpenZfsCollector, MockCollector, SnapshotSource};
use zfsio_tui::{AppState, TerminalGuard, draw_once, poll_exit_key};

#[derive(Debug, Parser)]
#[command(name = "zfs-io")]
#[command(about = "Low-interference OpenZFS terminal observability")]
struct Args {
    /// Use generated telemetry instead of reading OpenZFS state.
    #[arg(long)]
    mock: bool,

    /// Refresh interval in milliseconds for lightweight samples.
    #[arg(long, default_value_t = 1000)]
    refresh_ms: u64,

    /// Slow topology refresh interval in seconds.
    #[arg(long, default_value_t = 30)]
    topology_refresh_secs: u64,

    /// Hard timeout in milliseconds for external helper commands.
    #[arg(long, default_value_t = 2000)]
    command_timeout_ms: u64,

    /// Optional file for debug logs.
    #[arg(long)]
    log_file: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_logging(args.log_file.as_deref())?;

    let config = CollectorConfig {
        refresh_interval: Duration::from_millis(args.refresh_ms.max(250)),
        topology_refresh_interval: Duration::from_secs(args.topology_refresh_secs.max(15)),
        command_timeout: Duration::from_millis(args.command_timeout_ms.clamp(250, 10_000)),
        ..CollectorConfig::default()
    };

    let (tx, mut rx) = mpsc::channel(2);
    if args.mock {
        spawn_collector(MockCollector::new(config), tx);
    } else {
        spawn_collector(LinuxOpenZfsCollector::new(config), tx);
    }

    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut state = AppState::default();
    let mut render_tick = interval(Duration::from_millis(100));
    render_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        select! {
            maybe_snapshot = rx.recv() => {
                if let Some(snapshot) = maybe_snapshot
                    && !state.paused
                {
                    state.last_snapshot = Some(snapshot);
                }
            }
            _ = render_tick.tick() => {
                terminal.draw(|frame| draw_once(frame, &state))?;
                if poll_exit_key(&mut state, Duration::from_millis(0))? {
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    Ok(())
}

fn spawn_collector<C>(mut collector: C, tx: mpsc::Sender<zfsio_model::UiSnapshot>)
where
    C: SnapshotSource + 'static,
{
    tokio::spawn(async move {
        loop {
            match collector.next_snapshot().await {
                Ok(snapshot) => {
                    if tx.try_send(snapshot).is_err() {
                        // Low-interference behavior: skip samples instead of queueing unbounded work.
                    }
                }
                Err(error) => {
                    tracing::warn!(?error, "collector sample failed");
                }
            }
        }
    });
}

fn init_logging(path: Option<&std::path::Path>) -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    if let Some(path) = path {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(file)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(io::sink)
            .init();
    }
    Ok(())
}
