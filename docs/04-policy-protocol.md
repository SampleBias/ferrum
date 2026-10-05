# Policy protocol and activation contract

This is the proposed project protocol, not Laya's native API. The host adapter translates between it and the pinned SDK. Version it independently so kernel code does not depend on Python response-field changes.

## Transport and limits

Use one guest-initiated TCP connection over virtio-net to the host controller. Initial host endpoint: `127.0.0.1:7777`; guest endpoint under the selected QEMU user network: `10.0.2.2:7777`. Validate this mapping in G1. A negotiated protocol connection carries `hello`, `snapshot`, `proposal`, `abstain`, `applied`, `shadow`, and `reject` messages.

Proposed frame: 4-byte unsigned big-endian payload length, that many UTF-8 JSON bytes, then a 32-byte HMAC-SHA256 tag. Maximum payload: 16,384 bytes; maximum nesting depth: 6. Authenticate the direction marker, length bytes, and exact transmitted payload bytes. Use separate direction markers to prevent reflecting a guest message as a controller response. JSON is not reserialized before verification.

Use one per-experiment pre-shared key, provisioned in the guest image and in a mode-0600 host file. Do not put it in serial logs or command-line arguments. This authenticates the controller for a trusted local lab; it does not provide confidentiality or protection from another privileged participant in the same guest/host trust domain. A remote controller deployment needs an authenticated encrypted transport or a secured tunnel.

The bridge verifies framing/authentication in ordinary thread context, then decodes into fixed-size typed fields. Reject duplicate keys, unknown fields, unsupported versions, excessive lengths, invalid UTF-8, floats where integers are required, and out-of-range values. Never cast network bytes to a Rust struct. The policy core receives a small decoded proposal and performs its own semantic validation.

At most one snapshot is in flight, one newest unsent snapshot is retained, and one validated proposal may occupy the activation mailbox. New telemetry replaces stale unsent telemetry; it does not build an unbounded queue. A retained snapshot keeps the guest time at which it was captured. If that deadline has already passed when the snapshot would be sent, it is dropped. Partial-frame reads have deadlines and use preallocated buffers. Repeated malformed frames close the connection with rate-limited diagnostics.

## Negotiation

The guest `hello` carries protocol version, boot ID, supported catalog ID/hash, active mode, capability bits, hard limits, and telemetry schema version. The host replies with matching protocol/catalog, its controller session ID, model identity, calibration identity, and readiness. The guest compares these against experiment configuration; a reply cannot expand capability.

Generate a fresh 128-bit boot ID for every VM launch through the trusted harness. Reconnect creates a new controller session ID and clears outstanding requests. Do not rely on a monotonically increasing request number alone across reboot. Expire any active lease normally if the controller disconnects; reconnect does not renew it.

## Snapshot contract

Required fields:

| Field | Meaning |
| --- | --- |
| `protocol`, `kind` | Exact protocol version and message discriminator |
| `boot_id`, `session_id`, `request_seq` | Replay and request identity |
| `catalog_id`, `catalog_hash`, `telemetry_version` | Shared meaning of features and actions |
| `captured_guest_us`, `window_us` | Guest monotonic sample time and observation duration |
| `accept_until_guest_us`, `lease_until_guest_us` | Guest-authoritative deadlines for this request |
| `base_generation` | Active profile generation on which this observation was made |
| `objective_id` | Operator-selected immutable optimization objective |
| `groups` | Bounded per-class queue, CPU-service, wait-time, completion, and managed-memory counters |
| `pressure`, `current_profile`, `override_active` | Resource state and control context |
| `snapshot_hash` | SHA-256 of the canonical model-relevant snapshot representation |

Define exact integer units: microseconds, bytes, counts, and basis points (0–10,000). Percentiles carry a sample count; “no samples” is represented explicitly rather than as a false zero. Counters and time conversions use checked arithmetic. The snapshot hash uses a documented canonical encoding of fields in schema order; the host verifies it using cross-language golden vectors. Transport authentication still covers the original frame bytes.

The kernel records the pending snapshot metadata locally. It does not trust a host's echoed timestamps to establish freshness. A transient thread block/wakeup does not invalidate a class-level profile, but catalog changes, boot changes, and local emergency generation changes do.

## Proposal example

The JSON below is an illustrative payload with synthetic identities and hashes; it is not an authentic frame or a packet to send to a running guest.

