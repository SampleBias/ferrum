//! `jobs-v1` and `jobs-v2`: open-loop short jobs for branched labels.
//!
//! The scheduler splits CPU by the profile weights, so a group's service share
//! follows from the profile that was forced and cannot label it. This family
//! measures what the share does to work instead. Latency jobs arrive on a seeded
//! schedule whatever the guest's speed, each job is a fixed amount of solo CPU,
//! and its latency runs from the scheduled arrival to completion. Batch workers
//! stay runnable and count fixed units.
//!
//! Arrivals are Bernoulli draws on a [`TICK_US`] grid from SplitMix64, using
//! integers only, so the host and the guest build the same schedule bit for bit.
//!
//! The families share every timing constant and differ in their scenario
//! tables. `jobs-v1` put an overload 5 points over an even split, where host
//! speed noise reached the pre-decision backlog. `jobs-v2` puts overload at
//! 65% and above.

use policy_types::Observation;

use crate::{BATCH_WORKERS, LATENCY_WORKERS};

/// Balanced runs this long before the decision point.
pub const PREFIX_US: u64 = 3_000_000;
/// The pre-decision window ends at the decision point.
pub const STATE_WINDOW_US: u64 = 1_000_000;
/// The forced profile is measured over this horizon.
pub const HORIZON_US: u64 = 5_000_000;
/// Jobs that arrived inside the horizon may still finish during this tail.
/// Arrivals continue, and anything unfinished at its end is incomplete.
pub const DRAIN_US: u64 = 1_000_000;
pub const SCHEDULE_US: u64 = PREFIX_US + HORIZON_US + DRAIN_US;
pub const TICK_US: u64 = 100;
/// Solo CPU time of one batch unit.
pub const BATCH_UNIT_US: u64 = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobScenario {
    pub name: &'static str,
    /// Solo CPU time of one latency job.
    pub job_us: u64,
    pub rate_per_s: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobFamily {
    pub id: &'static str,
    pub scenarios: &'static [JobScenario],
}

impl JobFamily {
    pub fn scenario(self, name: &str) -> Option<JobScenario> {
        self.scenarios.iter().copied().find(|scenario| scenario.name == name)
    }
}

const fn job(name: &'static str, job_us: u64, rate_per_s: u32) -> JobScenario {
    JobScenario { name, job_us, rate_per_s }
}

pub const JOBS_V1: JobFamily = JobFamily {
    id: "jobs-v1",
    scenarios: &[
        job("s4-u15", 4_000, 38),
        job("s4-u35", 4_000, 88),
        job("s4-u45", 4_000, 112),
        job("s4-u55", 4_000, 138),
        job("s4-u75", 4_000, 188),
        job("s12-u15", 12_000, 13),
        job("s12-u35", 12_000, 29),
        job("s12-u45", 12_000, 37),
        job("s12-u55", 12_000, 46),
        job("s32-u35", 32_000, 11),
    ],
};

pub const JOBS_V2: JobFamily = JobFamily {
    id: "jobs-v2",
    scenarios: &[
        job("s4-u15", 4_000, 38),
        job("s4-u35", 4_000, 88),
        job("s4-u45", 4_000, 112),
        job("s4-u65", 4_000, 162),
        job("s4-u72", 4_000, 180),
        job("s4-u80", 4_000, 200),
        job("s4-u95", 4_000, 238),
        job("s12-u15", 12_000, 13),
        job("s12-u35", 12_000, 29),
        job("s12-u45", 12_000, 37),
        job("s12-u65", 12_000, 54),
        job("s12-u72", 12_000, 60),
        job("s12-u80", 12_000, 67),
        job("s32-u35", 32_000, 11),
        job("s32-u65", 32_000, 20),
    ],
};

pub const FAMILIES: [JobFamily; 2] = [JOBS_V1, JOBS_V2];

pub fn family(id: &str) -> Option<JobFamily> {
    FAMILIES.iter().copied().find(|family| family.id == id)
}

impl JobScenario {
    /// Offered latency CPU as basis points of one CPU.
    pub const fn offered_bp(self) -> u64 {
        self.job_us * self.rate_per_s as u64 / 100
    }

    /// Arrival offsets in microseconds from the trial start, over [`SCHEDULE_US`].
    pub fn schedule(self, seed: u64) -> Vec<u64> {
        let mut rng = SplitMix64(seed ^ fnv1a64(self.name.as_bytes()));
        let threshold = (u128::from(self.rate_per_s) * u128::from(TICK_US) << 64) / 1_000_000;
        let mut arrivals = Vec::new();
        for tick in 0..SCHEDULE_US / TICK_US {
            if u128::from(rng.next()) < threshold {
                arrivals.push(tick * TICK_US);
            }
        }
        arrivals
    }
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// FNV-1a over the little-endian arrival offsets. Branches of one unit must match.
pub fn schedule_digest(arrivals: &[u64]) -> u64 {
    let mut bytes = Vec::with_capacity(arrivals.len() * 8);
    for arrival in arrivals {
        bytes.extend_from_slice(&arrival.to_le_bytes());
    }
    fnv1a64(&bytes)
}

/// Latency-class state at the decision point, from arrivals and completion
/// offsets read at that instant. A completion of zero means not finished.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PrefixJobs {
    pub pending: u32,
    pub oldest_pending_us: u64,
    pub completed_in_window: u32,
    pub max_latency_in_window_us: u64,
}

pub fn prefix_jobs(arrivals: &[u64], completions: &[u64], decision_us: u64) -> PrefixJobs {
    let window_start = decision_us.saturating_sub(STATE_WINDOW_US);
    let mut state = PrefixJobs::default();
    for (arrival, done) in arrivals.iter().zip(completions) {
        if *arrival > decision_us {
            break;
        }
        if *done == 0 || *done > decision_us {
            state.pending += 1;
            state.oldest_pending_us = state.oldest_pending_us.max(decision_us - arrival);
        } else if *done > window_start {
            state.completed_in_window += 1;
            state.max_latency_in_window_us = state.max_latency_in_window_us.max(done - arrival);
        }
    }
    state
}

/// The pre-decision snapshot. `service_us` is latency, batch, maintenance, then
/// system over the [`STATE_WINDOW_US`] that ends at the decision point.
pub fn pre_decision(jobs: PrefixJobs, service_us: [u64; 4], batch_units: u64) -> Observation {
    let mut obs = Observation::quiet(STATE_WINDOW_US);
    let latency = &mut obs.groups[0];
    latency.queue_len = jobs.pending;
    latency.runnable = jobs.pending.min(u32::from(LATENCY_WORKERS));
    latency.cpu_service_us = service_us[0];
    latency.wait_samples = jobs.completed_in_window;
    latency.max_wait_us = jobs.oldest_pending_us.max(jobs.max_latency_in_window_us);
    latency.completions = u64::from(jobs.completed_in_window);
    let batch = &mut obs.groups[1];
    batch.queue_len = u32::from(BATCH_WORKERS);
    batch.runnable = u32::from(BATCH_WORKERS);
    batch.cpu_service_us = service_us[1];
    batch.completions = batch_units;
    obs.groups[2].cpu_service_us = service_us[2];
    obs.groups[3].cpu_service_us = service_us[3];
    obs
}

/// Latency outcome for jobs that arrived in `[from_us, until_us)`. A job that
/// has not finished by `end_us` is incomplete and ranks above every finished
/// job. With `end_us = until_us + DRAIN_US` it has waited at least the drain.
/// A percentile whose rank lands on an incomplete job is censored (`None`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobOutcome {
    pub offered: u32,
    pub completed: u32,
    pub completion_bp: u32,
    pub p50_us: Option<u64>,
    pub p90_us: Option<u64>,
    pub p95_us: Option<u64>,
    pub p99_us: Option<u64>,
}

