//! Distilled `no_std` multi-regime joint policy (`DISTILLED_JOINT_V1`).
//!
//! Combines the multi-regime `features-v1` boundary cuts (`pending < 15` vs
//! `>= 15` overload shedding) with full 4-profile joint actuation (`balanced`,
//! `latency`, `throughput`, `reclaim`) across CPU weights, cache soft target,
//! and per-class admission limits.
//!
//! Executes in `< 5 µs` using deterministic integer/basis-point arithmetic with
//! calibrated confidence output and selective abstention support.

use policy_types::{GroupId, Observation, ProfileId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DistilledRegime {
    /// Low ambient load; coalesce timer wakeups and hold balanced 1:1:1 baseline.
    IdleCoalesced,
    /// Under-capacity burst (`pending < cut`); boost latency class (`6:2:2`).
    UnderCapacityBurst,
    /// Sustained batch backlog with latency slack; boost batch class (`2:6:2`, 96 MiB cache).
    SustainedBatch,
    /// Memory pressure, cache backlog, or latency queue overload (`pending >= cut`); shed/reclaim (`2:2:6`).
    OverloadOrReclaim,
}

impl DistilledRegime {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IdleCoalesced => "idle_coalesced",
            Self::UnderCapacityBurst => "under_capacity_burst",
            Self::SustainedBatch => "sustained_batch",
            Self::OverloadOrReclaim => "overload_or_reclaim",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DistilledJointModel {
    /// Twice the overload queue cut (`29` = `14.5` pending jobs).
    pub overload_cut_times_two: u32,
    /// Minimum latency queue depth to trigger latency boost without wait samples.
    pub latency_queue_elevated: u32,
    /// Minimum latency wait (microseconds) to trigger latency boost under capacity.
    pub latency_wait_elevated_us: u64,
    /// Minimum batch queue depth to trigger batch throughput (`throughput`).
    pub batch_queue_high: u32,
    /// Managed arena usage ratio (basis points) that triggers reclamation.
    pub memory_pressure_bp: u16,
    /// Evictable cache backlog (bytes) that triggers reclamation.
    pub evictable_backlog_bytes: u64,
    /// Minimum unreserved headroom (bytes) before reclamation is preferred.
    pub min_headroom_bytes: u64,
    /// Minimum calibrated confidence (basis points) required before proposing;
    /// below this threshold a live controller abstains with `low_confidence`.
    pub min_confidence_bp: u16,
}

pub const DISTILLED_JOINT_V1: DistilledJointModel = DistilledJointModel {
    overload_cut_times_two: 29,
    latency_queue_elevated: 4,
    latency_wait_elevated_us: 5_000,
    batch_queue_high: 4,
    memory_pressure_bp: 8_000,
    evictable_backlog_bytes: 33_554_432,
    min_headroom_bytes: 67_108_864,
    min_confidence_bp: 6_500,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DistilledDecision {
    pub profile: ProfileId,
    pub regime: DistilledRegime,
    pub confidence_bp: u16,
    pub abstain_low_confidence: bool,
    pub margin_jobs_times_two: i32,
}

pub fn evaluate_distilled(obs: &Observation, model: &DistilledJointModel) -> DistilledDecision {
    let latency = group(obs, GroupId::Latency);
    let batch = group(obs, GroupId::Batch);
    let maintenance = group(obs, GroupId::Maintenance);

    let pressure_bp = if obs.pressure.managed_cap_bytes == 0 {
        0
    } else {
        obs.pressure
            .managed_used_bytes
            .saturating_mul(10_000)
            / obs.pressure.managed_cap_bytes
    };
    let headroom_low = obs.pressure.managed_cap_bytes > 0
        && obs.pressure.headroom_bytes < model.min_headroom_bytes;
    let memory_hot = obs.pressure.emergency
        || pressure_bp >= u64::from(model.memory_pressure_bp)
        || obs.pressure.evictable_backlog_bytes >= model.evictable_backlog_bytes
        || headroom_low;

    let pending_x2 = latency.queue_len.saturating_mul(2);
    let margin_x2 = (pending_x2 as i64 - i64::from(model.overload_cut_times_two))
        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;

    if memory_hot {
        let conf = if obs.pressure.emergency { 9_900 } else { 9_400 };
        return DistilledDecision {
            profile: ProfileId::Reclaim,
            regime: DistilledRegime::OverloadOrReclaim,
            confidence_bp: conf,
            abstain_low_confidence: conf < model.min_confidence_bp,
            margin_jobs_times_two: margin_x2,
        };
    }

    // Overload regime: latency queue at or above the 14.5 cut sheds latency priority
    // to protect the >= 95% batch CPU and completion floor.
    if u64::from(pending_x2) >= u64::from(model.overload_cut_times_two) {
        let excess = pending_x2.saturating_sub(model.overload_cut_times_two);
        let conf = (7_800u32 + excess.saturating_mul(150)).min(9_800) as u16;
        return DistilledDecision {
            profile: ProfileId::Reclaim,
            regime: DistilledRegime::OverloadOrReclaim,
            confidence_bp: conf,
            abstain_low_confidence: conf < model.min_confidence_bp,
            margin_jobs_times_two: margin_x2,
        };
    }

    // Under-capacity burst regime: elevate latency class when queue is elevated,
    // wait exceeds threshold, or solo arrival is pending.
    let wait_elevated =
        latency.wait_samples > 0 && latency.max_wait_us >= model.latency_wait_elevated_us;
    let solo_pending =
        latency.queue_len > 0 && maintenance.runnable == 0 && batch.queue_len == 0;
    if latency.queue_len >= model.latency_queue_elevated || wait_elevated || solo_pending {
        let slack = model.overload_cut_times_two.saturating_sub(pending_x2);
        let conf = (7_600u32 + slack.saturating_mul(75)).min(9_700) as u16;
        return DistilledDecision {
            profile: ProfileId::Latency,
            regime: DistilledRegime::UnderCapacityBurst,
            confidence_bp: conf,
            abstain_low_confidence: conf < model.min_confidence_bp,
            margin_jobs_times_two: margin_x2,
        };
    }

    // Sustained batch regime: sustained batch demand with quiet latency queue.
    if batch.queue_len >= model.batch_queue_high && latency.queue_len == 0 {
        let conf = (8_200u32 + batch.queue_len.saturating_mul(120)).min(9_600) as u16;
        return DistilledDecision {
            profile: ProfileId::Throughput,
            regime: DistilledRegime::SustainedBatch,
            confidence_bp: conf,
            abstain_low_confidence: conf < model.min_confidence_bp,
            margin_jobs_times_two: margin_x2,
        };
    }

    // Idle coalesced / balanced baseline.
    let conf = 9_100u16;
    DistilledDecision {
        profile: ProfileId::Balanced,
        regime: DistilledRegime::IdleCoalesced,
        confidence_bp: conf,
        abstain_low_confidence: conf < model.min_confidence_bp,
        margin_jobs_times_two: margin_x2,
    }
}

pub fn choose_distilled(obs: &Observation, model: &DistilledJointModel) -> ProfileId {
    evaluate_distilled(obs, model).profile
}

fn group(obs: &Observation, class: GroupId) -> &policy_types::GroupSample {
    obs.groups
        .iter()
        .find(|sample| sample.class == class)
        .expect("observation contains every class")
}

#[cfg(test)]
mod tests {
    use super::*;
    use policy_types::Pressure;

    #[test]
    fn distilled_joint_policy_matches_all_mixed_v1_phases() {
        let expected = [
            ("burst", ProfileId::Latency, DistilledRegime::UnderCapacityBurst),
            ("steady", ProfileId::Throughput, DistilledRegime::SustainedBatch),
            ("idle", ProfileId::Balanced, DistilledRegime::IdleCoalesced),
            ("memory", ProfileId::Reclaim, DistilledRegime::OverloadOrReclaim),
            ("recovery", ProfileId::Balanced, DistilledRegime::IdleCoalesced),
        ];
        for (phase, (name, profile, regime)) in workloads::mixed_v1().iter().zip(expected) {
            assert_eq!(phase.name, name);
            let decision = evaluate_distilled(&workloads::observation(*phase), &DISTILLED_JOINT_V1);
            assert_eq!(decision.profile, profile, "{name}");
            assert_eq!(decision.regime, regime, "{name}");
            assert!(!decision.abstain_low_confidence, "{name}");
            assert!(decision.confidence_bp >= DISTILLED_JOINT_V1.min_confidence_bp);
        }
    }

    #[test]
    fn distilled_joint_policy_matches_all_sealed_jobs_v2_golden_cases() {
        let raw = include_str!("../../../tests/policy-golden/pending-threshold-v0.json");
        let rows: Vec<serde_json::Value> = serde_json::from_str(raw).unwrap();
        assert!(!rows.is_empty());
        for row in rows {
            let pending = row["pending"].as_u64().unwrap() as u32;
            let expected = match row["choice"].as_str().unwrap() {
                "latency" => ProfileId::Latency,
                "reclaim" => ProfileId::Reclaim,
                other => panic!("unexpected profile {other}"),
            };
            let mut obs = Observation::quiet(1_000_000);
            obs.groups[0].queue_len = pending;
            obs.groups[0].wait_samples = 10;
            obs.groups[0].max_wait_us = if pending < 15 { 45_000 } else { 850_000 };
            obs.groups[1].queue_len = 3;
            obs.pressure = Pressure::idle();
            assert_eq!(
                choose_distilled(&obs, &DISTILLED_JOINT_V1),
                expected,
                "pending={pending}"
            );
        }
    }
}
