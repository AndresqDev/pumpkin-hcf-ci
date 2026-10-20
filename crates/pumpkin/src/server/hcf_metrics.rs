//! Lock-free HCF tick metrics (HCF-CORE-002).
//!
//! Every counter is a relaxed atomic; there is no mutex on the tick path. When
//! HCF is disabled [`HcfMetrics::section`] returns a guard that never calls
//! `Instant::now()` and never writes, so the vanilla path keeps its cost.
//!
//! The histogram has one bucket per millisecond for `0..=126 ms`; bucket 127
//! collects everything `>= 127 ms`. The p99 is therefore approximate (1 ms
//! resolution), while the average and maximum are exact.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tracing::info;

/// Number of histogram buckets. Bucket 127 is the overflow bucket.
const HISTOGRAM_BUCKETS: usize = 128;

/// How often the metrics line is emitted (30 s at 20 TPS).
const LOG_INTERVAL_TICKS: u64 = 600;

/// Tick phases measured by HCF-CORE-002.
///
/// The discriminants are the array indexes used by [`HcfMetrics`]. Keep them
/// stable: CI parses the `section=` names, and future patches add new names at
/// the end rather than reordering existing ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum HcfSection {
    TickTotal = 0,
    DatapackTick,
    Scheduler,
    ScheduledFunctions,
    Worlds,
    PlayerData,
    WorldFlush,
    ActiveChunks,
    Environment,
    Raids,
    Chunks,
    Players,
    Entities,
    Tracker,
    BlockEntities,
    DragonFight,
    BlockTicks,
    FluidTicks,
    RandomTicks,
    NaturalSpawning,
    InhabitedTime,
    PushEntities,
    MovementBroadcast,
    PluginEvents,
}

impl Default for HcfSection {
    fn default() -> Self {
        Self::TickTotal
    }
}

impl HcfSection {
    /// Number of sections known to this metrics ABI.
    pub const COUNT: usize = 24;

    /// All sections in log order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::TickTotal,
        Self::DatapackTick,
        Self::Scheduler,
        Self::ScheduledFunctions,
        Self::Worlds,
        Self::PlayerData,
        Self::WorldFlush,
        Self::ActiveChunks,
        Self::Environment,
        Self::Raids,
        Self::Chunks,
        Self::Players,
        Self::Entities,
        Self::Tracker,
        Self::BlockEntities,
        Self::DragonFight,
        Self::BlockTicks,
        Self::FluidTicks,
        Self::RandomTicks,
        Self::NaturalSpawning,
        Self::InhabitedTime,
        Self::PushEntities,
        Self::MovementBroadcast,
        Self::PluginEvents,
    ];

    /// Stable name used by the `HCF_METRIC` log line.
    #[inline]
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::TickTotal => "tick_total",
            Self::DatapackTick => "datapack_tick",
            Self::Scheduler => "scheduler",
            Self::ScheduledFunctions => "scheduled_functions",
            Self::Worlds => "worlds",
            Self::PlayerData => "player_data",
            Self::WorldFlush => "world_flush",
            Self::ActiveChunks => "active_chunks",
            Self::Environment => "environment",
            Self::Raids => "raids",
            Self::Chunks => "chunks",
            Self::Players => "players",
            Self::Entities => "entities",
            Self::Tracker => "tracker",
            Self::BlockEntities => "block_entities",
            Self::DragonFight => "dragon_fight",
            Self::BlockTicks => "block_ticks",
            Self::FluidTicks => "fluid_ticks",
            Self::RandomTicks => "random_ticks",
            Self::NaturalSpawning => "natural_spawning",
            Self::InhabitedTime => "inhabited_time",
            Self::PushEntities => "push_entities",
            Self::MovementBroadcast => "movement_broadcast",
            Self::PluginEvents => "plugin_events",
        }
    }

    /// Array index for this section.
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// Compact histogram and totals for one section.
#[derive(Debug)]
struct SectionMetrics {
    count: AtomicU64,
    total_nanos: AtomicU64,
    max_nanos: AtomicU64,
    histogram: [AtomicU64; HISTOGRAM_BUCKETS],
}

