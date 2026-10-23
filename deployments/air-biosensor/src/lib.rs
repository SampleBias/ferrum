//! Embedded Deployment: Air Biological Sampling Sensor (`deployments/air-biosensor`).
//!
//! This crate is an isolated embedded application deployment built on top of the
//! generic Ferrum adaptive unikernel (`hermit-policy-abi` and `policy-core`).
//! The base kernel and core crates remain completely domain-agnostic.
//!
//! Models an air biological sampling application across four operational phases:
//! 1. `clean_air_patrol`: Low-rate optical particle counter polling (battery-critical).
//! 2. `plume_trigger`: Bio-aerosol fluorescence burst requiring immediate capture
//!    before the impinger valve deadline (analytical sensitivity / recall critical).
//! 3. `clutter_confirm`: Multi-stage spectral/genomic deconvolution separating true
//!    pathogens from pollen/dust interferents (analytical specificity / precision critical).
//! 4. `ring_reclaim`: High dust/saturation memory pressure requiring safe cache
//!    eviction of negative controls and emergency hysteresis recovery.

#![forbid(unsafe_code)]

use policy_types::{GroupId, Observation, ProfileId, MANAGED_ARENA_BYTES};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BiosensorPhaseSpec {
    pub name: &'static str,
    pub duration_us: u64,
    pub latency_queue: u32,
    pub latency_max_wait_us: u64,
    pub latency_wait_samples: u32,
    pub batch_queue: u32,
    pub maintenance_runnable: bool,
    pub managed_used_bytes: u64,
    pub headroom_bytes: u64,
    pub evictable_backlog_bytes: u64,
    pub emergency: bool,
    /// True biological plume events arriving in this phase (for sensitivity / recall).
    pub true_plume_events: u32,
    /// Interferent / background clutter candidates requiring deep confirmation (for specificity).
    pub clutter_candidates: u32,
    /// Capture valve deadline in microseconds for plume trigger events.
    pub capture_deadline_us: u64,
    /// Expected optimal joint profile for this phase.
    pub expected_profile: ProfileId,
}

pub const BIOSENSOR_V1: &[BiosensorPhaseSpec] = &[
    BiosensorPhaseSpec {
        name: "clean_air_patrol",
        duration_us: 500_000,
        latency_queue: 0,
        latency_max_wait_us: 0,
        latency_wait_samples: 0,
        batch_queue: 0,
        maintenance_runnable: false,
        managed_used_bytes: 8 * 1024 * 1024,
        headroom_bytes: 128 * 1024 * 1024,
        evictable_backlog_bytes: 0,
        emergency: false,
        true_plume_events: 0,
        clutter_candidates: 0,
        capture_deadline_us: 35_000,
        expected_profile: ProfileId::Balanced,
    },
    BiosensorPhaseSpec {
        name: "plume_trigger",
        duration_us: 500_000,
        latency_queue: 8,
        latency_max_wait_us: 18_000,
        latency_wait_samples: 8,
        batch_queue: 2,
        maintenance_runnable: false,
        managed_used_bytes: 48 * 1024 * 1024,
        headroom_bytes: 112 * 1024 * 1024,
        evictable_backlog_bytes: 0,
        emergency: false,
        true_plume_events: 12,
        clutter_candidates: 4,
        capture_deadline_us: 35_000,
        expected_profile: ProfileId::Latency,
    },
    BiosensorPhaseSpec {
        name: "clutter_confirm",
        duration_us: 500_000,
        latency_queue: 0,
        latency_max_wait_us: 2_000,
        latency_wait_samples: 4,
        batch_queue: 10,
        maintenance_runnable: false,
        managed_used_bytes: 84 * 1024 * 1024,
        headroom_bytes: 96 * 1024 * 1024,
        evictable_backlog_bytes: 0,
        emergency: false,
        true_plume_events: 4,
        clutter_candidates: 12,
        capture_deadline_us: 35_000,
        expected_profile: ProfileId::Throughput,
    },
    BiosensorPhaseSpec {
        name: "ring_reclaim",
        duration_us: 500_000,
        latency_queue: 22,
        latency_max_wait_us: 420_000,
        latency_wait_samples: 16,
        batch_queue: 6,
        maintenance_runnable: true,
        managed_used_bytes: 236 * 1024 * 1024,
        headroom_bytes: 48 * 1024 * 1024,
        evictable_backlog_bytes: 64 * 1024 * 1024,
        emergency: true,
        true_plume_events: 4,
        clutter_candidates: 8,
        capture_deadline_us: 35_000,
        expected_profile: ProfileId::Reclaim,
    },
];

pub fn biosensor_v1() -> &'static [BiosensorPhaseSpec] {
    BIOSENSOR_V1
}

