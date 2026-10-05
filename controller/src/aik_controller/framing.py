"""Frame codec shared with crates/policy-wire."""

import hashlib
import hmac
import json
import struct

GUEST_TO_CONTROLLER = b"aik1-guest-to-controller"
CONTROLLER_TO_GUEST = b"aik1-controller-to-guest"
MAX_PAYLOAD = 16_384


class FrameError(Exception):
    pass


def seal(direction: bytes, key: bytes, payload: bytes) -> bytes:
    if len(key) != 32:
        raise FrameError("key must be 32 bytes")
    if len(payload) > MAX_PAYLOAD:
        raise FrameError("payload too long")
    length = struct.pack(">I", len(payload))
    tag = hmac.new(key, direction + length + payload, hashlib.sha256).digest()
    return length + payload + tag


def open_frame(direction: bytes, key: bytes, frame: bytes) -> bytes:
    if len(frame) < 36:
        raise FrameError("truncated")
    length = struct.unpack(">I", frame[:4])[0]
    if length > MAX_PAYLOAD:
        raise FrameError("payload too long")
    total = 4 + length + 32
    if len(frame) != total:
        raise FrameError("truncated" if len(frame) < total else "trailing")
    payload = frame[4 : 4 + length]
    expected = hmac.new(key, direction + frame[:4] + payload, hashlib.sha256).digest()
    if not hmac.compare_digest(expected, frame[4 + length :]):
        raise FrameError("bad mac")
    return payload


def canonical_snapshot(fields: dict) -> str:
    """Schema-order snapshot body. The hash covers this text, not the framed bytes."""
    groups = []
    for group in fields["groups"]:
        groups.append(
            "{"
            f'"class":"{group["class"]}",'
            f'"queue_len":{group["queue_len"]},'
            f'"runnable":{group["runnable"]},'
            f'"cpu_service_us":{group["cpu_service_us"]},'
            f'"max_wait_us":{group["max_wait_us"]},'
            f'"wait_samples":{group["wait_samples"]},'
            f'"completions":{group["completions"]},'
            f'"managed_bytes":{group["managed_bytes"]}'
            "}"
        )
    pressure = fields["pressure"]
    emergency = "true" if pressure["emergency"] else "false"
    override = "true" if fields["override_active"] else "false"
    return (
        "{"
        f'"protocol":{fields["protocol"]},'
        '"kind":"snapshot",'
        f'"boot_id":"{fields["boot_id"]}",'
        f'"session_id":"{fields["session_id"]}",'
        f'"request_seq":{fields["request_seq"]},'
        f'"catalog_id":"{fields["catalog_id"]}",'
        f'"catalog_hash":"{fields["catalog_hash"]}",'
        f'"telemetry_version":{fields["telemetry_version"]},'
        f'"captured_guest_us":{fields["captured_guest_us"]},'
        f'"window_us":{fields["window_us"]},'
        f'"accept_until_guest_us":{fields["accept_until_guest_us"]},'
        f'"lease_until_guest_us":{fields["lease_until_guest_us"]},'
        f'"base_generation":{fields["base_generation"]},'
        f'"objective_id":"{fields["objective_id"]}",'
        f'"groups":[{",".join(groups)}],'
        '"pressure":{'
        f'"managed_used_bytes":{pressure["managed_used_bytes"]},'
        f'"managed_cap_bytes":{pressure["managed_cap_bytes"]},'
        f'"headroom_bytes":{pressure["headroom_bytes"]},'
        f'"evictable_backlog_bytes":{pressure["evictable_backlog_bytes"]},'
        f'"emergency":{emergency}'
        "},"
        f'"current_profile":"{fields["current_profile"]}",'
        f'"override_active":{override}'
        "}"
    )


def snapshot_hash(fields: dict) -> str:
    body = canonical_snapshot(fields)
    return hashlib.sha256(body.encode()).hexdigest()


def loads(payload: bytes):
    return json.loads(payload)
