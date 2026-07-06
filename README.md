# zfs-io

`zfs-io` is proposed as a `btop`-style terminal user interface for live OpenZFS
observability. The first target is Linux OpenZFS on `x86_64`, followed by
`aarch64`, with a design that keeps platform-specific collectors isolated from
the interactive rendering layer.


## Current implementation status

The repository now contains an initial Rust workspace with a runnable `zfs-io`
binary. The first implementation is intentionally conservative: it includes a
mock telemetry mode for UI development and an OpenZFS collector scaffold that
falls back to safe mock data until read-only kstat parsing is completed.

Run the current TUI with generated data:

```bash
cargo run -p zfs-io -- --mock
```

The app uses bounded channels and skips queued samples when the renderer is
behind, preserving the low-interference behavior required for future real
OpenZFS collectors.

## Recommended implementation path

Build the first version in **Rust** rather than Go.

Rust is a better fit for the initial OpenZFS TUI because it has mature terminal
UI libraries, predictable low-latency rendering, strong cross-compilation
support for `x86_64` and `aarch64`, and a type system that helps keep the live
collector, time-series cache, and UI state machine reliable. Go remains a good
fallback for a daemon/exporter later, but the first interactive binary should be
Rust.

Suggested stack:

- `ratatui` for layout, widgets, tables, gauges, and charts.
- `crossterm` for terminal input, alternate-screen rendering, resize handling,
  and portable terminal control.
- `tokio` for concurrent polling tasks and UI event loops.
- `clap` for command-line flags such as refresh interval, pool filters, and
  read-only diagnostic modes.
- `tracing` plus a file appender for debug logs outside the TUI.

## Live data showcase

The TUI should make live ZFS behavior visible at three levels: host, pool, and
vdev/dataset. The default dashboard should refresh every second and keep a
rolling in-memory history for charts.

### Top dashboard

A `btop`-inspired overview should include:

- Pool health strip: pool name, state, capacity, fragmentation, dedup ratio,
  allocated/free bytes, and last scrub/resilver status.
- Throughput charts: stacked read/write bandwidth per pool and aggregate host
  totals.
- IOPS charts: read/write operations per second, with separate lines for sync
  and async operations when available.
- Latency panel: recent min/avg/max or percentile-style latency buckets from
  OpenZFS latency histograms where available.
- ARC panel: ARC size, target size, hit ratio, miss ratio, metadata pressure,
  MRU/MFU balance, and L2ARC read/write activity.
- Event ticker: recent `zed` or `zpool events` activity, scrub milestones,
  checksum/read/write errors, device removals, and resilver progress.

### Drill-down screens

Interactive screens should be reachable from the dashboard without leaving the
TUI:

- **Pools:** sortable table with capacity, health, fragmentation, errors,
  throughput, IOPS, and scrub/resilver state.
- **Vdevs:** tree view of mirrors, RAID-Z groups, spares, special vdevs,
  slog, cache, and individual disks with per-device read/write rates.
- **Datasets:** dataset and zvol table with logical/physical usage,
  compression ratio, quota/refquota, reservation, snapshots, and mountpoint.
- **ARC/L2ARC:** detailed cache charts and counters from `/proc/spl/kstat/zfs`.
- **Events:** live event feed with filters for pool, severity, and event type.

### Chart behavior

Live charts should be terminal-native and useful over SSH:

- Keep 60-second, 5-minute, and 1-hour ring buffers for each metric.
- Support sparkline mode for small terminals and full chart mode for wider
  terminals.
- Use fixed color semantics: reads blue/green, writes yellow/orange, errors red,
  capacity purple, cache cyan.
- Allow pause/resume so operators can inspect spikes without stopping the
  collectors.
- Offer keyboard zoom between time windows.

## OpenZFS data sources

The first Linux/OpenZFS collector should prefer low-overhead local sources,
avoid shelling out in the hot path, and never take actions that can block or
mutate ZFS state.

Primary sources:

- `/proc/spl/kstat/zfs/arcstats` for ARC and L2ARC counters.
- `/proc/spl/kstat/zfs/<pool>/io` and related kstats for pool-level I/O.
- `/proc/spl/kstat/zfs/<pool>/objset-*` when dataset-level kstats are exposed.
- `zpool status -P`, `zpool list -Hp`, and `zfs list -Hp` on slower intervals
  for topology, health, capacity, properties, and dataset inventory.