impl SectionMetrics {
    const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            total_nanos: AtomicU64::new(0),
            max_nanos: AtomicU64::new(0),
            histogram: [const { AtomicU64::new(0) }; HISTOGRAM_BUCKETS],
        }
    }

    fn record(&self, nanos: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.total_nanos.fetch_add(nanos, Ordering::Relaxed);
        self.max_nanos.fetch_max(nanos, Ordering::Relaxed);

        let bucket = usize::try_from(nanos / 1_000_000)
            .unwrap_or(HISTOGRAM_BUCKETS - 1)
            .min(HISTOGRAM_BUCKETS - 1);
        self.histogram[bucket].fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self, section: HcfSection) -> SectionSnapshot {
        let count = self.count.load(Ordering::Relaxed);
        let total_nanos = self.total_nanos.load(Ordering::Relaxed);
        let max_nanos = self.max_nanos.load(Ordering::Relaxed);

        // Ceil(count * 0.99): the first bucket whose cumulative count reaches
        // this value contains the p99 sample.
        let target = count.saturating_mul(99).div_ceil(100);
        let mut cumulative = 0u64;
        let mut p99_nanos = 0u64;
        if target > 0 {
            for (index, bucket) in self.histogram.iter().enumerate() {
                cumulative = cumulative.saturating_add(bucket.load(Ordering::Relaxed));
                if cumulative >= target {
                    // Bucket `index` covers `[index, index + 1) ms`; the last
                    // bucket is open-ended, so clamp it to 127 ms instead of
                    // claiming a 128 ms upper bound.
                    p99_nanos = if index == HISTOGRAM_BUCKETS - 1 {
                        127_000_000
                    } else {
                        (index as u64 + 1) * 1_000_000
                    };
                    break;
                }
            }
        }

        SectionSnapshot {
            section,
            count,
            total_nanos,
            max_nanos,
            p99_nanos,
        }
    }
}

/// Immutable snapshot of one section.
#[derive(Clone, Copy, Debug, Default)]
pub struct SectionSnapshot {
    pub section: HcfSection,
    pub count: u64,
    pub total_nanos: u64,
    pub max_nanos: u64,
    pub p99_nanos: u64,
}

impl SectionSnapshot {
    /// Average duration in nanoseconds, or `0` when the section was not hit.
    #[inline]
    #[must_use]
    pub const fn avg_nanos(&self) -> u64 {
        if self.count == 0 {
            0
        } else {
            self.total_nanos / self.count
        }
    }
}

/// Snapshot of every section, used by logs and future CI exports.
#[derive(Clone, Debug)]
pub struct HcfSnapshot {
    pub sections: [SectionSnapshot; HcfSection::COUNT],
}

/// Lock-free metrics registry shared by the server and its worlds.
#[derive(Debug)]
pub struct HcfMetrics {
    enabled: bool,
    sections: [SectionMetrics; HcfSection::COUNT],
}

impl Default for HcfMetrics {
    fn default() -> Self {
        Self::disabled()
    }
}

impl HcfMetrics {
    /// Creates a registry. A disabled registry is inert.
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            sections: [const { SectionMetrics::new() }; HcfSection::COUNT],
        }
    }

    /// Creates an inert registry.
    #[must_use]
    pub fn disabled() -> Self {
        Self::new(false)
    }

    /// Returns whether recording is enabled.
    #[inline]
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Starts a section timer. The returned guard records on `Drop`.
    ///
    /// When disabled, no clock is read and no atomic is written.
    #[inline]
    #[must_use = "the section is measured while the guard is alive"]
    pub fn section(&self, section: HcfSection) -> HcfSectionGuard<'_> {
        if self.enabled {
            HcfSectionGuard {
                metrics: Some(&self.sections[section.index()]),
                start: Some(Instant::now()),
            }
        } else {
            HcfSectionGuard {
                metrics: None,
                start: None,
            }
        }
    }

    /// Records a duration without a guard (used for the already-timed tick).
    #[inline]
    pub fn record(&self, section: HcfSection, nanos: u64) {
        if self.enabled {
            self.sections[section.index()].record(nanos);
        }
    }

    /// Returns a consistent-enough snapshot for logging.
    #[must_use]
    pub fn snapshot(&self) -> HcfSnapshot {
        HcfSnapshot {
            sections: std::array::from_fn(|index| {
                self.sections[index].snapshot(HcfSection::ALL[index])
            }),
        }
    }

    /// Emits one stable `HCF_METRIC` line per non-empty section every 600 ticks.
    ///
    /// This runs on the tick thread after `TickTotal` has been recorded, so the
    /// formatting cost is not included in that sample.
    pub fn maybe_log(&self, tick: u64) {
        if !self.enabled || tick == 0 || tick % LOG_INTERVAL_TICKS != 0 {
            return;
        }

        for section in self.snapshot().sections {
            if section.count == 0 {
                continue;
            }
            info!(
                "HCF_METRIC section={} count={} avg_ms={:.3} p99_ms={:.3} max_ms={:.3}",
                section.section.name(),
                section.count,
                section.avg_nanos() as f64 / 1_000_000.0,
                section.p99_nanos as f64 / 1_000_000.0,
                section.max_nanos as f64 / 1_000_000.0,
            );
        }
    }
}

/// RAII timer for one section.
#[derive(Debug)]
pub struct HcfSectionGuard<'a> {
    metrics: Option<&'a SectionMetrics>,
    start: Option<Instant>,
}

impl Drop for HcfSectionGuard<'_> {
    fn drop(&mut self) {
        if let (Some(metrics), Some(start)) = (self.metrics, self.start) {
            metrics.record(start.elapsed().as_nanos() as u64);
        }
    }
}
