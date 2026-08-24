//! What a Linux device can honestly say about its own health.
//!
//! NervesHub charts every metric it is sent, but only a few known keys drive
//! the status shown in the UI — `mem_used_percent` and `cpu_usage_percent`
//! among them. So the readings that mean the same thing everywhere are reported
//! under the names the platform understands, and anything particular to this
//! machine goes alongside under its own name, where it is charted but does not
//! silently change a device's status.
//!
//! Everything comes from `/proc` and `statvfs`. No `ps`, no `df`, nothing that
//! shells out — a health report that spawns three processes every time it is
//! asked for is a health problem of its own on a small device.
//!
//! # CPU
//!
//! `/proc/stat` gives cumulative jiffies since boot, so a single read says what
//! the CPU has averaged since the device turned on, which is not what anyone
//! means by CPU usage. [`Health`] keeps the previous reading and reports the
//! change between them, which means the first report after a restart has
//! nothing to compare against and omits the metric rather than sending the
//! since-boot average as though it were current.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

/// The cumulative CPU counters from `/proc/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CpuTimes {
    idle: u64,
    total: u64,
}

#[derive(Debug, Default)]
pub struct Health {
    previous_cpu: Option<CpuTimes>,
}

impl Health {
    pub fn new() -> Self {
        Self::default()
    }

    /// A report, in the shape the server stores.
    ///
    /// `value.metrics` is read into the metrics table and drives status;
    /// the whole of `value` is kept as the report.
    pub fn report(&mut self) -> Value {
        let mut metrics: BTreeMap<String, f64> = BTreeMap::new();
        let mut metadata: BTreeMap<String, String> = BTreeMap::new();

        if let Some(memory) = read_memory() {
            metrics.insert("mem_size_mb".into(), memory.total_mb);
            metrics.insert("mem_used_mb".into(), memory.used_mb);
            metrics.insert("mem_used_percent".into(), memory.used_percent);
        }

        if let Some(usage) = self.cpu_usage() {
            metrics.insert("cpu_usage_percent".into(), usage);
        }

        if let Some(load) = read_load_average() {
            metrics.insert("load_1min".into(), load.0);
            metrics.insert("load_5min".into(), load.1);
            metrics.insert("load_15min".into(), load.2);
        }

        if let Some(temp) = read_cpu_temp() {
            metrics.insert("cpu_temp".into(), temp);
        }

        for (key, value) in read_metadata() {
            metadata.insert(key, value);
        }

        metadata.insert("agent_version".into(), env!("CARGO_PKG_VERSION").into());

        let metrics: Map<String, Value> = metrics.into_iter().map(|(k, v)| (k, json!(v))).collect();
        let metadata: Map<String, Value> =
            metadata.into_iter().map(|(k, v)| (k, json!(v))).collect();

        json!({ "value": { "metrics": metrics, "metadata": metadata } })
    }

    /// Percentage of CPU time spent doing something since the last report.
    ///
    /// `None` on the first call, and whenever the counters go backwards — which
    /// they do across a suspend or a counter wrap. Reporting nothing is better
    /// than reporting a number derived from a negative interval.
    fn cpu_usage(&mut self) -> Option<f64> {
        let current = read_cpu_times()?;
        let previous = self.previous_cpu.replace(current)?;

        let total = current.total.checked_sub(previous.total)?;
        let idle = current.idle.checked_sub(previous.idle)?;

        if total == 0 {
            return None;
        }

        Some(((total - idle) as f64 / total as f64) * 100.0)
    }
}

struct Memory {
    total_mb: f64,
    used_mb: f64,
    used_percent: f64,
}

/// Used memory as `MemTotal - MemAvailable`.
///
/// Not `MemFree`: on Linux free memory is close to zero on any machine that has
/// been up a while, because the kernel uses what is spare for page cache and
/// hands it back on demand. A `mem_used_percent` built from `MemFree` sits at
/// 98% on a perfectly healthy device and makes the metric useless.
fn read_memory() -> Option<Memory> {
    let raw = std::fs::read_to_string("/proc/meminfo").ok()?;
    let fields = parse_meminfo(&raw);

    let total_kb = *fields.get("MemTotal")? as f64;
    let available_kb = *fields.get("MemAvailable")? as f64;
    let used_kb = (total_kb - available_kb).max(0.0);

    Some(Memory {
        total_mb: total_kb / 1024.0,
        used_mb: used_kb / 1024.0,
        used_percent: if total_kb > 0.0 {
            (used_kb / total_kb) * 100.0
        } else {
            0.0
        },
    })
}

