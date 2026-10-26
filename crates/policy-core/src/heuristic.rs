use policy_types::{CatalogId, GroupId, Observation, ProfileId};

use crate::catalog_spec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeuristicThresholds {
    pub latency_queue_high: u32,
    pub latency_wait_us_high: u64,
    pub batch_queue_high: u32,
    pub memory_pressure_bp: u16,
    pub evictable_backlog_bytes: u64,
}

pub use crate::catalog::{HEURISTIC_V0, HEURISTIC_V1, HEURISTIC_V2};

/// `configs/heuristic-v1.json`. Ratios are basis points of a class's share
/// under a profile, over the classes that wanted CPU in the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeuristicV1 {
    pub base: HeuristicThresholds,
    pub catalog: CatalogId,
    pub reference: ProfileId,
    pub contended_bp: u64,
    pub saturated_bp: u64,
    pub release_bp: u64,
    pub batch_floor_bp: u64,
    pub stall_wait_us: u64,
    /// Pending jobs required, with the stall wait, before a queue counts as stalled.
    /// heuristic-v1 uses 1, so any waiting job can stall. heuristic-v2 raises it.
    pub backlog_min: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Regime {
    Normal,
    Overload,
}

impl Regime {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Overload => "overload",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "normal" => Some(Self::Normal),
            "overload" => Some(Self::Overload),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeuristicRule {
    MemoryPressure,
    OverloadHold,
    BatchFloor,
    LatencyOverload,
    MeasureAtReference,
    LatencyPressure,
    BatchDemand,
    Fallback,
}

impl HeuristicRule {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MemoryPressure => "memory_pressure_or_emergency",
            Self::OverloadHold => "overload_hold",
            Self::BatchFloor => "batch_floor",
            Self::LatencyOverload => "latency_overload",
            Self::MeasureAtReference => "measure_at_reference",
            Self::LatencyPressure => "latency_queue_or_wait_high",
            Self::BatchDemand => "sustained_batch_demand",
            Self::Fallback => "default_balanced_fallback",
        }
    }
}

/// What v1 measured. 10000 bp load means the class received exactly the
/// share the profile gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeuristicSignals {
    pub latency_load_bp: u64,
    pub latency_current_bp: u64,
    pub batch_retained_bp: Option<u64>,
    pub busy_bp: u64,
    pub latency_below_reference: bool,
    pub stalled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeuristicDecision {
    pub profile: ProfileId,
    pub rule: HeuristicRule,
    pub regime: Regime,
    pub signals: HeuristicSignals,
}

/// Untuned development controller. Same inputs are available to a later model.
pub fn choose_heuristic(obs: &Observation, thresholds: &HeuristicThresholds) -> ProfileId {
    base_rule(obs, thresholds).0
}

fn memory_hot(obs: &Observation, thresholds: &HeuristicThresholds) -> bool {
    let pressure_bp = if obs.pressure.managed_cap_bytes == 0 {
        0
    } else {
        obs.pressure
            .managed_used_bytes
            .saturating_mul(10_000)
            / obs.pressure.managed_cap_bytes
    };
    obs.pressure.emergency
        || pressure_bp >= u64::from(thresholds.memory_pressure_bp)
        || obs.pressure.evictable_backlog_bytes >= thresholds.evictable_backlog_bytes
}

fn base_rule(obs: &Observation, thresholds: &HeuristicThresholds) -> (ProfileId, HeuristicRule) {
    let latency = group(obs, GroupId::Latency);
    let batch = group(obs, GroupId::Batch);
    if memory_hot(obs, thresholds) {
        return (ProfileId::Reclaim, HeuristicRule::MemoryPressure);
    }
    let wait_high =
        latency.wait_samples > 0 && latency.max_wait_us >= thresholds.latency_wait_us_high;
    if latency.queue_len >= thresholds.latency_queue_high || wait_high {
        return (ProfileId::Latency, HeuristicRule::LatencyPressure);
    }
    if batch.queue_len >= thresholds.batch_queue_high && latency.queue_len == 0 {
        return (ProfileId::Throughput, HeuristicRule::BatchDemand);
    }
    (ProfileId::Balanced, HeuristicRule::Fallback)
}

const CLASSES: [GroupId; 3] = [GroupId::Latency, GroupId::Batch, GroupId::Maintenance];

