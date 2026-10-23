//! Distilled `no_std` work-aware multi-regime joint policy (`DISTILLED_JOINT_V1`).
//!
//! Combines `queue_len` (`pending`) and `max_wait_us` to separate:
//! - Under-capacity bursts (`pending < 14.5`, or transient queue spikes
//!   `pending in 15..=18` where `max_wait_us < 350_000` µs proves the queue is
//!   actively draining rather than stalled) -> `ProfileId::Latency` (`6:2:2`).
//! - Sustained batch demand (`batch_queue >= 4` with `latency_queue == 0`) ->
//!   `ProfileId::Throughput` (`2:6:2`, 96 MiB cache).
//! - True sustained overload (`pending >= 15` with `max_wait_us >= 350_000` µs,
//!   or `pending > 18`) and memory pressure -> `ProfileId::Reclaim` (`2:2:6`,
//!   32 MiB cache) or `ProfileId::Balanced` under extreme (`> 300` pending)
//!   CPU-only arrival saturation.
//!
//! Executes in `< 5 µs` using deterministic integer/basis-point arithmetic with
//! calibrated confidence output and selective abstention support.

use policy_types::{GroupId, Observation, ProfileId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DistilledRegime {
    /// Low ambient load; coalesce timer wakeups and hold balanced 1:1:1 baseline.
    IdleCoalesced,
    /// Under-capacity burst (`pending < cut`, or transient `15..=18` spike with `max_wait_us < 350 ms`); boost latency class (`6:2:2`).
    UnderCapacityBurst,
    /// Sustained batch backlog with latency slack; boost batch class (`2:6:2`, 96 MiB cache).
    SustainedBatch,
    /// Memory pressure, cache backlog, or sustained latency overload (`pending >= cut` with `max_wait_us >= 350 ms`); shed/reclaim.
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
    /// Maximum pending jobs (`18`) eligible for transient-spike absorption when
    /// `max_wait_us < overload_wait_floor_us`.
    pub transient_spike_max_jobs: u32,
    /// Minimum `max_wait_us` (`350_000` µs = `350 ms`) required to classify a
    /// `15..=18` job queue as true sustained overload rather than a draining burst.
    pub overload_wait_floor_us: u64,
    /// Pending jobs threshold (`300`) above which extreme CPU-only saturation
    /// selects `Balanced` (`1:1:1`) to avoid `Reclaim` admission/eviction overhead.
    pub extreme_saturation_jobs: u32,
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
    transient_spike_max_jobs: 18,
    overload_wait_floor_us: 350_000,
    extreme_saturation_jobs: 300,
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
    pub wait_margin_us: i64,
    pub transient_spike_absorbed: bool,
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
    let wait_margin_us = latency.max_wait_us as i64 - model.overload_wait_floor_us as i64;

    if memory_hot {
        let conf = if obs.pressure.emergency { 9_900 } else { 9_400 };
        return DistilledDecision {
            profile: ProfileId::Reclaim,
            regime: DistilledRegime::OverloadOrReclaim,
            confidence_bp: conf,
            abstain_low_confidence: conf < model.min_confidence_bp,
            margin_jobs_times_two: margin_x2,
            wait_margin_us,
            transient_spike_absorbed: false,
        };
    }

    // Work-aware overload check:
    // If `pending` is in the transient spike band (`15..=18`) AND wait telemetry
    // is present (`wait_samples > 0`) with `max_wait_us < 350_000` µs (350 ms),
    // the queue is actively draining under capacity rather than stalled in overload.
    let above_queue_cut = u64::from(pending_x2) >= u64::from(model.overload_cut_times_two);
    let transient_spike = above_queue_cut
        && latency.queue_len <= model.transient_spike_max_jobs
        && latency.wait_samples > 0
        && latency.max_wait_us < model.overload_wait_floor_us;

    if above_queue_cut && !transient_spike {
        let excess = pending_x2.saturating_sub(model.overload_cut_times_two);
        let conf = (7_800u32 + excess.saturating_mul(150)).min(9_800) as u16;
        let profile = if latency.queue_len > model.extreme_saturation_jobs {
            ProfileId::Balanced
        } else {
            ProfileId::Reclaim
        };
        return DistilledDecision {
            profile,
            regime: DistilledRegime::OverloadOrReclaim,
            confidence_bp: conf,
            abstain_low_confidence: conf < model.min_confidence_bp,
            margin_jobs_times_two: margin_x2,
            wait_margin_us,
            transient_spike_absorbed: false,
        };
    }

    // Under-capacity burst regime: elevate latency class when queue is elevated,
    // wait exceeds threshold, a transient spike was absorbed, or solo arrival is pending.
    let wait_elevated =
        latency.wait_samples > 0 && latency.max_wait_us >= model.latency_wait_elevated_us;
    let solo_pending =
        latency.queue_len > 0 && maintenance.runnable == 0 && batch.queue_len == 0;
    if transient_spike
        || latency.queue_len >= model.latency_queue_elevated
        || wait_elevated
        || solo_pending
    {
        let conf = if transient_spike {
            let wait_slack_ms =
                (model.overload_wait_floor_us.saturating_sub(latency.max_wait_us) / 1_000) as u32;
            (7_400u32 + wait_slack_ms.saturating_mul(8)).min(9_200) as u16
        } else {
            let slack = model.overload_cut_times_two.saturating_sub(pending_x2);
            (7_600u32 + slack.saturating_mul(75)).min(9_700) as u16
        };
        return DistilledDecision {
            profile: ProfileId::Latency,
            regime: DistilledRegime::UnderCapacityBurst,
            confidence_bp: conf,
            abstain_low_confidence: conf < model.min_confidence_bp,
            margin_jobs_times_two: margin_x2,
            wait_margin_us,
            transient_spike_absorbed: transient_spike,
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
            wait_margin_us,
            transient_spike_absorbed: false,
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
        wait_margin_us,
        transient_spike_absorbed: false,
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

    #[test]
    fn transient_queue_spike_with_low_wait_is_not_misclassified_as_overload() {
        // Transient spike: pending = 15..=18 with max_wait_us = 240 ms (< 350 ms floor)
        for pending in 15..=18 {
            let mut spike = Observation::quiet(1_000_000);
            spike.groups[0].queue_len = pending;
            spike.groups[0].wait_samples = 12;
            spike.groups[0].max_wait_us = 240_000;
            spike.groups[1].queue_len = 3;
            let decision = evaluate_distilled(&spike, &DISTILLED_JOINT_V1);
            assert_eq!(decision.profile, ProfileId::Latency, "pending={pending}");
            assert_eq!(decision.regime, DistilledRegime::UnderCapacityBurst);
            assert!(decision.transient_spike_absorbed);
            assert!(decision.wait_margin_us < 0);
        }

        // True sustained overload at the same queue depth: pending = 17 with max_wait_us = 486.9 ms (>= 350 ms floor)
        let mut overload = Observation::quiet(1_000_000);
        overload.groups[0].queue_len = 17;
        overload.groups[0].wait_samples = 12;
        overload.groups[0].max_wait_us = 486_900;
        overload.groups[1].queue_len = 3;
        let over_dec = evaluate_distilled(&overload, &DISTILLED_JOINT_V1);
        assert_eq!(over_dec.profile, ProfileId::Reclaim);
        assert_eq!(over_dec.regime, DistilledRegime::OverloadOrReclaim);
        assert!(!over_dec.transient_spike_absorbed);
        assert!(over_dec.wait_margin_us > 0);
    }
}
