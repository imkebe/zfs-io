use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use tokio::time::{MissedTickBehavior, interval};
use zfsio_model::{
    ArcSnapshot, EventRecord, EventSeverity, PoolSnapshot, PoolState, TimeSeries, UiSnapshot,
};

const MAX_KSTAT_BYTES: u64 = 1024 * 1024;
const MAX_POOLS_PER_SAMPLE: usize = 256;

#[async_trait]
pub trait SnapshotSource: Send {
    async fn next_snapshot(&mut self) -> Result<UiSnapshot>;
}

#[derive(Debug, Clone)]
pub struct CollectorConfig {
    pub refresh_interval: Duration,
    pub stale_after: Duration,
    pub topology_refresh_interval: Duration,
    pub command_timeout: Duration,
    pub zfs_proc_root: PathBuf,
}

impl Default for CollectorConfig {
    fn default() -> Self {
        Self {
            refresh_interval: Duration::from_secs(1),
            stale_after: Duration::from_secs(5),
            topology_refresh_interval: Duration::from_secs(30),
            command_timeout: Duration::from_secs(2),
            zfs_proc_root: PathBuf::from("/proc/spl/kstat/zfs"),
        }
    }
}

pub struct MockCollector {
    config: CollectorConfig,
    tick: u64,
    read_history: TimeSeries,
    write_history: TimeSeries,
    ticker: tokio::time::Interval,
}