pub fn heuristic_v1_signals(
    obs: &Observation,
    current: ProfileId,
    config: &HeuristicV1,
) -> HeuristicSignals {
    let spec = catalog_spec(config.catalog);
    let reference = spec.profile(config.reference).weights();
    let now = spec.profile(current).weights();
    let window = obs.window_us.max(1);
    let mut ref_sum = 0u64;
    let mut cur_sum = 0u64;
    let mut batch_active = false;
    for (i, class) in CLASSES.into_iter().enumerate() {
        let sample = group(obs, class);
        let active = class == GroupId::Latency
            || sample.runnable > 0
            || sample.queue_len > 0
            || sample.cpu_service_us > 0;
        if active {
            ref_sum += u64::from(reference[i]);
            cur_sum += u64::from(now[i]);
        }
        if class == GroupId::Batch {
            batch_active = active;
        }
    }
    let share_bp = |service: u64, sum: u64, weight: u32| {
        service.saturating_mul(sum).saturating_mul(10_000)
            / window.saturating_mul(u64::from(weight).max(1))
    };
    let latency = group(obs, GroupId::Latency);
    let batch = group(obs, GroupId::Batch);
    let busy = obs
        .groups
        .iter()
        .fold(0u64, |sum, sample| sum.saturating_add(sample.cpu_service_us));
    HeuristicSignals {
        latency_load_bp: share_bp(latency.cpu_service_us, ref_sum, reference[0]),
        latency_current_bp: share_bp(latency.cpu_service_us, cur_sum, now[0]),
        batch_retained_bp: batch_active.then(|| share_bp(batch.cpu_service_us, ref_sum, reference[1])),
        busy_bp: busy.saturating_mul(10_000) / window,
        latency_below_reference: u64::from(now[0]) * ref_sum < u64::from(reference[0]) * cur_sum,
        stalled: latency.queue_len >= config.backlog_min
            && latency.max_wait_us >= config.stall_wait_us,
    }
}

/// `heuristic-v1`: v0's rules behind an overload regime.
///
/// When latency already holds its whole reference share with a backlog that
/// is not draining, or holds more than that share while batch drops under its
/// floor, a latency boost only moves CPU from batch. v1 then keeps the
/// reference profile until latency load drops under `release_bp`. `regime`
/// is what the session's previous round returned; a one-shot decision starts
/// from `Normal`.
pub fn evaluate_heuristic_v1(
    obs: &Observation,
    current: ProfileId,
    regime: Regime,
    config: &HeuristicV1,
) -> HeuristicDecision {
    let signals = heuristic_v1_signals(obs, current, config);
    let out = |profile, rule, regime| HeuristicDecision {
        profile,
        rule,
        regime,
        signals,
    };
    if memory_hot(obs, &config.base) {
        return out(ProfileId::Reclaim, HeuristicRule::MemoryPressure, regime);
    }
    let protect = signals.batch_retained_bp.is_some() && signals.busy_bp >= config.contended_bp;
    if regime == Regime::Overload && protect && signals.latency_load_bp >= config.release_bp {
        return out(config.reference, HeuristicRule::OverloadHold, Regime::Overload);
    }
    if protect {
        let batch_bp = signals.batch_retained_bp.unwrap_or(u64::MAX);
        if signals.latency_load_bp > 10_000 && batch_bp < config.batch_floor_bp {
            return out(config.reference, HeuristicRule::BatchFloor, Regime::Overload);
        }
        if signals.latency_load_bp >= config.saturated_bp && signals.stalled {
            return out(config.reference, HeuristicRule::LatencyOverload, Regime::Overload);
        }
        if signals.latency_below_reference && signals.latency_current_bp >= config.saturated_bp {
            return out(config.reference, HeuristicRule::MeasureAtReference, Regime::Normal);
        }
    }
    let (profile, rule) = base_rule(obs, &config.base);
    out(profile, rule, Regime::Normal)
}

fn active_classes(obs: &Observation) -> [bool; 3] {
    let mut active = [false; 3];
    for (i, class) in CLASSES.into_iter().enumerate() {
        let sample = group(obs, class);
        active[i] = class == GroupId::Latency
            || sample.runnable > 0
            || sample.queue_len > 0
            || sample.cpu_service_us > 0;
    }
    active
}

fn weight_share_bp(weights: [u32; 3], active: [bool; 3], index: usize) -> Option<u64> {
    if !active[index] {
        return None;
    }
    let sum = weights
        .iter()
        .zip(active)
        .filter(|(_, on)| *on)
        .fold(0u64, |sum, (weight, _)| sum.saturating_add(u64::from(*weight)));
    if sum == 0 {
        return None;
    }
    Some(u64::from(weights[index]).saturating_mul(10_000) / sum)
}

