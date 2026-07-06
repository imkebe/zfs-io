use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolState {
    Online,
    Degraded,
    Faulted,
    Unknown,
}

impl PoolState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Online => "ONLINE",
            Self::Degraded => "DEGRADED",
            Self::Faulted => "FAULTED",
            Self::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PoolSnapshot {
    pub name: String,
    pub state: PoolState,
    pub capacity_percent: f64,
    pub read_bytes_per_sec: f64,
    pub write_bytes_per_sec: f64,
    pub read_iops: f64,
    pub write_iops: f64,
    pub error_count: u64,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct ArcSnapshot {
    pub size_bytes: u64,
    pub target_bytes: u64,
    pub hit_ratio: f64,
    pub miss_ratio: f64,
}

#[derive(Debug, Clone)]
pub struct EventRecord {
    pub timestamp: Instant,
    pub severity: EventSeverity,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSeverity {
    Info,
    Warning,
    Error,
}

impl EventSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warn",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MetricPoint {
    pub timestamp: Instant,
    pub value: f64,
}

#[derive(Debug, Clone)]
pub struct RingBuffer<T> {
    capacity: usize,
    values: VecDeque<T>,
}

impl<T> RingBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "ring buffer capacity must be non-zero");
        Self {
            capacity,
            values: VecDeque::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, value: T) {
        if self.values.len() == self.capacity {
            self.values.pop_front();
        }
        self.values.push_back(value);
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.values.iter()
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct TimeSeries {
    pub last_minute: RingBuffer<MetricPoint>,
    pub last_five_minutes: RingBuffer<MetricPoint>,
    pub last_hour: RingBuffer<MetricPoint>,
}

impl Default for TimeSeries {
    fn default() -> Self {
        Self {
            last_minute: RingBuffer::new(60),
            last_five_minutes: RingBuffer::new(300),
            last_hour: RingBuffer::new(3_600),
        }
    }
}

impl TimeSeries {
    pub fn push(&mut self, timestamp: Instant, value: f64) {
        let point = MetricPoint { timestamp, value };
        self.last_minute.push(point.clone());
        self.last_five_minutes.push(point.clone());
        self.last_hour.push(point);
    }

    pub fn minute_values(&self) -> Vec<(f64, f64)> {
        let Some(first) = self.last_minute.iter().next() else {
            return Vec::new();
        };
        self.last_minute
            .iter()
            .map(|point| {
                (
                    point
                        .timestamp
                        .duration_since(first.timestamp)
                        .as_secs_f64(),
                    point.value,
                )
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct UiSnapshot {
    pub generated_at: Instant,
    pub stale_after: Duration,
    pub pools: Vec<PoolSnapshot>,
    pub arc: ArcSnapshot,
    pub events: Vec<EventRecord>,
    pub read_history: TimeSeries,
    pub write_history: TimeSeries,
}

impl UiSnapshot {
    pub fn is_stale(&self) -> bool {
        self.generated_at.elapsed() > self.stale_after
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_drops_oldest_values() {
        let mut buffer = RingBuffer::new(2);
        buffer.push(1);
        buffer.push(2);
        buffer.push(3);

        assert_eq!(buffer.iter().copied().collect::<Vec<_>>(), vec![2, 3]);
    }

    #[test]
    fn time_series_keeps_minute_history() {
        let mut series = TimeSeries::default();
        let start = Instant::now();

        for index in 0..65 {
            series.push(start + Duration::from_secs(index), index as f64);
        }

        assert_eq!(series.last_minute.len(), 60);
        assert_eq!(
            series.minute_values().first().map(|(_, value)| *value),
            Some(5.0)
        );
    }
}