pub fn outcome(
    arrivals: &[u64],
    completions: &[u64],
    from_us: u64,
    until_us: u64,
    end_us: u64,
) -> JobOutcome {
    let mut latencies = Vec::new();
    let mut offered = 0u32;
    for (arrival, done) in arrivals.iter().zip(completions) {
        if *arrival < from_us || *arrival >= until_us {
            continue;
        }
        offered += 1;
        if *done != 0 && *done <= end_us {
            latencies.push(done - arrival);
        }
    }
    latencies.sort_unstable();
    let completed = latencies.len() as u32;
    let completion_bp = if offered == 0 {
        10_000
    } else {
        (u64::from(completed) * 10_000 / u64::from(offered)) as u32
    };
    let rank = |percent: u64| -> Option<u64> {
        if offered == 0 {
            return None;
        }
        let index = (u64::from(offered) * percent).div_ceil(100).max(1) - 1;
        latencies.get(index as usize).copied()
    };
    JobOutcome {
        offered,
        completed,
        completion_bp,
        p50_us: rank(50),
        p90_us: rank(90),
        p95_us: rank(95),
        p99_us: rank(99),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenarios_match_the_config() {
        let files = [
            (JOBS_V1, include_str!("../../../configs/workloads/jobs-v1.json")),
            (JOBS_V2, include_str!("../../../configs/workloads/jobs-v2.json")),
        ];
        for (family, file) in files {
            let parsed: serde_json::Value = serde_json::from_str(file).unwrap();
            assert_eq!(parsed["id"], family.id);
            assert_eq!(parsed["prefix_us"], PREFIX_US);
            assert_eq!(parsed["state_window_us"], STATE_WINDOW_US);
            assert_eq!(parsed["horizon_us"], HORIZON_US);
            assert_eq!(parsed["drain_us"], DRAIN_US);
            assert_eq!(parsed["tick_us"], TICK_US);
            assert_eq!(parsed["batch_unit_us"], BATCH_UNIT_US);
            assert_eq!(parsed["latency_workers"], LATENCY_WORKERS);
            assert_eq!(parsed["batch_workers"], BATCH_WORKERS);
            assert_eq!(parsed["maintenance_workers"], 0);
            let scenarios = parsed["scenarios"].as_array().unwrap();
            assert_eq!(scenarios.len(), family.scenarios.len());
            for (spec, json) in family.scenarios.iter().zip(scenarios) {
                assert_eq!(json["name"], spec.name);
                assert_eq!(json["job_us"], spec.job_us);
                assert_eq!(json["rate_per_s"], spec.rate_per_s);
                assert_eq!(json["offered_bp"], spec.offered_bp());
            }
        }
        assert_eq!(family("jobs-v2"), Some(JOBS_V2));
        assert_eq!(family("jobs-v3"), None);
    }

    #[test]
    fn a_shared_name_is_the_same_scenario_in_every_family() {
        for scenario in JOBS_V1.scenarios {
            if let Some(other) = JOBS_V2.scenario(scenario.name) {
                assert_eq!(other, *scenario);
            }
        }
    }

    #[test]
    fn a_schedule_is_fixed_by_scenario_and_seed() {
        let scenario = JOBS_V1.scenario("s4-u35").unwrap();
        let first = scenario.schedule(101);
        assert_eq!(first, scenario.schedule(101));
        assert_ne!(schedule_digest(&first), schedule_digest(&scenario.schedule(102)));
        let other = JOBS_V1.scenario("s12-u35").unwrap().schedule(101);
        assert_ne!(schedule_digest(&first), schedule_digest(&other));
        assert_eq!(schedule_digest(&first), PINNED_S4_U35_101);
        assert!(first.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(first.iter().all(|arrival| arrival % TICK_US == 0 && *arrival < SCHEDULE_US));
    }

    /// Also pinned in `controller/tests/test_jobs.py`, so both builders agree.
    const PINNED_S4_U35_101: u64 = 0xd58e_8e4a_f254_388f;

    #[test]
    fn arrival_counts_follow_the_rate() {
        for scenario in FAMILIES.iter().flat_map(|family| family.scenarios) {
            let expected = u64::from(scenario.rate_per_s) * SCHEDULE_US / 1_000_000;
            let mut total = 0u64;
            for seed in 0..8 {
                total += scenario.schedule(seed).len() as u64;
            }
            let mean = total / 8;
            assert!(
                mean * 10 >= expected * 9 && mean * 10 <= expected * 11,
                "{} mean {mean} expected {expected}",
                scenario.name
            );
        }
    }

    #[test]
    fn offered_load_spans_both_sides_of_an_even_split() {
        for family in FAMILIES {
            let bp: Vec<u64> = family.scenarios.iter().map(|scenario| scenario.offered_bp()).collect();
            assert!(bp.iter().any(|value| *value < 2_000));
            assert!(bp.iter().any(|value| *value > 5_000));
            assert!(bp.iter().all(|value| *value < 10_000));
        }
    }

    #[test]
    fn jobs_v2_keeps_overload_clear_of_the_even_split() {
        for scenario in JOBS_V2.scenarios {
            let bp = scenario.offered_bp();
            assert!(bp <= 4_500 || bp >= 6_400, "{} offers {bp} bp", scenario.name);
        }
    }

    #[test]
    fn outcome_ranks_from_scheduled_arrival_and_censors_unfinished_jobs() {
        let arrivals = [0, 10, 20, 30, 40];
        let completions = [5, 30, 50, 0, 100];
        let all = outcome(&arrivals, &completions, 0, 50, 200);
        assert_eq!(all.offered, 5);
        assert_eq!(all.completed, 4);
        assert_eq!(all.completion_bp, 8_000);
        assert_eq!(all.p50_us, Some(30));
        assert_eq!(all.p90_us, None);
        assert_eq!(all.p99_us, None);

        let window = outcome(&arrivals, &completions, 10, 30, 200);
        assert_eq!(window.offered, 2);
        assert_eq!(window.completion_bp, 10_000);
        assert_eq!(window.p50_us, Some(20));
        assert_eq!(window.p99_us, Some(30));

        let early_end = outcome(&arrivals, &completions, 0, 50, 60);
        assert_eq!(early_end.completed, 3);

        let empty = outcome(&arrivals, &completions, 60, 70, 200);
        assert_eq!(empty.offered, 0);
        assert_eq!(empty.completion_bp, 10_000);
        assert_eq!(empty.p99_us, None);
    }

    #[test]
    fn prefix_state_uses_only_what_had_happened_by_the_decision() {
        let arrivals = [100, 200, 1_500_000, 2_500_000, 2_900_000, 3_100_000];
        let completions = [900, 3_050_000, 1_520_000, 2_540_000, 0, 3_200_000];
        let state = prefix_jobs(&arrivals, &completions, PREFIX_US);
        assert_eq!(state.pending, 2);
        assert_eq!(state.oldest_pending_us, PREFIX_US - 200);
        assert_eq!(state.completed_in_window, 1);
        assert_eq!(state.max_latency_in_window_us, 40_000);

        let obs = pre_decision(state, [400_000, 550_000, 0, 9_000], 1_100);
        assert_eq!(obs.window_us, STATE_WINDOW_US);
        assert_eq!(obs.groups[0].queue_len, 2);
        assert_eq!(obs.groups[0].runnable, 2);
        assert_eq!(obs.groups[0].max_wait_us, PREFIX_US - 200);
        assert_eq!(obs.groups[0].wait_samples, 1);
        assert_eq!(obs.groups[1].queue_len, u32::from(BATCH_WORKERS));
        assert_eq!(obs.groups[1].completions, 1_100);
        assert_eq!(obs.groups[2].runnable, 0);
        assert_eq!(obs.groups[3].cpu_service_us, 9_000);
    }
}