/// A profile cuts batch to fund latency when, over the classes that wanted
/// CPU, it gives latency more than the reference share and leaves batch under
/// `floor_bp` of the reference batch share. Reclaim moves weight to
/// maintenance, so a memory response is not this cut.
pub fn cuts_batch_floor(obs: &Observation, proposed: ProfileId, config: &HeuristicV1) -> bool {
    let spec = catalog_spec(config.catalog);
    let active = active_classes(obs);
    let reference = spec.profile(config.reference).weights();
    let proposed_weights = spec.profile(proposed).weights();
    let Some(reference_batch) = weight_share_bp(reference, active, 1) else {
        return false;
    };
    let Some(proposed_batch) = weight_share_bp(proposed_weights, active, 1) else {
        return false;
    };
    let Some(reference_latency) = weight_share_bp(reference, active, 0) else {
        return false;
    };
    let Some(proposed_latency) = weight_share_bp(proposed_weights, active, 0) else {
        return false;
    };
    if reference_batch == 0 {
        return false;
    }
    proposed_latency > reference_latency
        && proposed_batch.saturating_mul(10_000) / reference_batch < config.batch_floor_bp
}

/// Carry a batch-floor hold across proposals.
///
/// The hold is set and cleared only from a measurement taken at a profile that
/// does not already give latency more than its reference share. A latency boost
/// inflates the load reading, so those snapshots leave the hold as it was.
/// Saturation sets it. Falling under `release_bp`, or losing the contention
/// that makes batch worth protecting, clears it.
pub fn update_batch_floor_hold(
    hold: bool,
    obs: &Observation,
    current: ProfileId,
    config: &HeuristicV1,
) -> bool {
    let spec = catalog_spec(config.catalog);
    let reference = spec.profile(config.reference).weights();
    let now = spec.profile(current).weights();
    let active = active_classes(obs);
    let sum = |weights: [u32; 3]| {
        weights
            .iter()
            .zip(active)
            .filter(|(_, on)| *on)
            .fold(0u64, |total, (weight, _)| total.saturating_add(u64::from(*weight)))
    };
    let boosted = u64::from(now[0]) * sum(reference) > u64::from(reference[0]) * sum(now);
    if boosted {
        return hold;
    }
    let signals = heuristic_v1_signals(obs, current, config);
    let protect = signals.batch_retained_bp.is_some() && signals.busy_bp >= config.contended_bp;
    if protect && signals.latency_load_bp >= config.saturated_bp {
        return true;
    }
    if !protect || signals.latency_load_bp < config.release_bp {
        return false;
    }
    hold
}

/// One controller session's v1 heuristic. The regime carries between rounds.
#[derive(Clone, Copy, Debug)]
pub struct HeuristicSession {
    config: HeuristicV1,
    regime: Regime,
}

impl HeuristicSession {
    pub const fn new(config: HeuristicV1) -> Self {
        Self {
            config,
            regime: Regime::Normal,
        }
    }

    pub const fn regime(&self) -> Regime {
        self.regime
    }

    pub fn decide(&mut self, obs: &Observation, current: ProfileId) -> HeuristicDecision {
        let decision = evaluate_heuristic_v1(obs, current, self.regime, &self.config);
        self.regime = decision.regime;
        decision
    }
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
    use policy_types::{GroupSample, Pressure, MANAGED_ARENA_BYTES};

    fn obs_with(latency_q: u32, batch_q: u32, pressure: Pressure) -> Observation {
        let mut obs = Observation::quiet(1_000_000);
        obs.groups[0].queue_len = latency_q;
        obs.groups[1].queue_len = batch_q;
        obs.pressure = pressure;
        obs
    }

