import unittest

from aik_controller.server import accept_scheduler_report


def report(**overrides):
    body = {
        "kind": "shadow",
        "boot_id": "boot",
        "session_id": "session",
        "request_seq": 1,
        "previous_generation": 1,
        "generation": 1,
        "profile": "latency",
        "guest_us": 3_000_000,
        "lease_until_guest_us": 0,
    }
    body.update(overrides)
    return body


class ShadowReportTests(unittest.TestCase):
    def test_a_shadow_note_keeps_generation_and_the_lease(self):
        self.assertEqual(accept_scheduler_report(report(), "boot", "session", 1, "latency"), 0)

    def test_a_generation_advance_is_not_a_shadow_note(self):
        self.assertEqual(
            accept_scheduler_report(report(generation=2), "boot", "session", 1, "latency"),
            1,
        )

    def test_a_lease_is_not_a_shadow_note(self):
        self.assertEqual(
            accept_scheduler_report(
                report(lease_until_guest_us=6_000_000), "boot", "session", 1, "latency"
            ),
            1,
        )

    def test_the_noted_profile_must_be_the_proposal(self):
        self.assertEqual(
            accept_scheduler_report(report(profile="throughput"), "boot", "session", 1, "latency"),
            1,
        )
