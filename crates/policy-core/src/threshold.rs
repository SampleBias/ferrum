use policy_types::ProfileId;

/// Frozen `pending-threshold` from `configs/candidate-v0-r5-1600.json`.
///
/// The fit stores the cut as 14.5 jobs. Pending counts are integers, so the
/// comparison is `pending * 2 >= 29`: 14 selects `below`, 15 selects `above`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingThreshold {
    pub cut_times_two: u32,
    pub below: ProfileId,
    pub above: ProfileId,
}

pub const PENDING_THRESHOLD_V0: PendingThreshold = PendingThreshold {
    cut_times_two: 29,
    below: ProfileId::Latency,
    above: ProfileId::Reclaim,
};

pub fn choose_pending_threshold(pending_jobs: u32, threshold: &PendingThreshold) -> ProfileId {
    if u64::from(pending_jobs) * 2 >= u64::from(threshold.cut_times_two) {
        threshold.above
    } else {
        threshold.below
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cut_matches_the_python_half_job_boundary() {
        assert_eq!(
            choose_pending_threshold(14, &PENDING_THRESHOLD_V0),
            ProfileId::Latency
        );
        assert_eq!(
            choose_pending_threshold(15, &PENDING_THRESHOLD_V0),
            ProfileId::Reclaim
        );
    }

    #[test]
    fn sealed_units_match_the_recorded_choices() {
        let cases: &[(&str, u32, ProfileId)] = &[
            ("s12-u35/301", 1, ProfileId::Latency),
            ("s12-u45/312", 9, ProfileId::Latency),
            ("s12-u72/311", 56, ProfileId::Reclaim),
            ("s32-u65/401", 22, ProfileId::Reclaim),
        ];
        for (unit, pending, expected) in cases {
            assert_eq!(
                choose_pending_threshold(*pending, &PENDING_THRESHOLD_V0),
                *expected,
                "{unit}"
            );
        }
    }

    #[test]
    fn golden_fixture_agrees() {
        let raw = include_str!("../../../tests/policy-golden/pending-threshold-v0.json");
        let rows: Vec<serde_json::Value> = serde_json::from_str(raw).unwrap();
        assert!(!rows.is_empty());
        for row in rows {
            let pending = row["pending"].as_u64().unwrap() as u32;
            let choice = match row["choice"].as_str().unwrap() {
                "latency" => ProfileId::Latency,
                "reclaim" => ProfileId::Reclaim,
                other => panic!("unexpected profile {other}"),
            };
            assert_eq!(choose_pending_threshold(pending, &PENDING_THRESHOLD_V0), choice);
        }
    }
}