impl MockCollector {
    pub fn new(config: CollectorConfig) -> Self {
        let mut ticker = interval(config.refresh_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        Self {
            config,
            tick: 0,
            read_history: TimeSeries::default(),
            write_history: TimeSeries::default(),
            ticker,
        }
    }
}

#[async_trait]
impl SnapshotSource for MockCollector {
    async fn next_snapshot(&mut self) -> Result<UiSnapshot> {
        self.ticker.tick().await;
        self.tick = self.tick.saturating_add(1);
        let now = Instant::now();
        let wave = (self.tick as f64 / 8.0).sin().abs();
        let read = 80_000_000.0 + wave * 240_000_000.0;
        let write = 40_000_000.0 + (1.0 - wave) * 130_000_000.0;

        self.read_history.push(now, read);
        self.write_history.push(now, write);

        Ok(UiSnapshot {
            generated_at: now,
            stale_after: self.config.stale_after,
            pools: vec![PoolSnapshot {
                name: "tank".to_string(),
                state: PoolState::Online,
                capacity_percent: 61.0 + wave * 4.0,
                read_bytes_per_sec: read,
                write_bytes_per_sec: write,
                read_iops: 800.0 + wave * 1_200.0,
                write_iops: 350.0 + (1.0 - wave) * 700.0,
                error_count: 0,
                status: "mock telemetry".to_string(),
            }],
            arc: ArcSnapshot {
                size_bytes: 32 * 1024 * 1024 * 1024,
                target_bytes: 48 * 1024 * 1024 * 1024,
                hit_ratio: 0.94 + wave * 0.03,
                miss_ratio: 0.03 + (1.0 - wave) * 0.03,
            },
            events: vec![EventRecord {
                timestamp: now,
                severity: EventSeverity::Info,
                message: "mock collector active; no ZFS commands are being run".to_string(),
            }],
            read_history: self.read_history.clone(),
            write_history: self.write_history.clone(),
        })
    }
}

pub struct LinuxOpenZfsCollector {
    config: CollectorConfig,
    ticker: tokio::time::Interval,
    previous_pool_io: HashMap<String, TimedPoolIoCounters>,
    read_history: TimeSeries,
    write_history: TimeSeries,
}

impl LinuxOpenZfsCollector {
    pub fn new(config: CollectorConfig) -> Self {
        let mut ticker = interval(config.refresh_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        Self {
            config,
            ticker,
            previous_pool_io: HashMap::new(),
            read_history: TimeSeries::default(),
            write_history: TimeSeries::default(),
        }
    }

    fn collect_now(&mut self, now: Instant) -> UiSnapshot {
        let arc = read_arc_snapshot(&self.config.zfs_proc_root).unwrap_or_else(empty_arc_snapshot);
        let pools =
            read_pool_snapshots(&self.config.zfs_proc_root, now, &mut self.previous_pool_io);
        let total_read = pools
            .iter()
            .map(|pool| pool.read_bytes_per_sec)
            .sum::<f64>();
        let total_write = pools
            .iter()
            .map(|pool| pool.write_bytes_per_sec)
            .sum::<f64>();
        self.read_history.push(now, total_read);
        self.write_history.push(now, total_write);

        let events = if pools.is_empty() {
            vec![EventRecord {
                timestamp: now,
                severity: EventSeverity::Warning,
                message: "OpenZFS kstats unavailable or no pools found; telemetry is empty"
                    .to_string(),
            }]
        } else {
            vec![EventRecord {
                timestamp: now,
                severity: EventSeverity::Info,
                message: "OpenZFS kstats sampled from procfs".to_string(),
            }]
        };

        UiSnapshot {
            generated_at: now,
            stale_after: self.config.stale_after,
            pools,
            arc,
            events,
            read_history: self.read_history.clone(),
            write_history: self.write_history.clone(),
        }
    }
}

#[async_trait]
impl SnapshotSource for LinuxOpenZfsCollector {
    async fn next_snapshot(&mut self) -> Result<UiSnapshot> {
        self.ticker.tick().await;
        Ok(self.collect_now(Instant::now()))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PoolIoCounters {
    read_bytes: u64,
    write_bytes: u64,
    read_ops: u64,
    write_ops: u64,
}

#[derive(Debug, Clone, Copy)]
struct TimedPoolIoCounters {
    sampled_at: Instant,
    counters: PoolIoCounters,
}

fn parse_kstat_rows(contents: &str) -> HashMap<String, u64> {
    contents
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let name = fields.next()?;
            let _data_type = fields.next()?;
            let data = fields.next()?;
            if fields.next().is_some() || name == "name" {
                return None;
            }
            data.parse::<u64>()
                .ok()
                .map(|value| (name.to_string(), value))
        })
        .collect()
}

fn parse_arc_snapshot(contents: &str) -> ArcSnapshot {
    let rows = parse_kstat_rows(contents);
    let hits = rows.get("hits").copied().unwrap_or_default();
    let misses = rows.get("misses").copied().unwrap_or_default();
    let accesses = hits.saturating_add(misses);
    let hit_ratio = ratio(hits, accesses);
    let miss_ratio = ratio(misses, accesses);

    ArcSnapshot {
        size_bytes: rows.get("size").copied().unwrap_or_default(),
        target_bytes: rows
            .get("c")
            .or_else(|| rows.get("target_size"))
            .copied()
            .unwrap_or_default(),
        hit_ratio,
        miss_ratio,
    }
}

fn ratio(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 / total as f64
    }
}

fn parse_pool_io(contents: &str) -> Option<PoolIoCounters> {
    let mut lines = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    let header = lines.find(|line| line.split_whitespace().any(|field| field == "nread"))?;
    let values = lines.next()?;
    let headers = header.split_whitespace().collect::<Vec<_>>();
    let columns = values.split_whitespace().collect::<Vec<_>>();

    Some(PoolIoCounters {
        read_bytes: column_u64(&headers, &columns, "nread")?,
        write_bytes: column_u64(&headers, &columns, "nwritten")?,
        read_ops: column_u64(&headers, &columns, "reads")?,
        write_ops: column_u64(&headers, &columns, "writes")?,
    })
}

fn column_u64(headers: &[&str], columns: &[&str], name: &str) -> Option<u64> {
    let index = headers.iter().position(|header| *header == name)?;
    columns.get(index)?.parse().ok()
}

fn empty_arc_snapshot() -> ArcSnapshot {
    ArcSnapshot {
        size_bytes: 0,
        target_bytes: 0,
        hit_ratio: 0.0,
        miss_ratio: 0.0,
    }
}

fn read_arc_snapshot(root: &Path) -> Option<ArcSnapshot> {
    read_kstat_file(&root.join("arcstats"))
        .ok()
        .map(|contents| parse_arc_snapshot(&contents))
}

fn read_kstat_file(path: &Path) -> io::Result<String> {
    let file = File::open(path)?;
    let mut contents = String::new();
    file.take(MAX_KSTAT_BYTES).read_to_string(&mut contents)?;
    Ok(contents)
}

fn read_pool_snapshots(
    root: &Path,
    now: Instant,
    previous: &mut HashMap<String, TimedPoolIoCounters>,
) -> Vec<PoolSnapshot> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };

    let mut pools = Vec::new();
    let mut sampled_pool_names = HashSet::new();
    for entry in entries.take(MAX_POOLS_PER_SAMPLE).flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(contents) = read_kstat_file(&entry.path().join("io")) else {
            continue;
        };
        let Some(counters) = parse_pool_io(&contents) else {
            continue;
        };
        let previous_sample = previous.remove(name.as_str());
        pools.push(pool_snapshot_from_counters(
            name.clone(),
            counters,
            now,
            previous_sample,
        ));
        sampled_pool_names.insert(name.clone());
        previous.insert(
            name,
            TimedPoolIoCounters {
                sampled_at: now,
                counters,
            },
        );
    }
    previous.retain(|name, _| sampled_pool_names.contains(name));
    pools.sort_by(|left, right| left.name.cmp(&right.name));
    pools
}