    #[test]
    fn seed_rules_cover_each_profile() {
        let t = &HEURISTIC_V0;
        let quiet = obs_with(0, 0, Pressure::idle());
        assert_eq!(choose_heuristic(&quiet, t), ProfileId::Balanced);

        let burst = obs_with(4, 0, Pressure::idle());
        assert_eq!(choose_heuristic(&burst, t), ProfileId::Latency);

        let mut waited = Observation::quiet(1_000_000);
        waited.groups[0] = GroupSample {
            class: GroupId::Latency,
            queue_len: 0,
            runnable: 1,
            cpu_service_us: 0,
            max_wait_us: 5_000,
            wait_samples: 1,
            completions: 0,
            managed_bytes: 0,
        };
        assert_eq!(choose_heuristic(&waited, t), ProfileId::Latency);

        let steady = obs_with(0, 4, Pressure::idle());
        assert_eq!(choose_heuristic(&steady, t), ProfileId::Throughput);

        let mut hot = Pressure::idle();
        hot.managed_cap_bytes = 10_000;
        hot.managed_used_bytes = 8_000;
        assert_eq!(
            choose_heuristic(&obs_with(0, 0, hot), t),
            ProfileId::Reclaim
        );

        let mut backlog = Pressure::idle();
        backlog.evictable_backlog_bytes = t.evictable_backlog_bytes;
        assert_eq!(
            choose_heuristic(&obs_with(8, 8, backlog), t),
            ProfileId::Reclaim
        );
        let _ = MANAGED_ARENA_BYTES;
    }

    #[test]
    fn mixed_v1_phases_select_the_seed_profiles() {
        use crate::catalog_spec;
        use policy_types::CatalogId;
        use workloads::{ideal_service, mixed_v1, service_follows};

        let expected = [
            ("burst", ProfileId::Latency),
            ("steady", ProfileId::Throughput),
            ("idle", ProfileId::Balanced),
            ("memory", ProfileId::Reclaim),
            ("recovery", ProfileId::Balanced),
        ];
        for (phase, (name, profile)) in mixed_v1().iter().zip(expected) {
            assert_eq!(phase.name, name);
            let choice = choose_heuristic(&workloads::observation(*phase), &HEURISTIC_V0);
            assert_eq!(choice, profile, "{name}");
            let weights = catalog_spec(CatalogId::CpuV1).profile(choice).weights();
            let duty = phase.duty();
            service_follows(ideal_service(duty, weights), duty, weights)
                .unwrap_or_else(|err| panic!("{name} ideal window: {err}"));
        }
    }

    #[test]
    fn v1_matches_v0_on_the_mixed_v1_sequence() {
        use crate::catalog_spec;
        use policy_types::CatalogId;
        use workloads::{ideal_service, measured, mixed_v1, SAMPLE_US};

        // The soak measures each phase under the profile the previous phase
        // left installed, then renews on parked windows. Two passes.
        let mut session = HeuristicSession::new(HEURISTIC_V1);
        let mut installed = ProfileId::Balanced;
        for _ in 0..2 {
            for phase in mixed_v1() {
                let v0 = choose_heuristic(&workloads::observation(*phase), &HEURISTIC_V0);
                let weights = catalog_spec(CatalogId::CpuV1).profile(installed).weights();
                let first = measured(*phase, SAMPLE_US, ideal_service(phase.duty(), weights));
                let decision = session.decide(&first, installed);
                assert_eq!(decision.profile, v0, "{} after {}", phase.name, installed.as_str());
                let parked = measured(*phase, SAMPLE_US, [0, 0, 0]);
                assert_eq!(session.decide(&parked, v0).profile, v0, "{} renewal", phase.name);
                installed = v0;
            }
        }
        assert_eq!(session.regime(), Regime::Normal);
    }

    #[test]
    fn a_cpu_bound_burst_measured_under_the_latency_profile_is_an_overload() {
        use crate::catalog_spec;
        use policy_types::CatalogId;
        use workloads::{ideal_service, measured, mixed_v1, SAMPLE_US};

        let burst = mixed_v1()[0];
        let weights = catalog_spec(CatalogId::CpuV1).profile(ProfileId::Latency).weights();
        let obs = measured(burst, SAMPLE_US, ideal_service(burst.duty(), weights));
        let decision = evaluate_heuristic_v1(&obs, ProfileId::Latency, Regime::Normal, &HEURISTIC_V1);
        assert_eq!((decision.profile, decision.rule), (ProfileId::Balanced, HeuristicRule::BatchFloor));
        assert_eq!(decision.signals.batch_retained_bp, Some(5_000));
    }

    fn jobs_window(latency_us: u64, batch_us: u64, pending: u32, wait_us: u64) -> Observation {
        let mut obs = Observation::quiet(1_000_000);
        obs.groups[0].queue_len = pending;
        obs.groups[0].runnable = pending.min(3);
        obs.groups[0].cpu_service_us = latency_us;
        obs.groups[0].max_wait_us = wait_us;
        obs.groups[0].wait_samples = 10;
        obs.groups[1].queue_len = 3;
        obs.groups[1].runnable = 3;
        obs.groups[1].cpu_service_us = batch_us;
        obs
    }