```json
{
  "protocol": 1,
  "kind": "proposal",
  "boot_id": "0123456789abcdef0123456789abcdef",
  "session_id": "abcdef0123456789abcdef0123456789",
  "request_seq": 42,
  "base_generation": 7,
  "catalog_id": "joint-v1",
  "catalog_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "snapshot_hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "profile": "latency",
  "answer_confidence_bp": 9400,
  "model_manifest_hash": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
  "calibration_hash": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
  "reason_code": "model_choice"
}
```

The response deliberately contains no writable TTL, arbitrary weights, memory addresses, task IDs, or numeric resource limits. The guest resolves the profile against its compiled catalog. Confidence is diagnostic metadata after host calibration, not an independent capability.

An `abstain` message identifies the same request and has an enumerated reason such as `low_confidence`, `out_of_distribution`, `truncated_input`, `model_busy`, `backend_error`, or `expired`. Abstention never renews the active lease. Low-confidence choice labels may be logged on the host, but must not be hidden inside an actionable proposal.

## Validation and staging

Perform checks in this order:

1. Verify frame bounds, authentication, schema, and primitive types.
2. Match boot/session/request identity to the sole locally pending request.
3. Match catalog, snapshot hash, and experiment-approved model/calibration identity.
4. Check current guest monotonic time against locally recorded acceptance deadline. The live trial reads that clock after the proposal arrives. The capture timestamp is not reused, so time spent waiting on the controller counts against the snapshot's original deadline.
5. Require the current generation to equal `base_generation`; a local override or another activation makes the request stale.
6. Resolve the label to the immutable profile and recheck capability and resource invariants.
7. Apply mode, emergency, and dwell rules. A different profile during dwell is rejected; an identical fresh profile may renew the lease. Track `last_profile_change` separately from `last_renewal` so renewals cannot perpetually restart the dwell interval.
8. Copy the validated command into the fixed-capacity pending slot; consume the request so duplicates cannot activate twice.

If the slot is unavailable, return `busy` and keep the existing active profile. Avoid partially changing any field while validating. The kernel API is narrow: `stage_proposal`, `read_snapshot`, and `read_policy_status`; class registration is a separate startup-only interface.

## Atomic activation and acknowledgment

On the single-vCPU prototype, the scheduler owns active policy state. The bridge stages a copy while holding a short synchronization guard. At the next scheduling entry, with the appropriate local interrupt/scheduler exclusion, the scheduler accounts the outgoing execution interval under its old weight, then rechecks generation, deadline, and emergency state. It swaps the entire fixed-size desired profile and lease metadata, increments generation, and writes an acknowledgment record to a bounded ring.

This ordering prevents a memory-pressure override between staging and activation from being overwritten by stale inference. No network call, heap allocation, or JSON parse occurs inside the critical section. For the first version, reject stale base generations rather than attempt optimistic merging.

The bridge later sends `applied` with request ID, previous/new generation, selected profile, activation guest timestamp, lease expiry, and desired-versus-actual convergence flags. The host must distinguish `received`, `validated`, `applied`, and `converged`. A printed model choice is not any of those guest events.

While the guest is still in the shadow phase it sends `shadow` instead. That frame names the proposed profile and the guest timestamp, and it keeps `generation` equal to `previous_generation` with `lease_until_guest_us` at 0. The scheduler profile stays at boot. The host logs the note and does not treat it as an activation.

If an acknowledgment is lost, the next snapshot/status reports the authoritative generation. Retransmitting a consumed request produces a duplicate rejection or an idempotent cached status; it cannot extend the lease. Identical-profile renewals still increment generation and record a new accepted request.

## Expiry and local overrides

At expiry, a scheduling entry installs balanced fallback unless an independent emergency override remains in force, increments generation, clears staged proposals, and records `lease_expired`. Arrange a timer so expiry is checked even when the next ordinary quantum/wakeup would occur later. The check must not need a successful network read or a runnable bridge thread.

Emergency memory handling and operator disable take priority over model activation. Local override installation increments generation. Pressure recovery uses deterministic thresholds and hysteresis, then returns to shadow/baseline before a fresh active proposal. A profile lease never overrides an emergency state.

## Required contract tests

Use golden Rust/Python frames, partial-read tests, invalid authentication, maximum lengths, unknown enums, duplicates, reconnects, reboot replay, delayed responses, and generation races. Exercise rejection immediately before and during activation. Fuzz decoding and semantic validation on the host; reproduce representative failures in QEMU. These tests validate an actual trust boundary and state machine rather than mirroring trivial getters.