fn pool_snapshot_from_counters(
    name: String,
    counters: PoolIoCounters,
    now: Instant,
    previous: Option<TimedPoolIoCounters>,
) -> PoolSnapshot {
    let elapsed = previous
        .and_then(|sample| {
            now.checked_duration_since(sample.sampled_at)
                .map(|duration| (sample, duration))
        })
        .filter(|(_, duration)| !duration.is_zero());

    let (read_bytes_per_sec, write_bytes_per_sec, read_iops, write_iops) =
        if let Some((sample, duration)) = elapsed {
            let seconds = duration.as_secs_f64();
            (
                counters
                    .read_bytes
                    .saturating_sub(sample.counters.read_bytes) as f64
                    / seconds,
                counters
                    .write_bytes
                    .saturating_sub(sample.counters.write_bytes) as f64
                    / seconds,
                counters.read_ops.saturating_sub(sample.counters.read_ops) as f64 / seconds,
                counters.write_ops.saturating_sub(sample.counters.write_ops) as f64 / seconds,
            )
        } else {
            (0.0, 0.0, 0.0, 0.0)
        };

    PoolSnapshot {
        name,
        state: PoolState::Unknown,
        capacity_percent: 0.0,
        read_bytes_per_sec,
        write_bytes_per_sec,
        read_iops,
        write_iops,
        error_count: 0,
        status: "pool io kstat".to_string(),
    }
}