- `zpool events -f` or `zed` integration for an asynchronous event stream.

The collector should normalize all samples into internal structs with monotonic
timestamps. Counter deltas should be computed centrally so UI widgets only read
already-derived rates.

## Low-interference safety model

`zfs-io` should behave like a read-only observer. Its collectors must be designed
so they do not interfere with pool I/O, administrative ZFS commands, imports,
exports, scrubs, resilvers, or fault handling.

Required safeguards:

- Use read-only procfs/kstat reads for the fast path and avoid ioctls or commands
  that acquire heavy pool locks.
- Never run mutating commands such as `zpool scrub`, `zpool clear`, `zpool
  online`, `zpool offline`, `zpool import`, `zpool export`, `zfs set`, or `zfs
  destroy` from the TUI.
- Run `zpool` and `zfs` discovery commands on slow, jittered intervals with hard
  timeouts so a hung helper process cannot stall rendering or future samples.
- Put each external helper in its own cancellable task and drop late results
  rather than waiting on them in the UI thread.
- Use bounded channels and fixed-size ring buffers so memory growth cannot occur
  during long sessions or event storms.
- Apply backpressure by skipping samples when the system is overloaded instead of
  queueing unbounded work.
- Lower collector priority where available, for example best-effort `nice` and
  idle I/O priority for external helper processes.
- Prefer cached topology between slow refreshes; the one-second loop should only
  read lightweight counters and compute deltas.
- Degrade gracefully when files, counters, commands, or permissions are missing;
  show `unknown` or stale data markers instead of retrying aggressively.
- Keep event streaming optional and reconnect with exponential backoff if
  `zpool events -f` exits or blocks unexpectedly.

The failure mode should always be missing or delayed telemetry, never delayed ZFS
operations.

## Interaction model

Suggested key bindings:

| Key | Action |
| --- | --- |
| `q` / `Esc` | Quit |
| `?` | Help overlay |
| `Space` | Pause/resume live updates |
| `Tab` / `Shift+Tab` | Cycle screens |
| `1`..`5` | Jump to dashboard, pools, vdevs, datasets, ARC |
| `/` | Filter current table |
| `s` | Change sort column |
| `Enter` | Drill into selected pool, vdev, or dataset |
| `r` | Force topology refresh |
| `+` / `-` | Zoom chart time window |

## Architecture

Use a small workspace split into focused crates once implementation begins:

```text
crates/
  zfsio-app/        # binary, CLI, config, startup
  zfsio-collector/  # OpenZFS collectors and sample normalization
  zfsio-model/      # metric structs, ring buffers, derived rates
  zfsio-tui/        # ratatui views, widgets, input handling
```

Runtime flow:

1. CLI parses configuration and starts the terminal in alternate-screen mode.
2. Collector tasks sample fast counters every 1s and slow topology data every
   15s-60s using cancellable, timeout-bound work.
3. Samples are sent over bounded channels to the model layer.
4. The model updates ring buffers, derives rates, and snapshots immutable UI
   state.
5. The renderer draws at a capped frame rate, usually 10-15 FPS, while data
   samples update independently and late collector results are skipped.

## Portability plan

Initial release targets:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`

Keep the collector behind traits so future illumos, FreeBSD, or macOS OpenZFS
support can add platform-specific backends without rewriting the TUI. The first
portable milestone should be read-only and require no elevated privileges beyond
what the local OpenZFS tools and procfs/kstats already require.

## Milestones

1. **MVP dashboard:** Rust CLI, terminal lifecycle, mock data mode, ARC panel,
   pool list, throughput and IOPS charts.
2. **OpenZFS Linux collector:** parse kstats, poll `zpool`/`zfs` command output,
   compute rates, and handle missing counters gracefully.
3. **Interactive navigation:** add pool/vdev/dataset screens, filtering, sorting,
   help overlay, pause, and chart zoom.
4. **Event integration:** stream `zpool events`, highlight errors, and show scrub
   or resilver progress.
5. **Packaging:** release static or mostly-static binaries for `x86_64` and
   `aarch64`, plus distro packages after the CLI stabilizes.
