"""Mock controller. It speaks the guest protocol and chooses with the heuristic."""

import argparse
import hashlib
import json
import secrets
import socket
import struct
import sys
from pathlib import Path

from aik_controller.framing import (
    CONTROLLER_TO_GUEST,
    GUEST_TO_CONTROLLER,
    FrameError,
    open_frame,
    seal,
    snapshot_hash,
)
from aik_controller.heuristic import choose, load_thresholds

ROOT = Path(__file__).resolve().parents[3]


def load_key(path: Path) -> bytes:
    text = path.read_text().strip()
    key = bytes.fromhex(text)
    if len(key) != 32:
        raise FrameError("lab key must be 32 bytes")
    return key


def read_exact(conn: socket.socket, size: int) -> bytes:
    buf = bytearray()
    while len(buf) < size:
        chunk = conn.recv(size - len(buf))
        if not chunk:
            raise FrameError("truncated")
        buf.extend(chunk)
    return bytes(buf)


def read_payload(conn: socket.socket, key: bytes) -> dict:
    header = read_exact(conn, 4)
    length = struct.unpack(">I", header)[0]
    payload = read_exact(conn, length)
    tag = read_exact(conn, 32)
    body = open_frame(GUEST_TO_CONTROLLER, key, header + payload + tag)
    return json.loads(body)


def write_payload(conn: socket.socket, key: bytes, message: dict) -> None:
    payload = json.dumps(message, separators=(",", ":")).encode()
    conn.sendall(seal(CONTROLLER_TO_GUEST, key, payload))


