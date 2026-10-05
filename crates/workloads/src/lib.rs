//! Synthetic workload phases. Durations are tens of seconds so a one-second
//! supervisory decision can matter. This crate does not generate application text.

#![forbid(unsafe_code)]

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhaseSpec {
    pub name: &'static str,
    pub duration_us: u64,
    pub latency_queue: u32,
    pub batch_queue: u32,
    pub maintenance_runnable: bool,
    pub managed_used_bytes: u64,
    pub evictable_backlog_bytes: u64,
    pub emergency: bool,
}

const MIXED_V1: &[PhaseSpec] = &[
    PhaseSpec {
        name: "burst",
        duration_us: 30_000_000,
        latency_queue: 8,
        batch_queue: 1,
        maintenance_runnable: false,
        managed_used_bytes: 8_388_608,
        evictable_backlog_bytes: 0,
        emergency: false,
    },
    PhaseSpec {
        name: "steady",
        duration_us: 30_000_000,
        latency_queue: 0,
        batch_queue: 8,
        maintenance_runnable: false,
        managed_used_bytes: 16_777_216,
        evictable_backlog_bytes: 0,
        emergency: false,
    },
    PhaseSpec {
        name: "idle",
        duration_us: 20_000_000,
        latency_queue: 0,
        batch_queue: 0,
        maintenance_runnable: false,
        managed_used_bytes: 1_048_576,
        evictable_backlog_bytes: 0,
        emergency: false,
    },
    PhaseSpec {
        name: "memory",
        duration_us: 30_000_000,
        latency_queue: 1,
        batch_queue: 1,
        maintenance_runnable: true,
        managed_used_bytes: 241_591_910,
        evictable_backlog_bytes: 67_108_864,
        emergency: true,
    },
    PhaseSpec {
        name: "recovery",
        duration_us: 20_000_000,
        latency_queue: 1,
        batch_queue: 1,
        maintenance_runnable: true,
        managed_used_bytes: 8_388_608,
        evictable_backlog_bytes: 0,
        emergency: false,
    },
];

pub fn mixed_v1() -> &'static [PhaseSpec] {
    MIXED_V1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_last_tens_of_seconds_and_match_the_config() {
        let file = include_str!("../../../configs/workloads/mixed-v1.json");
        let parsed: serde_json::Value = serde_json::from_str(file).unwrap();
        let phases = parsed["phases"].as_array().unwrap();
        assert_eq!(phases.len(), mixed_v1().len());
        for (spec, json) in mixed_v1().iter().zip(phases) {
            assert!(spec.duration_us >= 10_000_000);
            assert_eq!(json["name"], spec.name);
            assert_eq!(json["duration_us"], spec.duration_us);
            assert_eq!(json["latency_queue"], spec.latency_queue);
            assert_eq!(json["batch_queue"], spec.batch_queue);
            assert_eq!(json["emergency"], spec.emergency);
        }
    }
}
