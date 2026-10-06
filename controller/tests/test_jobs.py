import unittest
from pathlib import Path

from aik_controller.jobs import digest, load_family, schedule

ROOT = Path(__file__).resolve().parents[2]
FAMILY = load_family(ROOT / "configs" / "workloads" / "jobs-v1.json")
FAMILY_V2 = load_family(ROOT / "configs" / "workloads" / "jobs-v2.json")


class JobScheduleTests(unittest.TestCase):
    def test_python_and_rust_build_the_same_schedule(self):
        arrivals = schedule(FAMILY, "s4-u35", 101)
        # Pinned in crates/workloads/src/jobs.rs and printed by the guest.
        self.assertEqual(digest(arrivals), 0xD58E8E4AF254388F)
        self.assertEqual(len(arrivals), 851)

    def test_scenario_and_seed_both_change_the_schedule(self):
        base = digest(schedule(FAMILY, "s4-u35", 101))
        self.assertNotEqual(base, digest(schedule(FAMILY, "s4-u35", 102)))
        self.assertNotEqual(base, digest(schedule(FAMILY, "s12-u35", 101)))

    def test_offered_load_is_job_time_times_rate(self):
        for family in (FAMILY, FAMILY_V2):
            for entry in family["scenarios"]:
                self.assertEqual(entry["offered_bp"], entry["job_us"] * entry["rate_per_s"] // 100)

    def test_a_shared_name_is_the_same_schedule_in_both_families(self):
        self.assertEqual(schedule(FAMILY, "s4-u35", 101), schedule(FAMILY_V2, "s4-u35", 101))
        v1_names = {entry["name"] for entry in FAMILY["scenarios"]}
        for entry in FAMILY_V2["scenarios"]:
            if entry["name"] in v1_names:
                self.assertEqual(entry, next(e for e in FAMILY["scenarios"] if e["name"] == entry["name"]))


if __name__ == "__main__":
    unittest.main()