def catalog_hash(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def serve_once(
    conn: socket.socket, key: bytes, catalog: dict, thresholds: dict, identity: dict, hold: bool
) -> int:
    hello = read_payload(conn, key)
    if hello.get("kind") != "hello" or hello.get("protocol") != 1:
        print("rejected hello", file=sys.stderr)
        return 1
    if hello.get("catalog_id") != catalog["id"] or hello.get("catalog_hash") != catalog["hash"]:
        print("rejected catalog", file=sys.stderr)
        return 1
    if hello.get("telemetry_version") != 1:
        print("rejected telemetry version", file=sys.stderr)
        return 1
    session_id = secrets.token_hex(16)
    print(f"HELLO boot={hello['boot_id']} session={session_id}", flush=True)
    write_payload(
        conn,
        key,
        {
            "protocol": 1,
            "kind": "hello_ack",
            "boot_id": hello["boot_id"],
            "session_id": session_id,
            "catalog_id": catalog["id"],
            "catalog_hash": catalog["hash"],
            "model_manifest_hash": identity["model_manifest_hash"],
            "calibration_hash": identity["calibration_hash"],
            "ready": True,
        },
    )

    snapshot = read_payload(conn, key)
    if snapshot.get("kind") != "snapshot":
        print("expected snapshot", file=sys.stderr)
        return 1
    if snapshot.get("boot_id") != hello["boot_id"] or snapshot.get("session_id") != session_id:
        print("snapshot identity mismatch", file=sys.stderr)
        return 1
    claimed = snapshot.get("snapshot_hash")
    body = dict(snapshot)
    body.pop("snapshot_hash", None)
    digest = snapshot_hash(body)
    if claimed != digest:
        print("snapshot hash mismatch", file=sys.stderr)
        return 1
    profile = choose(body, thresholds)
    print(f"PROPOSAL profile={profile} seq={snapshot['request_seq']}", flush=True)
    write_payload(
        conn,
        key,
        {
            "protocol": 1,
            "kind": "proposal",
            "boot_id": hello["boot_id"],
            "session_id": session_id,
            "request_seq": snapshot["request_seq"],
            "base_generation": snapshot["base_generation"],
            "catalog_id": catalog["id"],
            "catalog_hash": catalog["hash"],
            "snapshot_hash": digest,
            "profile": profile,
            "answer_confidence_bp": 10000,
            "model_manifest_hash": identity["model_manifest_hash"],
            "calibration_hash": identity["calibration_hash"],
            "reason_code": "heuristic",
        },
    )

    report = read_payload(conn, key)
    status = accept_scheduler_report(report, hello["boot_id"], session_id, snapshot["request_seq"], profile)
    if status == 0 and hold:
        # Stay connected for the rest of the lease so a kill is a dead peer,
        # not a controller that already finished and closed the socket.
        print("HOLDING", flush=True)
        conn.settimeout(None)
        try:
            while conn.recv(64):
                pass
        except OSError as err:
            print(f"HOLD_ENDED {err}", file=sys.stderr)
    return status


def accept_scheduler_report(
    report: dict, boot_id: str, session_id: str, request_seq: int, profile: str
) -> int:
    """Accept the guest frame that answers this proposal.

    An `applied` frame is the scheduler activation record: generation, guest
    timestamp, and lease. A `reject` frame is the scheduler dropping that same
    proposal. Either one completes the session. A transport or identity failure
    does not.
    """
    if report.get("boot_id") != boot_id or report.get("session_id") != session_id:
        print("report identity mismatch", file=sys.stderr)
        return 1
    if report.get("request_seq") != request_seq:
        print("report sequence mismatch", file=sys.stderr)
        return 1
    kind = report.get("kind")
    if kind == "applied":
        if report.get("profile") != profile:
            print("applied profile does not match the proposal", file=sys.stderr)
            return 1
        previous = report.get("previous_generation")
        generation = report.get("new_generation")
        guest_us = report.get("activated_guest_us")
        lease_until = report.get("lease_until_guest_us")
        if not all(isinstance(value, int) for value in (previous, generation, guest_us, lease_until)):
            print("applied acknowledgment is missing scheduler fields", file=sys.stderr)
            return 1
        if generation != previous + 1 or guest_us <= 0 or lease_until <= guest_us:
            print("applied acknowledgment is not a scheduler activation", file=sys.stderr)
            return 1
        if report.get("desired_matches_actual") is not True:
            print("applied acknowledgment has not converged", file=sys.stderr)
            return 1
        print(
            f"APPLIED profile={profile} generation={generation} previous={previous} "
            f"guest_us={guest_us} lease_until={lease_until}",
            flush=True,
        )
        return 0
    if kind == "reject":
        reason = report.get("reason")
        if reason not in {"late", "stale_generation", "emergency"}:
            print(f"unexpected reject {reason}", file=sys.stderr)
            return 1
        print(f"REJECTED reason={reason} seq={request_seq}", flush=True)
        return 0
    print(f"guest reported {kind} {report.get('reason', '')}", file=sys.stderr)
    return 1


def propose_once(
    conn: socket.socket, key: bytes, catalog: dict, thresholds: dict, identity: dict
) -> dict:
    """Hello, snapshot, and one proposal. The caller decides whether to read the report."""
    hello = read_payload(conn, key)
    if hello.get("kind") != "hello" or hello.get("protocol") != 1:
        raise FrameError("rejected hello")
    if hello.get("catalog_id") != catalog["id"] or hello.get("catalog_hash") != catalog["hash"]:
        raise FrameError("rejected catalog")
    session_id = secrets.token_hex(16)
    print(f"HELLO boot={hello['boot_id']} session={session_id}", flush=True)
    write_payload(
        conn,
        key,
        {
            "protocol": 1,
            "kind": "hello_ack",
            "boot_id": hello["boot_id"],
            "session_id": session_id,
            "catalog_id": catalog["id"],
            "catalog_hash": catalog["hash"],
            "model_manifest_hash": identity["model_manifest_hash"],
            "calibration_hash": identity["calibration_hash"],
            "ready": True,
        },
    )
    snapshot = read_payload(conn, key)
    if snapshot.get("kind") != "snapshot":
        raise FrameError("expected snapshot")
    if snapshot.get("boot_id") != hello["boot_id"] or snapshot.get("session_id") != session_id:
        raise FrameError("snapshot identity mismatch")
    body = dict(snapshot)
    body.pop("snapshot_hash", None)
    digest = snapshot_hash(body)
    if snapshot.get("snapshot_hash") != digest:
        raise FrameError("snapshot hash mismatch")
    profile = choose(body, thresholds)
    proposal = {
        "protocol": 1,
        "kind": "proposal",
        "boot_id": hello["boot_id"],
        "session_id": session_id,
        "request_seq": snapshot["request_seq"],
        "base_generation": snapshot["base_generation"],
        "catalog_id": catalog["id"],
        "catalog_hash": catalog["hash"],
        "snapshot_hash": digest,
        "profile": profile,
        "answer_confidence_bp": 10000,
        "model_manifest_hash": identity["model_manifest_hash"],
        "calibration_hash": identity["calibration_hash"],
        "reason_code": "heuristic",
    }
    print(f"PROPOSAL profile={profile} seq={snapshot['request_seq']}", flush=True)
    write_payload(conn, key, proposal)
    return proposal


def serve_lost_ack(
    listener: socket.socket, key: bytes, catalog: dict, thresholds: dict, identity: dict
) -> int:
    """Drop the applied frame, then require the reconnect snapshot to carry the kernel generation.

    The original proposal is replayed on the new connection. The guest must reject it.
    """
    try:
        return serve_lost_ack_session(listener, key, catalog, thresholds, identity)
    except (FrameError, ConnectionError, TimeoutError, json.JSONDecodeError) as err:
        print(f"session failed: {err}", file=sys.stderr)
        return 1


def serve_lost_ack_session(
    listener: socket.socket, key: bytes, catalog: dict, thresholds: dict, identity: dict
) -> int:
    first, _addr = listener.accept()
    first.settimeout(60)
    with first:
        proposal = propose_once(first, key, catalog, thresholds, identity)
    print(f"DROPPED_ACK seq={proposal['request_seq']}", flush=True)

    second, _addr = listener.accept()
    second.settimeout(60)
    with second:
        hello = read_payload(second, key)
        if hello.get("kind") != "hello" or hello.get("boot_id") != proposal["boot_id"]:
            print("reconnect hello rejected", file=sys.stderr)
            return 1
        session_id = secrets.token_hex(16)
        print(f"HELLO boot={hello['boot_id']} session={session_id}", flush=True)
        write_payload(
            second,
            key,
            {
                "protocol": 1,
                "kind": "hello_ack",
                "boot_id": hello["boot_id"],
                "session_id": session_id,
                "catalog_id": catalog["id"],
                "catalog_hash": catalog["hash"],
                "model_manifest_hash": identity["model_manifest_hash"],
                "calibration_hash": identity["calibration_hash"],
                "ready": True,
            },
        )
        snapshot = read_payload(second, key)
        if snapshot.get("kind") != "snapshot" or snapshot.get("session_id") != session_id:
            print("reconnect snapshot rejected", file=sys.stderr)
            return 1
        body = dict(snapshot)
        body.pop("snapshot_hash", None)
        if snapshot.get("snapshot_hash") != snapshot_hash(body):
            print("reconnect snapshot hash mismatch", file=sys.stderr)
            return 1
        generation = snapshot.get("base_generation")
        profile = snapshot.get("current_profile")
        if generation != proposal["base_generation"] + 1 or profile != proposal["profile"]:
            print(
                f"authoritative generation was not recovered: generation={generation} profile={profile}",
                file=sys.stderr,
            )
            return 1
        print(f"RECOVERED generation={generation} profile={profile}", flush=True)
        write_payload(second, key, proposal)
        report = read_payload(second, key)
        if report.get("kind") != "reject" or report.get("reason") != "identity_mismatch":
            print(f"replay was not rejected: {report}", file=sys.stderr)
            return 1
        if report.get("request_seq") != proposal["request_seq"] or report.get("session_id") != proposal["session_id"]:
            print("replay rejection did not name the original request", file=sys.stderr)
            return 1
        print(f"REPLAY_REJECTED reason={report['reason']} seq={report['request_seq']}", flush=True)
        return 0


def serve_reject_frames(
    listener: socket.socket, key: bytes, catalog: dict, identity: dict
) -> int:
    """Send four frames the guest must refuse without staging a profile."""
    conn, _addr = listener.accept()
    conn.settimeout(60)
    with conn:
        hello = read_payload(conn, key)
        if hello.get("kind") != "hello" or hello.get("boot_id") is None:
            print("rejected hello", file=sys.stderr)
            return 1
        session_id = secrets.token_hex(16)
        print(f"HELLO boot={hello['boot_id']} session={session_id}", flush=True)
        write_payload(
            conn,
            key,
            {
                "protocol": 1,
                "kind": "hello_ack",
                "boot_id": hello["boot_id"],
                "session_id": session_id,
                "catalog_id": catalog["id"],
                "catalog_hash": catalog["hash"],
                "model_manifest_hash": identity["model_manifest_hash"],
                "calibration_hash": identity["calibration_hash"],
                "ready": True,
            },
        )
        snapshot = read_payload(conn, key)
        if snapshot.get("kind") != "snapshot":
            print("expected snapshot", file=sys.stderr)
            return 1
        bad_mac = bytearray(seal(CONTROLLER_TO_GUEST, key, b'{"protocol":1,"kind":"proposal"}'))
        bad_mac[-1] ^= 0x01
        duplicate = seal(CONTROLLER_TO_GUEST, key, b'{"protocol":1,"protocol":1}')
        invalid = seal(CONTROLLER_TO_GUEST, key, b'{"protocol":1,"kind":"proposal","profile":1}')
        oversized = struct.pack(">I", 16_384 + 1)
        conn.sendall(bytes(bad_mac) + duplicate + invalid + oversized)
        print("REJECTED_FRAMES unauthenticated duplicate invalid oversized", flush=True)
        return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Ferrum mock policy controller")
    parser.add_argument("--bind", default="127.0.0.1:7777")
    parser.add_argument("--once", action="store_true")
    parser.add_argument("--hold", action="store_true", help="keep the session open after the scheduler report")
    parser.add_argument(
        "--lose-ack",
        action="store_true",
        help="drop the connection before the applied frame, then accept one reconnect",
    )
    parser.add_argument(
        "--reject-frames",
        action="store_true",
        help="send unauthenticated, duplicate, invalid, and oversized frames",
    )
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args(argv)

    root = args.root
    host, port_text = args.bind.rsplit(":", 1)
    key = load_key(root / "configs" / "lab-psk.hex")
    catalog_path = root / "configs" / "catalog-cpu-v1.json"
    catalog_doc = json.loads(catalog_path.read_text())
    catalog = {"id": catalog_doc["catalog_id"], "hash": catalog_hash(catalog_path)}
    thresholds = load_thresholds(root / "configs" / "heuristic-v0.json")
    identity = json.loads((root / "configs" / "lab-identity.json").read_text())

    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind((host, int(port_text)))
    listener.listen(1)
    bound_host, bound_port = listener.getsockname()
    print(f"LISTENING {bound_host} {bound_port}", flush=True)
    try:
        if args.lose_ack:
            return serve_lost_ack(listener, key, catalog, thresholds, identity)
        if args.reject_frames:
            return serve_reject_frames(listener, key, catalog, identity)
        while True:
            conn, _addr = listener.accept()
            # The guest answers after the scheduler acknowledgment. A TCG
            # one-second measurement can take longer than that on the host clock.
            conn.settimeout(60)
            with conn:
                try:
                    status = serve_once(conn, key, catalog, thresholds, identity, args.hold)
                except (FrameError, ConnectionError, TimeoutError, json.JSONDecodeError) as err:
                    print(f"session failed: {err}", file=sys.stderr)
                    status = 1
            if args.once:
                return status
    finally:
        listener.close()


if __name__ == "__main__":
    raise SystemExit(main())