    #[test]
    fn overload_holds_the_reference_until_latency_drops_under_release() {
        let mut session = HeuristicSession::new(HEURISTIC_V1);
        let under = jobs_window(350_000, 650_000, 2, 40_000);
        assert_eq!(session.decide(&under, ProfileId::Balanced).profile, ProfileId::Latency);

        let overload = jobs_window(500_000, 500_000, 40, 600_000);
        let entered = session.decide(&overload, ProfileId::Balanced);
        assert_eq!(
            (entered.profile, entered.rule, entered.regime),
            (ProfileId::Balanced, HeuristicRule::LatencyOverload, Regime::Overload)
        );

        // Backlog drained but demand still near the share: stay put.
        let near = jobs_window(450_000, 550_000, 3, 20_000);
        let held = session.decide(&near, ProfileId::Balanced);
        assert_eq!((held.profile, held.rule), (ProfileId::Balanced, HeuristicRule::OverloadHold));

        let fell = jobs_window(400_000, 600_000, 3, 20_000);
        let left = session.decide(&fell, ProfileId::Balanced);
        assert_eq!(
            (left.profile, left.rule, left.regime),
            (ProfileId::Latency, HeuristicRule::LatencyPressure, Regime::Normal)
        );
    }

    #[test]
    fn a_boosted_latency_class_under_the_batch_floor_returns_to_reference() {
        // Latency profile in overload: 75% latency, 25% batch.
        let boosted = jobs_window(750_000, 250_000, 30, 100_000);
        let decision = evaluate_heuristic_v1(&boosted, ProfileId::Latency, Regime::Normal, &HEURISTIC_V1);
        assert_eq!(decision.rule, HeuristicRule::BatchFloor);
        assert_eq!(decision.profile, ProfileId::Balanced);
        assert_eq!(decision.signals.batch_retained_bp, Some(5_000));

        // Under capacity the same profile leaves batch everything else.
        let fine = jobs_window(400_000, 600_000, 2, 30_000);
        let decision = evaluate_heuristic_v1(&fine, ProfileId::Latency, Regime::Normal, &HEURISTIC_V1);
        assert_eq!(decision.profile, ProfileId::Latency);
    }

    #[test]
    fn latency_starved_by_throughput_is_measured_at_the_reference_first() {
        let starved = jobs_window(250_000, 750_000, 6, 100_000);
        let decision = evaluate_heuristic_v1(&starved, ProfileId::Throughput, Regime::Normal, &HEURISTIC_V1);
        assert_eq!(
            (decision.profile, decision.rule, decision.regime),
            (ProfileId::Balanced, HeuristicRule::MeasureAtReference, Regime::Normal)
        );
    }

    #[test]
    fn a_short_queue_with_a_long_wait_keeps_the_latency_boost() {
        let burst = jobs_window(500_000, 500_000, 3, 400_000);
        let v1 = evaluate_heuristic_v1(&burst, ProfileId::Balanced, Regime::Normal, &HEURISTIC_V1);
        let v2 = evaluate_heuristic_v1(&burst, ProfileId::Balanced, Regime::Normal, &HEURISTIC_V2);
        assert_eq!(v1.profile, ProfileId::Balanced);
        assert!(v1.signals.stalled);
        assert_eq!(v2.profile, ProfileId::Latency);
        assert!(!v2.signals.stalled);

        let backlog = jobs_window(500_000, 500_000, 26, 500_000);
        let held = evaluate_heuristic_v1(&backlog, ProfileId::Balanced, Regime::Normal, &HEURISTIC_V2);
        assert_eq!(held.profile, ProfileId::Balanced);
        assert!(held.signals.stalled);
    }

