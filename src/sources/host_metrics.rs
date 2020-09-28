use crate::{
    config::{DataType, GlobalOptions, SourceConfig, SourceDescription},
    event::{
        metric::{Metric, MetricKind, MetricValue},
        Event,
    },
    shutdown::ShutdownSignal,
    Pipeline,
};
use chrono::{DateTime, Utc};
use futures::{
    compat::Future01CompatExt,
    future::{FutureExt, TryFutureExt},
    stream::{self, StreamExt},
};
use futures01::Sink;
use glob::{Pattern, PatternError};
#[cfg(target_os = "macos")]
use heim::memory::os::macos::MemoryExt;
#[cfg(not(target_os = "windows"))]
use heim::memory::os::SwapExt;
#[cfg(target_os = "windows")]
use heim::net::os::windows::IoCountersExt;
#[cfg(target_os = "linux")]
use heim::{
    cpu::os::linux::CpuTimeExt, memory::os::linux::MemoryExt, net::os::linux::IoCountersExt,
};
use heim::{
    units::{information::byte, ratio::ratio, time::second},
    Error,
};
use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer, Serialize, Serializer,
};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;
use tokio::{select, time};

macro_rules! btreemap {
    ( $( $key:expr => $value:expr ),* ) => {{
        #[allow(unused_mut)]
        let mut result = std::collections::BTreeMap::default();
        $( result.insert($key, $value); )*
            result
    }}
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Collector {
    Cpu,
    Disk,
    Filesystem,
    Load,
    Memory,
    Network,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct DiskConfig {
    devices: Option<Vec<PatternWrapper>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct FilesystemConfig {
    devices: Option<Vec<PatternWrapper>>,
    filesystems: Option<Vec<PatternWrapper>>,
    mountpoints: Option<Vec<PatternWrapper>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct NetworkConfig {
    devices: Option<Vec<PatternWrapper>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Namespace(String);

impl Default for Namespace {
    fn default() -> Self {
        Self("host".into())
    }
}

impl Namespace {
    fn encode(&self, word: &str) -> String {
        if self.0.is_empty() {
            word.into()
        } else {
            format!("{}_{}", self.0, word)
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct HostMetricsConfig {
    #[serde(default = "default_scrape_interval")]
    scrape_interval_secs: u64,

    collectors: Option<Vec<Collector>>,
    #[serde(default)]
    namespace: Namespace,

    #[serde(default)]
    disk: DiskConfig,
    #[serde(default)]
    filesystem: FilesystemConfig,
    #[serde(default)]
    network: NetworkConfig,
}

const fn default_scrape_interval() -> u64 {
    15
}

inventory::submit! {
    SourceDescription::new::<HostMetricsConfig>("host_metrics")
}

#[typetag::serde(name = "host_metrics")]
impl SourceConfig for HostMetricsConfig {
    fn build(
        &self,
        _name: &str,
        _globals: &GlobalOptions,
        shutdown: ShutdownSignal,
        out: Pipeline,
    ) -> crate::Result<super::Source> {
        Ok(Box::new(self.clone().run(out, shutdown).boxed().compat()))
    }

    fn output_type(&self) -> DataType {
        DataType::Metric
    }

    fn source_type(&self) -> &'static str {
        "host_metrics"
    }
}

macro_rules! tags {
    ( $( $key:expr => $value:expr ),* ) => {{
        #[allow(unused_mut)]
        let mut result = std::collections::BTreeMap::default();
        $( result.insert($key.to_string(), $value.to_string()); )*
            result
    }}
}

impl HostMetricsConfig {
    async fn run(self, mut out: Pipeline, shutdown: ShutdownSignal) -> Result<(), ()> {
        let interval = Duration::from_secs(self.scrape_interval_secs);
        let mut interval = time::interval(interval).map(|_| ());
        let mut shutdown = shutdown.compat();

        loop {
            select! {
                Some(()) = interval.next() => (),
                _ = &mut shutdown => break,
                else => break,
            };

            let metrics = self.capture_metrics().await;

            let (sink, _) = out
                .send_all(futures01::stream::iter_ok(metrics))
                .compat()
                .await
                .map_err(|error| error!(message = "Error sending host metrics", %error))?;
            out = sink;
        }

        Ok(())
    }

    fn has_collector(&self, collector: Collector) -> bool {
        match &self.collectors {
            None => true,
            Some(collectors) => collectors.iter().any(|&c| c == collector),
        }
    }

    async fn capture_metrics(&self) -> impl Iterator<Item = Event> {
        let hostname = crate::get_hostname();
        let mut metrics = Vec::new();
        if self.has_collector(Collector::Cpu) {
            metrics.extend(add_collector("cpu", self.cpu_metrics().await));
        }
        if self.has_collector(Collector::Disk) {
            metrics.extend(add_collector("disk", self.disk_metrics().await));
        }
        if self.has_collector(Collector::Filesystem) {
            metrics.extend(add_collector("filesystem", self.filesystem_metrics().await));
        }
        if self.has_collector(Collector::Load) {
            metrics.extend(add_collector("load", self.loadavg_metrics().await));
        }
        if self.has_collector(Collector::Memory) {
            metrics.extend(add_collector("memory", self.memory_metrics().await));
            metrics.extend(add_collector("memory", self.swap_metrics().await));
        }
        if self.has_collector(Collector::Network) {
            metrics.extend(add_collector("network", self.network_metrics().await));
        }
        if let Ok(hostname) = &hostname {
            for metric in &mut metrics {
                (metric.tags.as_mut().unwrap()).insert("host".into(), hostname.into());
            }
        }
        metrics.into_iter().map(Into::into)
    }

    async fn cpu_metrics(&self) -> Vec<Metric> {
        match heim::cpu::times().await {
            Ok(times) => {
                times
                    .filter_map(|result| filter_result(result, "Failed to load/parse CPU time"))
                    .map(|times| {
                        let timestamp = Utc::now();
                        let name = "cpu_seconds_total";
                        stream::iter(
                            vec![
                                self.counter(
                                    name,
                                    timestamp,
                                    times.idle().get::<second>(),
                                    tags!["mode" => "idle"],
                                ),
                                #[cfg(target_os = "linux")]
                                self.counter(
                                    name,
                                    timestamp,
                                    times.nice().get::<second>(),
                                    tags!["mode" => "nice"],
                                ),
                                self.counter(
                                    name,
                                    timestamp,
                                    times.system().get::<second>(),
                                    tags!["mode" => "system"],
                                ),
                                self.counter(
                                    name,
                                    timestamp,
                                    times.user().get::<second>(),
                                    tags!["mode" => "user"],
                                ),
                            ]
                            .into_iter(),
                        )
                    })
                    .flatten()
                    .collect::<Vec<_>>()
                    .await
            }
            Err(error) => {
                error!(message = "Failed to load CPU times", %error, rate_limit_secs = 60);
                vec![]
            }
        }
    }

    async fn memory_metrics(&self) -> Vec<Metric> {
        match heim::memory::memory().await {
            Ok(memory) => {
                let timestamp = Utc::now();
                vec![
                    self.gauge(
                        "memory_total_bytes",
                        timestamp,
                        memory.total().get::<byte>() as f64,
                        tags![],
                    ),
                    self.gauge(
                        "memory_free_bytes",
                        timestamp,
                        memory.free().get::<byte>() as f64,
                        tags![],
                    ),
                    self.gauge(
                        "memory_available_bytes",
                        timestamp,
                        memory.available().get::<byte>() as f64,
                        tags![],
                    ),
                    #[cfg(target_os = "linux")]
                    self.gauge(
                        "memory_active_bytes",
                        timestamp,
                        memory.active().get::<byte>() as f64,
                        tags![],
                    ),
                    #[cfg(target_os = "linux")]
                    self.gauge(
                        "memory_buffers_bytes",
                        timestamp,
                        memory.buffers().get::<byte>() as f64,
                        tags![],
                    ),
                    #[cfg(target_os = "linux")]
                    self.gauge(
                        "memory_cached_bytes",
                        timestamp,
                        memory.cached().get::<byte>() as f64,
                        tags![],
                    ),
                    #[cfg(target_os = "linux")]
                    self.gauge(
                        "memory_shared_bytes",
                        timestamp,
                        memory.shared().get::<byte>() as f64,
                        tags![],
                    ),
                    #[cfg(target_os = "linux")]
                    self.gauge(
                        "memory_used_bytes",
                        timestamp,
                        memory.used().get::<byte>() as f64,
                        tags![],
                    ),
                    #[cfg(target_os = "macos")]
                    self.gauge(
                        "memory_active_bytes",
                        timestamp,
                        memory.active().get::<byte>() as f64,
                        tags![],
                    ),
                    #[cfg(target_os = "macos")]
                    self.gauge(
                        "memory_inactive_bytes",
                        timestamp,
                        memory.inactive().get::<byte>() as f64,
                        tags![],
                    ),
                    #[cfg(target_os = "macos")]
                    self.gauge(
                        "memory_wired_bytes",
                        timestamp,
                        memory.wire().get::<byte>() as f64,
                        tags![],
                    ),
                ]
            }
            Err(error) => {
                error!(message = "Failed to load memory info", %error, rate_limit_secs = 60);
                vec![]
            }
        }
    }

    async fn swap_metrics(&self) -> Vec<Metric> {
        match heim::memory::swap().await {
            Ok(swap) => {
                let timestamp = Utc::now();
                vec![
                    self.gauge(
                        "memory_swap_free_bytes",
                        timestamp,
                        swap.free().get::<byte>() as f64,
                        tags![],
                    ),
                    self.gauge(
                        "memory_swap_total_bytes",
                        timestamp,
                        swap.total().get::<byte>() as f64,
                        tags![],
                    ),
                    self.gauge(
                        "memory_swap_used_bytes",
                        timestamp,
                        swap.used().get::<byte>() as f64,
                        tags![],
                    ),
                    #[cfg(not(target_os = "windows"))]
                    self.counter(
                        "memory_swapped_in_bytes_total",
                        timestamp,
                        swap.sin().map(|swap| swap.get::<byte>()).unwrap_or(0) as f64,
                        tags![],
                    ),
                    #[cfg(not(target_os = "windows"))]
                    self.counter(
                        "memory_swapped_out_bytes_total",
                        timestamp,
                        swap.sout().map(|swap| swap.get::<byte>()).unwrap_or(0) as f64,
                        tags![],
                    ),
                ]
            }
            Err(error) => {
                error!(message = "Failed to load swap info", %error, rate_limit_secs = 60);
                vec![]
            }
        }
    }

    async fn loadavg_metrics(&self) -> Vec<Metric> {
        #[cfg(unix)]
        let result = match heim::cpu::os::unix::loadavg().await {
            Ok(loadavg) => {
                let timestamp = Utc::now();
                vec![
                    self.gauge("load1", timestamp, loadavg.0.get::<ratio>() as f64, tags![]),
                    self.gauge("load5", timestamp, loadavg.1.get::<ratio>() as f64, tags![]),
                    self.gauge(
                        "load15",
                        timestamp,
                        loadavg.2.get::<ratio>() as f64,
                        tags![],
                    ),
                ]
            }
            Err(error) => {
                error!(message = "Failed to load load average info", %error, rate_limit_secs = 60);
                vec![]
            }
        };
        #[cfg(not(unix))]
        let result = vec![];

        result
    }

    async fn network_metrics(&self) -> Vec<Metric> {
        match heim::net::io_counters().await {
            Ok(counters) => {
                counters
                    .filter_map(|result| filter_result(result, "Failed to load/parse network data"))
                    // The following pair should be possible to do in one
                    // .filter_map, but it results in a strange "one type is
                    // more general than the other" error.
                    .map(|counter| {
                        vec_contains_str(&self.network.devices, counter.interface())
                            .map(|()| counter)
                    })
                    .filter_map(|counter| async { counter })
                    .map(|counter| {
                        let timestamp = Utc::now();
                        let interface = counter.interface();
                        stream::iter(
                            vec![
                                self.counter(
                                    "network_receive_bytes_total",
                                    timestamp,
                                    counter.bytes_recv().get::<byte>() as f64,
                                    tags!["device" => interface],
                                ),
                                self.counter(
                                    "network_receive_errs_total",
                                    timestamp,
                                    counter.errors_recv() as f64,
                                    tags!["device" => interface],
                                ),
                                self.counter(
                                    "network_receive_packets_drop_total",
                                    timestamp,
                                    counter.drop_sent() as f64,
                                    tags!["device" => interface],
                                ),
                                self.counter(
                                    "network_receive_packets_total",
                                    timestamp,
                                    counter.drop_recv() as f64,
                                    tags!["device" => interface],
                                ),
                                self.counter(
                                    "network_transmit_bytes_total",
                                    timestamp,
                                    counter.bytes_sent().get::<byte>() as f64,
                                    tags!["device" => interface],
                                ),
                                self.counter(
                                    "network_transmit_errs_total",
                                    timestamp,
                                    counter.errors_sent() as f64,
                                    tags!["device" => interface],
                                ),
                                #[cfg(any(target_os = "windows", target_os = "linux"))]
                                self.counter(
                                    "network_transmit_packets_total",
                                    timestamp,
                                    counter.packets_sent() as f64,
                                    tags!["device" => interface],
                                ),
                            ]
                            .into_iter(),
                        )
                    })
                    .flatten()
                    .collect::<Vec<_>>()
                    .await
            }
            Err(error) => {
                error!(message = "Failed to load network I/O counters", %error, rate_limit_secs = 60);
                vec![]
            }
        }
    }

    async fn filesystem_metrics(&self) -> Vec<Metric> {
        match heim::disk::partitions().await {
            Ok(partitions) => {
                partitions
                .filter_map(|result| filter_result(result, "Failed to load/parse partition data"))
                // Filter on configured mountpoints
                .map(|partition| {
                    vec_contains_path(&self.filesystem.mountpoints, partition.mount_point()).map(|()| partition)
                })
                .filter_map(|partition| async { partition })
                // Filter on configured devices
                .map(|partition| match &self.filesystem.devices {
                    Some(_) => partition
                        .device()
                        .and_then(|device| vec_contains_path(&self.filesystem.devices, device.as_ref())),
                    None => Some(())
                }.map(|()| partition))
                .filter_map(|partition| async { partition })
                // Filter on configured filesystems
                .map(|partition| {
                    vec_contains_str(&self.filesystem.filesystems, partition.file_system().as_str())
                        .map(|()| partition)
                })
                .filter_map(|partition| async { partition })
                // Load usage from the partition mount point
                .filter_map(|partition| async {
                    heim::disk::usage(partition.mount_point())
                        .await
                        .map(|usage| (partition, usage))
                        .map_err(|error| {
                            error!(message = "Failed to load partition usage data", %error, rate_limit_secs = 60)
                        })
                        .ok()
                })
                .map(|(partition, usage)| {
                    let timestamp = Utc::now();
                    let fs = partition.file_system();
                    let mut tags = btreemap![
                        "filesystem".to_string() => fs.as_str().to_string(),
                        "mountpoint".into() => partition.mount_point().to_string_lossy().into()
                    ];
                    if let Some(device) = partition.device() {
                        tags.insert("device".into(), device.to_string_lossy().into());
                    }
                    stream::iter(
                        vec![
                            self.gauge(
                                "filesystem_free_bytes",
                                timestamp,
                                usage.free().get::<byte>() as f64,
                                tags.clone()
                            ),
                            self.gauge(
                                "filesystem_total_bytes",
                                timestamp,
                                usage.total().get::<byte>() as f64,
                                tags.clone()
                            ),
                            self.gauge(
                                "filesystem_used_bytes",
                                timestamp,
                                usage.used().get::<byte>() as f64,
                                tags
                            ),
                        ]
                        .into_iter(),
                    )
                })
                .flatten()
                .collect::<Vec<_>>()
                .await
            }
            Err(error) => {
                error!(message = "Failed to load partitions info", %error, rate_limit_secs = 60);
                vec![]
            }
        }
    }

    async fn disk_metrics(&self) -> Vec<Metric> {
        match heim::disk::io_counters().await {
            Ok(counters) => {
                counters
                    .filter_map(|result| {
                        filter_result(result, "Failed to load/parse disk I/O data")
                    })
                    .map(|counter| {
                        vec_contains_path(&self.disk.devices, counter.device_name().as_ref())
                            .map(|()| counter)
                    })
                    .filter_map(|counter| async { counter })
                    .map(|counter| {
                        let timestamp = Utc::now();
                        let tags = btreemap![
                            "device".into() => counter.device_name().to_string_lossy().to_string()
                        ];
                        stream::iter(
                            vec![
                                self.counter(
                                    "disk_read_bytes_total",
                                    timestamp,
                                    counter.read_bytes().get::<byte>() as f64,
                                    tags.clone(),
                                ),
                                self.counter(
                                    "disk_reads_completed_total",
                                    timestamp,
                                    counter.read_count() as f64,
                                    tags.clone(),
                                ),
                                self.counter(
                                    "disk_written_bytes_total",
                                    timestamp,
                                    counter.write_bytes().get::<byte>() as f64,
                                    tags.clone(),
                                ),
                                self.counter(
                                    "disk_writes_completed_total",
                                    timestamp,
                                    counter.write_count() as f64,
                                    tags,
                                ),
                            ]
                            .into_iter(),
                        )
                    })
                    .flatten()
                    .collect::<Vec<_>>()
                    .await
            }
            Err(error) => {
                error!(message = "Failed to load disk I/O info", %error, rate_limit_secs = 60);
                vec![]
            }
        }
    }

    fn counter(
        &self,
        name: &str,
        timestamp: DateTime<Utc>,
        value: f64,
        tags: BTreeMap<String, String>,
    ) -> Metric {
        Metric {
            name: self.namespace.encode(name),
            timestamp: Some(timestamp),
            kind: MetricKind::Absolute,
            value: MetricValue::Counter { value },
            tags: Some(tags),
        }
    }

    fn gauge(
        &self,
        name: &str,
        timestamp: DateTime<Utc>,
        value: f64,
        tags: BTreeMap<String, String>,
    ) -> Metric {
        Metric {
            name: self.namespace.encode(name),
            timestamp: Some(timestamp),
            kind: MetricKind::Absolute,
            value: MetricValue::Gauge { value },
            tags: Some(tags),
        }
    }
}

async fn filter_result<T>(result: Result<T, Error>, message: &'static str) -> Option<T> {
    result
        .map_err(|error| error!(message, %error, rate_limit_secs = 60))
        .ok()
}

fn vec_contains_path(vec: &Option<Vec<PatternWrapper>>, value: &Path) -> Option<()> {
    match vec {
        // No patterns list matches everything
        None => Some(()),
        // Otherwise find the given value
        Some(vec) => vec
            .iter()
            .find(|&pattern| pattern.matches_path(value))
            .map(|_| ()),
    }
}

fn vec_contains_str(vec: &Option<Vec<PatternWrapper>>, value: &str) -> Option<()> {
    match vec {
        // No patterns list matches everything
        None => Some(()),
        // Otherwise find the given value
        Some(vec) => vec
            .iter()
            .find(|&pattern| pattern.matches(value))
            .map(|_| ()),
    }
}

fn add_collector(collector: &str, mut metrics: Vec<Metric>) -> Vec<Metric> {
    for metric in &mut metrics {
        (metric.tags.as_mut().unwrap()).insert("collector".into(), collector.into());
    }
    metrics
}

// Pattern doesn't implement Deserialize or Serialize, and we can't
// implement them ourselves due the orphan rules, so make a wrapper.
// This also adds support for negative patterns.
#[derive(Clone, Debug)]
struct PatternWrapper {
    negate: bool,
    pattern: Pattern,
}

impl PatternWrapper {
    fn new(s: &str) -> Result<PatternWrapper, PatternError> {
        let negate = s.starts_with('!');
        let s = s.trim_start_matches('!');
        let pattern = Pattern::new(s)?;
        Ok(PatternWrapper { negate, pattern })
    }

    fn matches(&self, s: &str) -> bool {
        self.pattern.matches(s) ^ self.negate
    }

    fn matches_path(&self, p: &Path) -> bool {
        self.pattern.matches_path(p) ^ self.negate
    }
}

impl<'de> Deserialize<'de> for PatternWrapper {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(PatternVisitor)
    }
}

struct PatternVisitor;

impl<'de> Visitor<'de> for PatternVisitor {
    type Value = PatternWrapper;

    fn expecting(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "a string")
    }

    fn visit_str<E: de::Error>(self, s: &str) -> Result<Self::Value, E> {
        PatternWrapper::new(s).map_err(de::Error::custom)
    }
}

impl Serialize for PatternWrapper {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.negate {
            serializer.serialize_str(&format!("!{}", self.pattern.as_str()))
        } else {
            serializer.serialize_str(self.pattern.as_str())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::future::Future;

    #[tokio::test]
    async fn filters_on_collectors() {
        let all_metrics = HostMetricsConfig::default()
            .capture_metrics()
            .await
            .collect::<Vec<_>>();

        for collector in &[
            Collector::Cpu,
            Collector::Disk,
            Collector::Filesystem,
            Collector::Load,
            Collector::Memory,
            Collector::Network,
        ] {
            let some_metrics = HostMetricsConfig {
                collectors: Some(vec![*collector]),
                ..Default::default()
            }
            .capture_metrics()
            .await
            .collect::<Vec<_>>();

            assert!(
                all_metrics.len() > some_metrics.len(),
                "collector={:?}",
                collector
            );
        }
    }

    #[tokio::test]
    async fn are_taged_with_hostname() {
        let metrics = HostMetricsConfig::default()
            .capture_metrics()
            .await
            .collect::<Vec<_>>();
        let hostname = crate::get_hostname().expect("Broken hostname");
        assert!(!metrics.into_iter().any(|event| event
            .into_metric()
            .tags
            .expect("Missing tags")
            .get("host")
            .expect("Missing \"host\" tag")
            != &hostname));
    }

    #[tokio::test]
    async fn uses_custom_namespace() {
        let metrics = HostMetricsConfig {
            namespace: Namespace("other".into()),
            ..Default::default()
        }
        .capture_metrics()
        .await
        .collect::<Vec<_>>();

        assert!(!metrics
            .into_iter()
            .any(|event| !event.into_metric().name.starts_with("other_")));
    }

    #[tokio::test]
    async fn generates_cpu_metrics() {
        let metrics = HostMetricsConfig::default().cpu_metrics().await;
        assert!(metrics.len() > 0);
        assert!(all_counters(&metrics));

        // They should all be named cpu_seconds_total
        assert_eq!(
            metrics.len(),
            count_name(&metrics, "host_cpu_seconds_total")
        );

        // They should all have a "mode" tag
        assert_eq!(count_tag(&metrics, "mode"), metrics.len());
    }

    #[tokio::test]
    async fn generates_disk_metrics() {
        let metrics = HostMetricsConfig::default().disk_metrics().await;
        assert!(metrics.len() > 0);
        assert!(metrics.len() % 4 == 0);
        assert!(all_counters(&metrics));

        // There are exactly four disk_* names
        for name in &[
            "host_disk_read_bytes_total",
            "host_disk_reads_completed_total",
            "host_disk_written_bytes_total",
            "host_disk_writes_completed_total",
        ] {
            assert_eq!(
                count_name(&metrics, name),
                metrics.len() / 4,
                "name={}",
                name
            );
        }

        // They should all have a "device" tag
        assert_eq!(count_tag(&metrics, "device"), metrics.len());
    }

    #[tokio::test]
    async fn filters_disk_metrics_on_device() {
        assert_filtered_metrics("device", |devices| async {
            HostMetricsConfig {
                disk: DiskConfig { devices },
                ..Default::default()
            }
            .disk_metrics()
            .await
        })
        .await;
    }

    #[tokio::test]
    async fn generates_filesystem_metrics() {
        let metrics = HostMetricsConfig::default().filesystem_metrics().await;
        assert!(metrics.len() > 0);
        assert!(metrics.len() % 3 == 0);
        assert!(all_gauges(&metrics));

        // There are exactly three filesystem_* names
        for name in &[
            "host_filesystem_free_bytes",
            "host_filesystem_total_bytes",
            "host_filesystem_used_bytes",
        ] {
            assert_eq!(
                count_name(&metrics, name),
                metrics.len() / 3,
                "name={}",
                name
            );
        }

        // They should all have "filesystem" and "mountpoint" tags
        assert_eq!(count_tag(&metrics, "filesystem"), metrics.len());
        assert_eq!(count_tag(&metrics, "mountpoint"), metrics.len());
    }

    #[tokio::test]
    async fn filesystem_metrics_filters_on_device() {
        assert_filtered_metrics("device", |devices| async {
            HostMetricsConfig {
                filesystem: FilesystemConfig {
                    devices,
                    ..Default::default()
                },
                ..Default::default()
            }
            .filesystem_metrics()
            .await
        })
        .await;
    }

    #[tokio::test]
    async fn filesystem_metrics_filters_on_filesystem() {
        assert_filtered_metrics("filesystem", |filesystems| async {
            HostMetricsConfig {
                filesystem: FilesystemConfig {
                    filesystems,
                    ..Default::default()
                },
                ..Default::default()
            }
            .filesystem_metrics()
            .await
        })
        .await;
    }

    #[tokio::test]
    async fn filesystem_metrics_filters_on_mountpoint() {
        assert_filtered_metrics("mountpoint", |mountpoints| async {
            HostMetricsConfig {
                filesystem: FilesystemConfig {
                    mountpoints,
                    ..Default::default()
                },
                ..Default::default()
            }
            .filesystem_metrics()
            .await
        })
        .await;
    }

    #[tokio::test]
    async fn generates_network_metrics() {
        let metrics = HostMetricsConfig::default().network_metrics().await;
        assert!(metrics.len() > 0);
        assert!(all_counters(&metrics));

        // All metrics are named network_*
        assert!(!metrics
            .iter()
            .any(|metric| !metric.name.starts_with("host_network_")));

        // They should all have a "device" tag
        assert_eq!(count_tag(&metrics, "device"), metrics.len());
    }

    #[tokio::test]
    async fn network_metrics_filters_on_device() {
        assert_filtered_metrics("device", |devices| async {
            HostMetricsConfig {
                network: NetworkConfig { devices },
                ..Default::default()
            }
            .network_metrics()
            .await
        })
        .await;
    }

    #[tokio::test]
    async fn generates_loadavg_metrics() {
        let metrics = HostMetricsConfig::default().loadavg_metrics().await;
        assert_eq!(metrics.len(), 3);
        assert!(all_gauges(&metrics));

        // All metrics are named load*
        assert!(!metrics
            .iter()
            .any(|metric| !metric.name.starts_with("host_load")));
    }

    fn all_counters(metrics: &[Metric]) -> bool {
        !metrics
            .iter()
            .any(|metric| !matches!(metric.value, MetricValue::Counter { .. }))
    }

    fn all_gauges(metrics: &[Metric]) -> bool {
        !metrics
            .iter()
            .any(|metric| !matches!(metric.value, MetricValue::Gauge { .. }))
    }

    fn all_tags_match(metrics: &[Metric], tag: &str, matches: impl Fn(&str) -> bool) -> bool {
        !metrics.iter().any(|metric| {
            metric
                .tags
                .as_ref()
                .unwrap()
                .get(tag)
                .map(|value| !matches(value))
                .unwrap_or(false)
        })
    }

    fn count_name(metrics: &[Metric], name: &str) -> usize {
        metrics.iter().filter(|metric| metric.name == name).count()
    }

    fn count_tag(metrics: &[Metric], tag: &str) -> usize {
        metrics
            .iter()
            .filter(|metric| {
                metric
                    .tags
                    .as_ref()
                    .expect("Metric is missing tags")
                    .contains_key(tag)
            })
            .count()
    }

    fn collect_tag_values(metrics: &[Metric], tag: &str) -> HashSet<String> {
        metrics
            .iter()
            .filter_map(|metric| metric.tags.as_ref().unwrap().get(tag).cloned())
            .collect::<HashSet<_>>()
    }

    // Run a series of tests using filters to ensure they are obeyed
    async fn assert_filtered_metrics<'a, Get, Fut>(tag: &str, get_metrics: Get)
    where
        Get: Fn(Option<Vec<PatternWrapper>>) -> Fut,
        Fut: Future<Output = Vec<Metric>>,
    {
        let all_metrics = get_metrics(None).await;
        let keys = collect_tag_values(&all_metrics, tag);
        // Pick an arbitrary key value
        let key = keys.into_iter().next().unwrap();
        let key_prefix = &key[..key.len() - 1];

        let filtered_metrics_with =
            get_metrics(Some(vec![PatternWrapper::new(&key).unwrap()])).await;

        assert!(filtered_metrics_with.len() < all_metrics.len());
        assert!(all_tags_match(&filtered_metrics_with, tag, |s| s == key));

        let filtered_metrics_with_match =
            get_metrics(Some(vec![
                PatternWrapper::new(&format!("{}*", key_prefix)).unwrap()
            ]))
            .await;

        assert!(filtered_metrics_with_match.len() >= filtered_metrics_with.len());
        assert!(all_tags_match(&filtered_metrics_with_match, tag, |s| {
            s.starts_with(key_prefix)
        }));

        let filtered_metrics_without =
            get_metrics(Some(vec![
                PatternWrapper::new(&format!("!{}", key)).unwrap()
            ]))
            .await;

        assert!(filtered_metrics_without.len() < all_metrics.len());
        assert!(all_tags_match(&filtered_metrics_without, tag, |s| s != key));

        let filtered_metrics_without_match = get_metrics(Some(vec![PatternWrapper::new(
            &format!("!{}*", key_prefix),
        )
        .unwrap()]))
        .await;

        assert!(filtered_metrics_without_match.len() <= filtered_metrics_without.len());
        assert!(all_tags_match(&filtered_metrics_without_match, tag, |s| {
            !s.starts_with(key_prefix)
        }));
    }
}
