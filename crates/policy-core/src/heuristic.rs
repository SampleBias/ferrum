use policy_types::{GroupId, Observation, ProfileId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeuristicThresholds {
    pub latency_queue_high: u32,
    pub latency_wait_us_high: u64,
    pub batch_queue_high: u32,
    pub memory_pressure_bp: u16,
    pub evictable_backlog_bytes: u64,
}

pub use crate::catalog::HEURISTIC_V0;

/// Untuned development controller. Same inputs are available to a later model.
pub fn choose_heuristic(obs: &Observation, thresholds: &HeuristicThresholds) -> ProfileId {
    let latency = group(obs, GroupId::Latency);
    let batch = group(obs, GroupId::Batch);
    let pressure_bp = if obs.pressure.managed_cap_bytes == 0 {
        0
    } else {
        obs.pressure
            .managed_used_bytes
            .saturating_mul(10_000)
            / obs.pressure.managed_cap_bytes
    };
    if obs.pressure.emergency
        || pressure_bp >= u64::from(thresholds.memory_pressure_bp)
        || obs.pressure.evictable_backlog_bytes >= thresholds.evictable_backlog_bytes
    {
        return ProfileId::Reclaim;
    }
    let wait_high =
        latency.wait_samples > 0 && latency.max_wait_us >= thresholds.latency_wait_us_high;
    if latency.queue_len >= thresholds.latency_queue_high || wait_high {
        return ProfileId::Latency;
    }
    if batch.queue_len >= thresholds.batch_queue_high && latency.queue_len == 0 {
        return ProfileId::Throughput;
    }
    ProfileId::Balanced
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