/// Build the kernel telemetry [`Observation`] corresponding to `phase`.
pub fn biosensor_observation(phase: BiosensorPhaseSpec, service_us: [u64; 3]) -> Observation {
    let mut obs = Observation::quiet(phase.duration_us);
    obs.groups[0].class = GroupId::Latency;
    obs.groups[0].queue_len = phase.latency_queue;
    obs.groups[0].runnable = phase.latency_queue.min(3);
    obs.groups[0].cpu_service_us = service_us[0];
    obs.groups[0].max_wait_us = phase.latency_max_wait_us;
    obs.groups[0].wait_samples = phase.latency_wait_samples;
    obs.groups[0].completions = u64::from(phase.latency_wait_samples);

    obs.groups[1].class = GroupId::Batch;
    obs.groups[1].queue_len = phase.batch_queue;
    obs.groups[1].runnable = phase.batch_queue.min(3);
    obs.groups[1].cpu_service_us = service_us[1];

    obs.groups[2].class = GroupId::Maintenance;
    obs.groups[2].queue_len = u32::from(phase.maintenance_runnable);
    obs.groups[2].runnable = if phase.maintenance_runnable { 2 } else { 0 };
    obs.groups[2].cpu_service_us = service_us[2];

    obs.pressure.managed_cap_bytes = MANAGED_ARENA_BYTES;
    obs.pressure.managed_used_bytes = phase.managed_used_bytes;
    obs.pressure.headroom_bytes = phase.headroom_bytes;
    obs.pressure.evictable_backlog_bytes = phase.evictable_backlog_bytes;
    obs.pressure.emergency = phase.emergency;
    obs
}

/// Summary metrics for an air biological sampling run across all phases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BiosensorMetrics {
    /// Total true biological plume frames offered across the trajectory.
    pub plumes_offered: u32,
    /// True biological plume frames captured and admitted within the valve deadline.
    pub plumes_captured: u32,
    /// Analytical sensitivity (recall) in basis points (`0..=10_000`).
    pub sensitivity_bp: u32,
    /// Total clutter/interferent candidates requiring deep spectral/qPCR confirmation.
    pub clutter_offered: u32,
    /// Clutter candidates accurately rejected by completed deep confirmation with calibrated baseline.
    pub clutter_rejected: u32,
    /// Analytical specificity (true negative rate) in basis points (`0..=10_000`).
    pub specificity_bp: u32,
    /// Timer wakeups recorded during `clean_air_patrol`.
    pub patrol_timer_wakeups: u64,
    /// Timer wakeups coalesced (saved) during `clean_air_patrol`.
    pub patrol_coalesced_wakeups: u64,
    /// Idle sleep residency (microseconds) during `clean_air_patrol`.
    pub patrol_idle_us: u64,
    /// Evicted negative-control cache bytes during `ring_reclaim`.
    pub reclaimed_cache_bytes: u64,
}

impl BiosensorMetrics {
    #[allow(clippy::too_many_arguments)]
    pub fn compute(
        plumes_offered: u32,
        plumes_captured: u32,
        clutter_offered: u32,
        clutter_rejected: u32,
        patrol_timer_wakeups: u64,
        patrol_coalesced_wakeups: u64,
        patrol_idle_us: u64,
        reclaimed_cache_bytes: u64,
    ) -> Self {
        let sensitivity_bp = if plumes_offered == 0 {
            10_000
        } else {
            (u64::from(plumes_captured.min(plumes_offered)) * 10_000 / u64::from(plumes_offered))
                as u32
        };
        let specificity_bp = if clutter_offered == 0 {
            10_000
        } else {
            (u64::from(clutter_rejected.min(clutter_offered)) * 10_000
                / u64::from(clutter_offered)) as u32
        };
        Self {
            plumes_offered,
            plumes_captured,
            sensitivity_bp,
            clutter_offered,
            clutter_rejected,
            specificity_bp,
            patrol_timer_wakeups,
            patrol_coalesced_wakeups,
            patrol_idle_us,
            reclaimed_cache_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use policy_core::resources::SmallDeviceEnvelope;
    use policy_core::{evaluate_distilled, DISTILLED_JOINT_V1};

    #[test]
    fn biosensor_v1_phases_cover_all_four_joint_profiles() {
        let phases = biosensor_v1();
        assert_eq!(phases.len(), 4);
        assert_eq!(phases[0].expected_profile, ProfileId::Balanced);
        assert_eq!(phases[1].expected_profile, ProfileId::Latency);
        assert_eq!(phases[2].expected_profile, ProfileId::Throughput);
        assert_eq!(phases[3].expected_profile, ProfileId::Reclaim);

        for phase in phases {
            let obs = biosensor_observation(*phase, [100_000, 100_000, 50_000]);
            assert_eq!(obs.window_us, phase.duration_us);
            assert_eq!(obs.groups[0].queue_len, phase.latency_queue);
            assert_eq!(obs.groups[1].queue_len, phase.batch_queue);
            assert_eq!(obs.pressure.emergency, phase.emergency);
            let decision = evaluate_distilled(&obs, &DISTILLED_JOINT_V1);
            assert_eq!(decision.profile, phase.expected_profile, "{}", phase.name);
            assert!(!decision.abstain_low_confidence, "{}", phase.name);
        }
    }

    #[test]
    fn metrics_and_64m_envelope_fit_air_biosensor_budget() {
        let env = SmallDeviceEnvelope::EMBEDDED_64M;
        assert_eq!(env.total_guest_bytes, 64 * 1024 * 1024);
        assert_eq!(env.cache_cap_bytes, 16 * 1024 * 1024);
        let m = BiosensorMetrics::compute(20, 20, 24, 24, 14, 236, 485_000, 48 * 1024 * 1024);
        assert_eq!(m.sensitivity_bp, 10_000);
        assert_eq!(m.specificity_bp, 10_000);
        assert!(m.patrol_coalesced_wakeups > m.patrol_timer_wakeups * 10);
    }
}