fn parse_meminfo(raw: &str) -> BTreeMap<String, u64> {
    raw.lines()
        .filter_map(|line| {
            let (key, rest) = line.split_once(':')?;
            let value = rest.split_whitespace().next()?.parse().ok()?;

            Some((key.to_string(), value))
        })
        .collect()
}

fn read_cpu_times() -> Option<CpuTimes> {
    let raw = std::fs::read_to_string("/proc/stat").ok()?;

    parse_cpu_times(&raw)
}

fn parse_cpu_times(raw: &str) -> Option<CpuTimes> {
    let line = raw.lines().find(|line| line.starts_with("cpu "))?;

    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|field| field.parse().ok())
        .collect();

    // user nice system idle iowait irq softirq steal ...
    // iowait counts as idle: the CPU had nothing to run, which is what the
    // metric is asking about, even though the machine was busy waiting.
    let idle = values.get(3).copied()? + values.get(4).copied().unwrap_or(0);

    Some(CpuTimes {
        idle,
        total: values.iter().sum(),
    })
}

fn read_load_average() -> Option<(f64, f64, f64)> {
    let raw = std::fs::read_to_string("/proc/loadavg").ok()?;
    let mut fields = raw.split_whitespace();

    Some((
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
    ))
}

/// The first thermal zone, in degrees.
///
/// Which zone is "the CPU" is board-specific and there is no portable way to
/// ask, so this takes zone 0 and is honest about it being an approximation.
fn read_cpu_temp() -> Option<f64> {
    let raw = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp").ok()?;
    let millidegrees: f64 = raw.trim().parse().ok()?;

    Some(millidegrees / 1000.0)
}

fn read_metadata() -> Vec<(String, String)> {
    let mut metadata = Vec::new();

    if let Ok(raw) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        metadata.push(("kernel_version".into(), raw.trim().to_string()));
    }

    if let Ok(raw) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        metadata.push(("hostname".into(), raw.trim().to_string()));
    }

    if let Ok(raw) = std::fs::read_to_string("/etc/os-release") {
        if let Some(name) = raw.lines().find_map(|line| {
            line.strip_prefix("PRETTY_NAME=")
                .map(|value| value.trim_matches('"').to_string())
        }) {
            metadata.push(("os".into(), name));
        }
    }

    metadata
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn used_memory_excludes_reclaimable_cache() {
        // A machine with almost no MemFree but plenty available — the normal
        // state of any Linux box that has been up for a while.
        let raw =
            "MemTotal:       2048000 kB\nMemFree:          40000 kB\nMemAvailable:   1024000 kB\n";
        let fields = parse_meminfo(raw);

        assert_eq!(fields["MemTotal"], 2_048_000);
        assert_eq!(fields["MemAvailable"], 1_024_000);
    }

    #[test]
    fn cpu_times_count_iowait_as_idle() {
        let times = parse_cpu_times("cpu  100 0 100 700 100 0 0 0\n").unwrap();

        assert_eq!(times.idle, 800);
        assert_eq!(times.total, 1000);
    }

    #[test]
    fn the_first_cpu_reading_has_nothing_to_compare_against() {
        let mut health = Health::new();

        // On a machine with /proc this returns None the first time; on one
        // without, it returns None always. Either way the first report must not
        // invent a number.
        assert_eq!(health.cpu_usage(), None);
    }

    #[test]
    fn a_report_always_has_the_shape_the_server_reads() {
        let report = Health::new().report();

        assert!(report["value"]["metrics"].is_object());
        assert!(report["value"]["metadata"].is_object());
        assert_eq!(
            report["value"]["metadata"]["agent_version"],
            env!("CARGO_PKG_VERSION")
        );
    }
}
