//! Synthetic workload phases. Durations are tens of seconds so a one-second
//! supervisory decision can matter. This crate does not generate application text.
//!
//! The guest keeps a fixed pool of eight workload threads: three latency, three
//! batch, and two maintenance. A phase's queue depth is offered work. The duty
//! is how many of those threads actually run. Group service still follows the
//! profile weights, not the thread count inside a group.

#![forbid(unsafe_code)]

use policy_types::{Observation, MANAGED_ARENA_BYTES};

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

pub const LATENCY_WORKERS: u8 = 3;
pub const BATCH_WORKERS: u8 = 3;
pub const MAINTENANCE_WORKERS: u8 = 2;
/// Service window the guest measures after a phase's runnable set is in force.
pub const SAMPLE_US: u64 = 400_000;

const QUIET_US: u64 = 25_000;
const SOLO_US: u64 = 250_000;
const SHARED_US: u64 = 40_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhaseDuty {
    pub latency: u8,
    pub batch: u8,
    pub maintenance: u8,
}

impl PhaseDuty {
    pub const fn threads(self) -> u32 {
        self.latency as u32 + self.batch as u32 + self.maintenance as u32
    }

    const fn slots(self) -> [u8; 3] {
        [self.latency, self.batch, self.maintenance]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceFault {
    QuietRan { class: u8 },
    ActiveStarved { class: u8 },
    WeightSkew { left: u8, right: u8 },
}

impl core::fmt::Display for ServiceFault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let name = |class: u8| match class {
            0 => "latency",
            1 => "batch",
            _ => "maintenance",
        };
        match *self {
            Self::QuietRan { class } => {
                write!(f, "{} ran while the phase parked it", name(class))
            }
            Self::ActiveStarved { class } => {
                write!(f, "{} was runnable and received almost no service", name(class))
            }
            Self::WeightSkew { left, right } => {
                write!(f, "{} and {} did not follow their weights", name(left), name(right))
            }
        }
    }
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

impl PhaseSpec {
    pub const fn duty(self) -> PhaseDuty {
        PhaseDuty {
            latency: clamp_queue(self.latency_queue, LATENCY_WORKERS),
            batch: clamp_queue(self.batch_queue, BATCH_WORKERS),
            maintenance: if self.maintenance_runnable {
                MAINTENANCE_WORKERS
            } else {
                0
            },
        }
    }
}

const fn clamp_queue(queue: u32, workers: u8) -> u8 {
    if queue == 0 {
        0
    } else if queue >= workers as u32 {
        workers
    } else {
        queue as u8
    }
}

/// Counters the heuristic sees for this phase. Queue length stays the offered
/// depth. `runnable` is the duty, which cannot exceed the eight-thread pool.
pub fn observation(phase: PhaseSpec) -> Observation {
    let duty = phase.duty();
    let mut obs = Observation::quiet(phase.duration_us);
    obs.groups[0].queue_len = phase.latency_queue;
    obs.groups[0].runnable = u32::from(duty.latency);
    obs.groups[1].queue_len = phase.batch_queue;
    obs.groups[1].runnable = u32::from(duty.batch);
    obs.groups[2].queue_len = u32::from(phase.maintenance_runnable);
    obs.groups[2].runnable = u32::from(duty.maintenance);
    obs.pressure.managed_cap_bytes = MANAGED_ARENA_BYTES;
    obs.pressure.managed_used_bytes = phase.managed_used_bytes;
    obs.pressure.evictable_backlog_bytes = phase.evictable_backlog_bytes;
    obs.pressure.emergency = phase.emergency;
    obs
}

/// Wall time that lets the memory phase absorb the virtual-runtime lead from
/// `steady` before its sample starts.
///
/// `steady` runs batch alone for [`SAMPLE_US`] at weight 6, adding
/// `SAMPLE_US / 6` of virtual runtime. Under reclaim weights, latency and
/// maintenance climb together at one eighth of a virtual microsecond per wall
/// microsecond, so that lead lasts `SAMPLE_US * 8 / 6`. The wait is twice that.
pub fn memory_catch_up_us() -> u64 {
    SAMPLE_US.saturating_mul(8) / 6 * 2
}

/// Ideal group service for one [`SAMPLE_US`] window once virtual runtimes have
/// met. A parked group gets nothing. Runnable groups split the window by weight.
pub fn ideal_service(duty: PhaseDuty, weights: [u32; 3]) -> [u64; 3] {
    let slots = duty.slots();
    let mut share = [0u64; 3];
    let mut sum = 0u64;
    for i in 0..3 {
        if slots[i] > 0 {
            share[i] = u64::from(weights[i]);
            sum += share[i];
        }
    }
    if sum == 0 {
        return [0, 0, 0];
    }
    let mut out = [0u64; 3];
    for i in 0..3 {
        out[i] = SAMPLE_US.saturating_mul(share[i]) / sum;
    }
    out
}

/// Check a [`SAMPLE_US`] window. Parked groups stay near zero. A group that is
/// alone takes the window. Runnable groups stay within a factor of two of the
/// weight ratio.
pub fn service_follows(
    sample: [u64; 3],
    duty: PhaseDuty,
    weights: [u32; 3],
) -> Result<(), ServiceFault> {
    let slots = duty.slots();
    let active = slots.iter().filter(|slot| **slot > 0).count();
    for i in 0..3 {
        if slots[i] == 0 {
            if sample[i] > QUIET_US {
                return Err(ServiceFault::QuietRan { class: i as u8 });
            }
            continue;
        }
        let floor = if active == 1 { SOLO_US } else { SHARED_US };
        if sample[i] < floor {
            return Err(ServiceFault::ActiveStarved { class: i as u8 });
        }
        if weights[i] == 0 {
            return Err(ServiceFault::WeightSkew {
                left: i as u8,
                right: i as u8,
            });
        }
    }
    for i in 0..3 {
        if slots[i] == 0 {
            continue;
        }
        for j in (i + 1)..3 {
            if slots[j] == 0 {
                continue;
            }
            let left = sample[i].saturating_mul(u64::from(weights[j]));
            let right = sample[j].saturating_mul(u64::from(weights[i]));
            if left > right.saturating_mul(2) || right > left.saturating_mul(2) {
                return Err(ServiceFault::WeightSkew {
                    left: i as u8,
                    right: j as u8,
                });
            }
        }
    }
    Ok(())
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
            assert_eq!(json["maintenance_runnable"], spec.maintenance_runnable);
            assert_eq!(json["managed_used_bytes"], spec.managed_used_bytes);
            assert_eq!(json["evictable_backlog_bytes"], spec.evictable_backlog_bytes);
            assert_eq!(json["emergency"], spec.emergency);
        }
    }

    #[test]
    fn duties_follow_the_queues_and_fit_eight_threads() {
        let duties: Vec<PhaseDuty> = mixed_v1().iter().map(|phase| phase.duty()).collect();
        assert_eq!(
            duties,
            vec![
                PhaseDuty { latency: 3, batch: 1, maintenance: 0 },
                PhaseDuty { latency: 0, batch: 3, maintenance: 0 },
                PhaseDuty { latency: 0, batch: 0, maintenance: 0 },
                PhaseDuty { latency: 1, batch: 1, maintenance: 2 },
                PhaseDuty { latency: 1, batch: 1, maintenance: 2 },
            ]
        );
        for duty in duties {
            assert!(duty.threads() <= 8);
        }
    }

    #[test]
    fn ideal_windows_match_the_duty_and_a_skewed_window_does_not() {
        let balanced = [1, 1, 1];
        let latency = [6, 2, 2];
        let burst = mixed_v1()[0].duty();
        let ideal = ideal_service(burst, latency);
        assert!(ideal[0] > ideal[1]);
        assert_eq!(ideal[2], 0);
        service_follows(ideal, burst, latency).unwrap();

        let reversed = [ideal[1], ideal[0], 0];
        assert!(service_follows(reversed, burst, latency).is_err());

        let idle = mixed_v1()[2].duty();
        assert_eq!(idle.threads(), 0);
        service_follows([0, 0, 0], idle, balanced).unwrap();
        assert!(service_follows([QUIET_US + 1, 0, 0], idle, balanced).is_err());

        let steady = mixed_v1()[1].duty();
        service_follows(ideal_service(steady, [2, 6, 2]), steady, [2, 6, 2]).unwrap();
        service_follows(ideal_service(idle, balanced), idle, balanced).unwrap();
    }

    #[test]
    fn memory_catch_up_covers_one_solo_sample() {
        let wait = memory_catch_up_us();
        assert_eq!(wait, SAMPLE_US * 8 / 6 * 2);
        assert!(wait > SAMPLE_US);
    }
}