pub fn bytes_per_second(value: f64) -> String {
    const UNITS: &[&str] = &["B/s", "KiB/s", "MiB/s", "GiB/s", "TiB/s"];
    let mut size = value.max(0.0);
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARCSTATS: &str = include_str!("../fixtures/arcstats.sample");
    const POOL_IO_FIRST: &str = include_str!("../fixtures/pool_io_first.sample");
    const POOL_IO_SECOND: &str = include_str!("../fixtures/pool_io_second.sample");

    #[test]
    fn formats_byte_rates() {
        assert_eq!(bytes_per_second(1_048_576.0), "1.0 MiB/s");
    }

    #[test]
    fn default_config_is_low_interference() {
        let config = CollectorConfig::default();
        assert_eq!(config.refresh_interval, Duration::from_secs(1));
        assert_eq!(config.command_timeout, Duration::from_secs(2));
        assert!(config.topology_refresh_interval >= Duration::from_secs(15));
    }

    #[test]
    fn parses_kstat_rows_from_arcstats_fixture() {
        let rows = parse_kstat_rows(ARCSTATS);

        assert_eq!(rows.get("hits"), Some(&9_000));
        assert_eq!(rows.get("misses"), Some(&1_000));
        assert_eq!(rows.get("size"), Some(&(4 * 1024 * 1024 * 1024)));
        assert_eq!(rows.get("c"), Some(&(8 * 1024 * 1024 * 1024)));
        assert!(!rows.contains_key("name"));
    }

    #[test]
    fn computes_arc_hit_and_miss_ratios_from_counters() {
        let arc = parse_arc_snapshot(ARCSTATS);

        assert_eq!(arc.size_bytes, 4 * 1024 * 1024 * 1024);
        assert_eq!(arc.target_bytes, 8 * 1024 * 1024 * 1024);
        assert_eq!(arc.hit_ratio, 0.9);
        assert_eq!(arc.miss_ratio, 0.1);
    }

    #[test]
    fn zeroes_arc_ratios_when_access_counters_are_absent() {
        let arc = parse_arc_snapshot(
            "name                            type data\nsize                            4    1024\nc                               4    2048\n",
        );

        assert_eq!(arc.hit_ratio, 0.0);
        assert_eq!(arc.miss_ratio, 0.0);
    }

    #[test]
    fn pool_io_first_sample_reports_zero_rates_and_records_history() {
        let start = Instant::now();
        let counters = parse_pool_io(POOL_IO_FIRST).expect("fixture should include io counters");

        let snapshot = pool_snapshot_from_counters("tank".to_string(), counters, start, None);

        assert_eq!(snapshot.name, "tank");
        assert_eq!(snapshot.read_bytes_per_sec, 0.0);
        assert_eq!(snapshot.write_bytes_per_sec, 0.0);
        assert_eq!(snapshot.read_iops, 0.0);
        assert_eq!(snapshot.write_iops, 0.0);
    }

    #[test]
    fn pool_io_second_sample_reports_counter_delta_rates() {
        let start = Instant::now();
        let second = start + Duration::from_secs(2);
        let first_counters = parse_pool_io(POOL_IO_FIRST).expect("first fixture should parse");
        let second_counters = parse_pool_io(POOL_IO_SECOND).expect("second fixture should parse");
        let previous = TimedPoolIoCounters {
            sampled_at: start,
            counters: first_counters,
        };

        let snapshot = pool_snapshot_from_counters(
            "tank".to_string(),
            second_counters,
            second,
            Some(previous),
        );

        assert_eq!(snapshot.read_bytes_per_sec, 1_024.0);
        assert_eq!(snapshot.write_bytes_per_sec, 2_048.0);
        assert_eq!(snapshot.read_iops, 5.0);
        assert_eq!(snapshot.write_iops, 7.5);
    }

    #[tokio::test]
    async fn collector_ring_histories_store_first_and_second_pool_rates() {
        let root = unique_temp_dir("ring_histories");
        fs::create_dir_all(root.join("tank")).expect("temp pool dir");
        fs::write(root.join("arcstats"), ARCSTATS).expect("arcstats fixture");
        fs::write(root.join("tank/io"), POOL_IO_FIRST).expect("first pool io fixture");

        let mut collector = LinuxOpenZfsCollector::new(CollectorConfig {
            zfs_proc_root: root.clone(),
            ..CollectorConfig::default()
        });
        let first_time = Instant::now();
        let first = collector.collect_now(first_time);

        fs::write(root.join("tank/io"), POOL_IO_SECOND).expect("second pool io fixture");
        let second = collector.collect_now(first_time + Duration::from_secs(2));

        assert_eq!(first.pools[0].read_bytes_per_sec, 0.0);
        assert_eq!(second.pools[0].read_bytes_per_sec, 1_024.0);
        assert_eq!(second.read_history.last_minute.len(), 2);
        assert_eq!(second.write_history.last_minute.len(), 2);
        assert_eq!(minute_values(&second.read_history), vec![0.0, 1_024.0]);
        assert_eq!(minute_values(&second.write_history), vec![0.0, 2_048.0]);

        fs::remove_dir_all(root).expect("cleanup temp fixture root");
    }

    #[tokio::test]
    async fn missing_proc_files_degrade_to_empty_snapshot() {
        let root = unique_temp_dir("missing_files");
        fs::create_dir_all(&root).expect("temp proc root");
        let mut collector = LinuxOpenZfsCollector::new(CollectorConfig {
            zfs_proc_root: root.clone(),
            ..CollectorConfig::default()
        });

        let snapshot = collector.collect_now(Instant::now());

        assert!(snapshot.pools.is_empty());
        assert_eq!(snapshot.arc.size_bytes, 0);
        assert_eq!(snapshot.arc.target_bytes, 0);
        assert_eq!(snapshot.arc.hit_ratio, 0.0);
        assert_eq!(snapshot.arc.miss_ratio, 0.0);
        assert_eq!(snapshot.events[0].severity, EventSeverity::Warning);
        assert_eq!(snapshot.read_history.last_minute.len(), 1);
        assert_eq!(minute_values(&snapshot.read_history), vec![0.0]);

        fs::remove_dir_all(root).expect("cleanup temp fixture root");
    }

    fn minute_values(series: &TimeSeries) -> Vec<f64> {
        series.last_minute.iter().map(|point| point.value).collect()
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "zfsio-collector-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }
}