    #[test]
    fn the_kernel_floor_rejects_a_latency_cut_only_while_the_reference_is_saturated() {
        let under = jobs_window(200_000, 800_000, 2, 40_000);
        let mut hold = update_batch_floor_hold(false, &under, ProfileId::Balanced, &HEURISTIC_V1);
        assert!(!hold);

        let over = jobs_window(500_000, 500_000, 40, 500_000);
        hold = update_batch_floor_hold(hold, &over, ProfileId::Balanced, &HEURISTIC_V1);
        assert!(hold);
        assert!(cuts_batch_floor(&over, ProfileId::Latency, &HEURISTIC_V1));
        assert!(!cuts_batch_floor(&over, ProfileId::Balanced, &HEURISTIC_V1));
        assert!(!cuts_batch_floor(&over, ProfileId::Throughput, &HEURISTIC_V1));
        assert!(!cuts_batch_floor(&over, ProfileId::Reclaim, &HEURISTIC_V1));

        let mid = jobs_window(450_000, 550_000, 40, 500_000);
        hold = update_batch_floor_hold(hold, &mid, ProfileId::Balanced, &HEURISTIC_V1);
        assert!(hold, "a load inside the release band keeps the floor");

        hold = update_batch_floor_hold(hold, &under, ProfileId::Balanced, &HEURISTIC_V1);
        assert!(!hold);

        let boosted = jobs_window(750_000, 250_000, 2, 40_000);
        let kept = update_batch_floor_hold(false, &boosted, ProfileId::Latency, &HEURISTIC_V1);
        assert!(!kept, "a boost is not a measurement of demand");
    }

    #[test]
    fn golden_vectors_agree_with_python() {
        let raw = include_str!("../../../tests/policy-golden/heuristic-v1.json");
        let doc: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(doc["config"], "heuristic-v1");
        let cases = doc["cases"].as_array().unwrap();
        assert!(cases.len() >= 10);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let snap = &case["snapshot"];
            let mut obs = Observation::quiet(snap["window_us"].as_u64().unwrap());
            for (slot, value) in obs.groups.iter_mut().zip(snap["groups"].as_array().unwrap()) {
                let class = GroupId::parse(value["class"].as_str().unwrap()).unwrap();
                assert_eq!(slot.class, class, "{name}");
                slot.queue_len = value["queue_len"].as_u64().unwrap() as u32;
                slot.runnable = value["runnable"].as_u64().unwrap() as u32;
                slot.cpu_service_us = value["cpu_service_us"].as_u64().unwrap();
                slot.max_wait_us = value["max_wait_us"].as_u64().unwrap();
                slot.wait_samples = value["wait_samples"].as_u64().unwrap() as u32;
                slot.completions = value["completions"].as_u64().unwrap();
            }
            let pressure = &snap["pressure"];
            obs.pressure.managed_used_bytes = pressure["managed_used_bytes"].as_u64().unwrap();
            obs.pressure.managed_cap_bytes = pressure["managed_cap_bytes"].as_u64().unwrap();
            obs.pressure.headroom_bytes = pressure["headroom_bytes"].as_u64().unwrap();
            obs.pressure.evictable_backlog_bytes = pressure["evictable_backlog_bytes"].as_u64().unwrap();
            obs.pressure.emergency = pressure["emergency"].as_bool().unwrap();
            let current = ProfileId::parse(snap["current_profile"].as_str().unwrap()).unwrap();
            let regime = Regime::parse(case["regime"].as_str().unwrap()).unwrap();
            let decision = evaluate_heuristic_v1(&obs, current, regime, &HEURISTIC_V1);
            let expect = &case["expect"];
            assert_eq!(decision.profile.as_str(), expect["profile"], "{name}");
            assert_eq!(decision.rule.as_str(), expect["rule"], "{name}");
            assert_eq!(decision.regime.as_str(), expect["regime"], "{name}");
            assert_eq!(decision.signals.latency_load_bp, expect["latency_load_bp"], "{name}");
            assert_eq!(
                decision.signals.batch_retained_bp,
                expect["batch_retained_bp"].as_u64(),
                "{name}"
            );
        }
    }

    #[test]
    fn every_fixed_profile_has_an_ideal_window_on_every_phase() {
        use crate::catalog_spec;
        use policy_types::{CatalogId, ProfileId};
        use workloads::{ideal_service, service_follows};

        let profiles = [
            ProfileId::Balanced,
            ProfileId::Latency,
            ProfileId::Throughput,
            ProfileId::Reclaim,
        ];
        for profile in profiles {
            let weights = catalog_spec(CatalogId::CpuV1).profile(profile).weights();
            for phase in workloads::mixed_v1() {
                let duty = phase.duty();
                service_follows(ideal_service(duty, weights), duty, weights).unwrap_or_else(|err| {
                    panic!("{} {} ideal window: {err}", profile.as_str(), phase.name)
                });
            }
        }
    }
}
