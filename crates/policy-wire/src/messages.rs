use serde::Deserialize;
use serde_json::json;

use policy_core::{hash_snapshot, write_snapshot_canonical};
use policy_types::{
    encode_hex, Abstain, AbstainReason, BootId, CatalogId, GroupId, GroupSample, Hash32, Hello,
    HelloAck, ObjectiveId, Pressure, ProfileId, Proposal, ReasonCode, RejectReason, RunMode,
    SessionId, Snapshot, BASIS_POINTS_MAX, GROUP_ORDER, PROTOCOL_VERSION, TELEMETRY_VERSION,
};

use crate::json::validate_json;
use crate::WireError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedReport {
    pub boot_id: BootId,
    pub session_id: SessionId,
    pub request_seq: u64,
    pub previous_generation: u64,
    pub new_generation: u64,
    pub profile: ProfileId,
    pub activated_guest_us: u64,
    pub lease_until_guest_us: u64,
    pub desired_matches_actual: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectReport {
    pub boot_id: BootId,
    pub session_id: SessionId,
    pub request_seq: u64,
    pub reason: RejectReason,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decoded {
    Hello(Hello),
    HelloAck(HelloAck),
    Snapshot(Snapshot),
    Proposal(Proposal),
    Abstain(Abstain),
    Applied(AppliedReport),
    Reject(RejectReport),
}

pub fn decode_message(bytes: &[u8]) -> Result<Decoded, WireError> {
    let text = std::str::from_utf8(bytes).map_err(|_| WireError::Utf8)?;
    validate_json(text)?;
    let value: WireMessage = serde_json::from_str(text).map_err(|_| WireError::Schema)?;
    value.into_decoded()
}

pub fn encode_hello(hello: &Hello) -> Result<Vec<u8>, WireError> {
    Ok(serde_json::to_vec(&json!({
        "protocol": PROTOCOL_VERSION,
        "kind": "hello",
        "boot_id": hello.boot_id.to_string(),
        "catalog_id": hello.catalog_id.as_str(),
        "catalog_hash": hello.catalog_hash.to_string(),
        "mode": hello.mode.as_str(),
        "capabilities": hello.capabilities,
        "max_workload_threads": hello.max_workload_threads,
        "guest_memory_bytes": hello.guest_memory_bytes,
        "telemetry_version": hello.telemetry_version,
    }))
    .map_err(|_| WireError::Schema)?)
}

pub fn encode_hello_ack(ack: &HelloAck) -> Result<Vec<u8>, WireError> {
    Ok(serde_json::to_vec(&json!({
        "protocol": PROTOCOL_VERSION,
        "kind": "hello_ack",
        "boot_id": ack.boot_id.to_string(),
        "session_id": ack.session_id.to_string(),
        "catalog_id": ack.catalog_id.as_str(),
        "catalog_hash": ack.catalog_hash.to_string(),
        "model_manifest_hash": ack.model_manifest_hash.to_string(),
        "calibration_hash": ack.calibration_hash.to_string(),
        "ready": ack.ready,
    }))
    .map_err(|_| WireError::Schema)?)
}

pub fn encode_snapshot(snapshot: &Snapshot) -> Result<Vec<u8>, WireError> {
    let hash = hash_snapshot(snapshot).map_err(|_| WireError::Buffer)?;
    let mut body = [0u8; 4096];
    let len = write_snapshot_canonical(snapshot, &mut body).map_err(|_| WireError::Buffer)?;
    if len == 0 || body[len - 1] != b'}' {
        return Err(WireError::Buffer);
    }
    let mut out = Vec::with_capacity(len + 80);
    out.extend_from_slice(&body[..len - 1]);
    out.extend_from_slice(b",\"snapshot_hash\":\"");
    let mut hex = [0u8; 64];
    encode_hex(&hash.0, &mut hex);
    out.extend_from_slice(&hex);
    out.extend_from_slice(b"\"}");
    Ok(out)
}

pub fn encode_proposal(proposal: &Proposal) -> Result<Vec<u8>, WireError> {
    Ok(serde_json::to_vec(&json!({
        "protocol": PROTOCOL_VERSION,
        "kind": "proposal",
        "boot_id": proposal.boot_id.to_string(),
        "session_id": proposal.session_id.to_string(),
        "request_seq": proposal.request_seq,
        "base_generation": proposal.base_generation,
        "catalog_id": proposal.catalog_id.as_str(),
        "catalog_hash": proposal.catalog_hash.to_string(),
        "snapshot_hash": proposal.snapshot_hash.to_string(),
        "profile": proposal.profile.as_str(),
        "answer_confidence_bp": proposal.answer_confidence_bp,
        "model_manifest_hash": proposal.model_manifest_hash.to_string(),
        "calibration_hash": proposal.calibration_hash.to_string(),
        "reason_code": proposal.reason_code.as_str(),
    }))
    .map_err(|_| WireError::Schema)?)
}

pub fn encode_abstain(abstain: &Abstain) -> Result<Vec<u8>, WireError> {
    Ok(serde_json::to_vec(&json!({
        "protocol": PROTOCOL_VERSION,
        "kind": "abstain",
        "boot_id": abstain.boot_id.to_string(),
        "session_id": abstain.session_id.to_string(),
        "request_seq": abstain.request_seq,
        "snapshot_hash": abstain.snapshot_hash.to_string(),
        "reason": abstain.reason.as_str(),
    }))
    .map_err(|_| WireError::Schema)?)
}

pub fn encode_applied(report: &AppliedReport) -> Result<Vec<u8>, WireError> {
    Ok(serde_json::to_vec(&json!({
        "protocol": PROTOCOL_VERSION,
        "kind": "applied",
        "boot_id": report.boot_id.to_string(),
        "session_id": report.session_id.to_string(),
        "request_seq": report.request_seq,
        "previous_generation": report.previous_generation,
        "new_generation": report.new_generation,
        "profile": report.profile.as_str(),
        "activated_guest_us": report.activated_guest_us,
        "lease_until_guest_us": report.lease_until_guest_us,
        "desired_matches_actual": report.desired_matches_actual,
    }))
    .map_err(|_| WireError::Schema)?)
}

pub fn encode_reject(report: &RejectReport) -> Result<Vec<u8>, WireError> {
    Ok(serde_json::to_vec(&json!({
        "protocol": PROTOCOL_VERSION,
        "kind": "reject",
        "boot_id": report.boot_id.to_string(),
        "session_id": report.session_id.to_string(),
        "request_seq": report.request_seq,
        "reason": report.reason.as_str(),
    }))
    .map_err(|_| WireError::Schema)?)
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WireMessage {
    Hello {
        protocol: u16,
        boot_id: String,
        catalog_id: String,
        catalog_hash: String,
        mode: String,
        capabilities: u32,
        max_workload_threads: u8,
        guest_memory_bytes: u64,
        telemetry_version: u16,
    },
    HelloAck {
        protocol: u16,
        boot_id: String,
        session_id: String,
        catalog_id: String,
        catalog_hash: String,
        model_manifest_hash: String,
        calibration_hash: String,
        ready: bool,
    },
    Snapshot {
        protocol: u16,
        boot_id: String,
        session_id: String,
        request_seq: u64,
        catalog_id: String,
        catalog_hash: String,
        telemetry_version: u16,
        captured_guest_us: u64,
        window_us: u64,
        accept_until_guest_us: u64,
        lease_until_guest_us: u64,
        base_generation: u64,
        objective_id: String,
        groups: Vec<GroupDto>,
        pressure: PressureDto,
        current_profile: String,
        override_active: bool,
        snapshot_hash: String,
    },
    Proposal {
        protocol: u16,
        boot_id: String,
        session_id: String,
        request_seq: u64,
        base_generation: u64,
        catalog_id: String,
        catalog_hash: String,
        snapshot_hash: String,
        profile: String,
        answer_confidence_bp: u16,
        model_manifest_hash: String,
        calibration_hash: String,
        reason_code: String,
    },
    Abstain {
        protocol: u16,
        boot_id: String,
        session_id: String,
        request_seq: u64,
        snapshot_hash: String,
        reason: String,
    },
    Applied {
        protocol: u16,
        boot_id: String,
        session_id: String,
        request_seq: u64,
        previous_generation: u64,
        new_generation: u64,
        profile: String,
        activated_guest_us: u64,
        lease_until_guest_us: u64,
        desired_matches_actual: bool,
    },
    Reject {
        protocol: u16,
        boot_id: String,
        session_id: String,
        request_seq: u64,
        reason: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupDto {
    class: String,
    queue_len: u32,
    runnable: u32,
    cpu_service_us: u64,
    max_wait_us: u64,
    wait_samples: u32,
    completions: u64,
    managed_bytes: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PressureDto {
    managed_used_bytes: u64,
    managed_cap_bytes: u64,
    headroom_bytes: u64,
    evictable_backlog_bytes: u64,
    emergency: bool,
}

impl WireMessage {
    fn into_decoded(self) -> Result<Decoded, WireError> {
        match self {
            Self::Hello {
                protocol,
                boot_id,
                catalog_id,
                catalog_hash,
                mode,
                capabilities,
                max_workload_threads,
                guest_memory_bytes,
                telemetry_version,
            } => {
                check_version(protocol)?;
                Ok(Decoded::Hello(Hello {
                    boot_id: parse_boot(&boot_id)?,
                    catalog_id: CatalogId::parse(&catalog_id).ok_or(WireError::Schema)?,
                    catalog_hash: parse_hash(&catalog_hash)?,
                    mode: RunMode::parse(&mode).ok_or(WireError::Schema)?,
                    capabilities,
                    max_workload_threads,
                    guest_memory_bytes,
                    telemetry_version,
                }))
            }
            Self::HelloAck {
                protocol,
                boot_id,
                session_id,
                catalog_id,
                catalog_hash,
                model_manifest_hash,
                calibration_hash,
                ready,
            } => {
                check_version(protocol)?;
                Ok(Decoded::HelloAck(HelloAck {
                    boot_id: parse_boot(&boot_id)?,
                    session_id: parse_session(&session_id)?,
                    catalog_id: CatalogId::parse(&catalog_id).ok_or(WireError::Schema)?,
                    catalog_hash: parse_hash(&catalog_hash)?,
                    model_manifest_hash: parse_hash(&model_manifest_hash)?,
                    calibration_hash: parse_hash(&calibration_hash)?,
                    ready,
                }))
            }
            Self::Snapshot {
                protocol,
                boot_id,
                session_id,
                request_seq,
                catalog_id,
                catalog_hash,
                telemetry_version,
                captured_guest_us,
                window_us,
                accept_until_guest_us,
                lease_until_guest_us,
                base_generation,
                objective_id,
                groups,
                pressure,
                current_profile,
                override_active,
                snapshot_hash,
            } => {
                check_version(protocol)?;
                if telemetry_version != TELEMETRY_VERSION {
                    return Err(WireError::UnsupportedVersion);
                }
                if groups.len() != GROUP_ORDER.len() {
                    return Err(WireError::Schema);
                }
                let mut decoded_groups = [GroupSample::zero(GroupId::System); 4];
                for (index, group) in groups.into_iter().enumerate() {
                    let class = GroupId::parse(&group.class).ok_or(WireError::Schema)?;
                    if class != GROUP_ORDER[index] {
                        return Err(WireError::Schema);
                    }
                    decoded_groups[index] = GroupSample {
                        class,
                        queue_len: group.queue_len,
                        runnable: group.runnable,
                        cpu_service_us: group.cpu_service_us,
                        max_wait_us: group.max_wait_us,
                        wait_samples: group.wait_samples,
                        completions: group.completions,
                        managed_bytes: group.managed_bytes,
                    };
                }
                let snapshot = Snapshot {
                    boot_id: parse_boot(&boot_id)?,
                    session_id: parse_session(&session_id)?,
                    request_seq,
                    catalog_id: CatalogId::parse(&catalog_id).ok_or(WireError::Schema)?,
                    catalog_hash: parse_hash(&catalog_hash)?,
                    telemetry_version,
                    captured_guest_us,
                    window_us,
                    accept_until_guest_us,
                    lease_until_guest_us,
                    base_generation,
                    objective_id: ObjectiveId::parse(&objective_id).ok_or(WireError::Schema)?,
                    groups: decoded_groups,
                    pressure: Pressure {
                        managed_used_bytes: pressure.managed_used_bytes,
                        managed_cap_bytes: pressure.managed_cap_bytes,
                        headroom_bytes: pressure.headroom_bytes,
                        evictable_backlog_bytes: pressure.evictable_backlog_bytes,
                        emergency: pressure.emergency,
                    },
                    current_profile: ProfileId::parse(&current_profile).ok_or(WireError::Schema)?,
                    override_active,
                };
                let actual = hash_snapshot(&snapshot).map_err(|_| WireError::Buffer)?;
                if actual != parse_hash(&snapshot_hash)? {
                    return Err(WireError::SnapshotHash);
                }
                Ok(Decoded::Snapshot(snapshot))
            }
            Self::Proposal {
                protocol,
                boot_id,
                session_id,
                request_seq,
                base_generation,
                catalog_id,
                catalog_hash,
                snapshot_hash,
                profile,
                answer_confidence_bp,
                model_manifest_hash,
                calibration_hash,
                reason_code,
            } => {
                check_version(protocol)?;
                if answer_confidence_bp > BASIS_POINTS_MAX {
                    return Err(WireError::OutOfRange);
                }
                Ok(Decoded::Proposal(Proposal {
                    boot_id: parse_boot(&boot_id)?,
                    session_id: parse_session(&session_id)?,
                    request_seq,
                    base_generation,
                    catalog_id: CatalogId::parse(&catalog_id).ok_or(WireError::Schema)?,
                    catalog_hash: parse_hash(&catalog_hash)?,
                    snapshot_hash: parse_hash(&snapshot_hash)?,
                    profile: ProfileId::parse(&profile).ok_or(WireError::Schema)?,
                    answer_confidence_bp,
                    model_manifest_hash: parse_hash(&model_manifest_hash)?,
                    calibration_hash: parse_hash(&calibration_hash)?,
                    reason_code: ReasonCode::parse(&reason_code).ok_or(WireError::Schema)?,
                }))
            }
            Self::Abstain {
                protocol,
                boot_id,
                session_id,
                request_seq,
                snapshot_hash,
                reason,
            } => {
                check_version(protocol)?;
                Ok(Decoded::Abstain(Abstain {
                    boot_id: parse_boot(&boot_id)?,
                    session_id: parse_session(&session_id)?,
                    request_seq,
                    snapshot_hash: parse_hash(&snapshot_hash)?,
                    reason: AbstainReason::parse(&reason).ok_or(WireError::Schema)?,
                }))
            }
            Self::Applied {
                protocol,
                boot_id,
                session_id,
                request_seq,
                previous_generation,
                new_generation,
                profile,
                activated_guest_us,
                lease_until_guest_us,
                desired_matches_actual,
            } => {
                check_version(protocol)?;
                Ok(Decoded::Applied(AppliedReport {
                    boot_id: parse_boot(&boot_id)?,
                    session_id: parse_session(&session_id)?,
                    request_seq,
                    previous_generation,
                    new_generation,
                    profile: ProfileId::parse(&profile).ok_or(WireError::Schema)?,
                    activated_guest_us,
                    lease_until_guest_us,
                    desired_matches_actual,
                }))
            }
            Self::Reject {
                protocol,
                boot_id,
                session_id,
                request_seq,
                reason,
            } => {
                check_version(protocol)?;
                Ok(Decoded::Reject(RejectReport {
                    boot_id: parse_boot(&boot_id)?,
                    session_id: parse_session(&session_id)?,
                    request_seq,
                    reason: RejectReason::parse(&reason).ok_or(WireError::Schema)?,
                }))
            }
        }
    }
}

fn check_version(protocol: u16) -> Result<(), WireError> {
    if protocol != PROTOCOL_VERSION {
        Err(WireError::UnsupportedVersion)
    } else {
        Ok(())
    }
}

fn parse_boot(text: &str) -> Result<BootId, WireError> {
    BootId::from_hex(text).map_err(|_| WireError::Schema)
}

fn parse_session(text: &str) -> Result<SessionId, WireError> {
    SessionId::from_hex(text).map_err(|_| WireError::Schema)
}

fn parse_hash(text: &str) -> Result<Hash32, WireError> {
    Hash32::from_hex(text).map_err(|_| WireError::Schema)
}

#[cfg(test)]
mod tests {
    use super::*;
    use policy_types::{Observation, Pressure};

    fn sample_snapshot() -> Snapshot {
        let mut groups = [
            GroupSample::zero(GroupId::Latency),
            GroupSample::zero(GroupId::Batch),
            GroupSample::zero(GroupId::Maintenance),
            GroupSample::zero(GroupId::System),
        ];
        groups[0].queue_len = 3;
        groups[0].runnable = 1;
        groups[1].runnable = 1;
        groups[3].runnable = 1;
        Snapshot {
            boot_id: BootId::from_hex("0123456789abcdef0123456789abcdef").unwrap(),
            session_id: SessionId::from_hex("abcdef0123456789abcdef0123456789").unwrap(),
            request_seq: 42,
            catalog_id: CatalogId::CpuV1,
            catalog_hash: Hash32::repeat(0xaa),
            telemetry_version: TELEMETRY_VERSION,
            captured_guest_us: 1_000,
            window_us: 1_000_000,
            accept_until_guest_us: 751_000,
            lease_until_guest_us: 3_001_000,
            base_generation: 1,
            objective_id: ObjectiveId::MixedLatencyV1,
            groups,
            pressure: Pressure {
                managed_used_bytes: 0,
                managed_cap_bytes: 268_435_456,
                headroom_bytes: 134_217_728,
                evictable_backlog_bytes: 0,
                emergency: false,
            },
            current_profile: ProfileId::Balanced,
            override_active: false,
        }
    }

    #[test]
    fn snapshot_roundtrip_checks_the_content_hash() {
        let snapshot = sample_snapshot();
        let bytes = encode_snapshot(&snapshot).unwrap();
        match decode_message(&bytes).unwrap() {
            Decoded::Snapshot(decoded) => assert_eq!(decoded, snapshot),
            other => panic!("unexpected {other:?}"),
        }
        let mut tampered = bytes.clone();
        let hash_at = tampered.windows(4).position(|window| window == b"aaaa").unwrap();
        tampered[hash_at] = b'b';
        assert_eq!(decode_message(&tampered), Err(WireError::SnapshotHash));
        let _ = Observation::quiet(1);
    }

    #[test]
    fn unknown_fields_and_bad_profiles_are_schema_errors() {
        let raw = include_bytes!("../../../tests/protocol-vectors/proposal.json");
        let end = raw.iter().rposition(|byte| *byte != b'\n').unwrap() + 1;
        let text = std::str::from_utf8(&raw[..end]).unwrap();
        match decode_message(text.as_bytes()).unwrap() {
            Decoded::Proposal(proposal) => {
                assert_eq!(proposal.profile, ProfileId::Latency);
                assert_eq!(proposal.answer_confidence_bp, 9400);
            }
            other => panic!("unexpected {other:?}"),
        }
        let extra = text.replacen("\"profile\"", "\"ttl\":1,\"profile\"", 1);
        assert_eq!(decode_message(extra.as_bytes()), Err(WireError::Schema));
        let unknown = text.replace("\"latency\"", "\"turbo\"");
        assert_eq!(decode_message(unknown.as_bytes()), Err(WireError::Schema));
    }
}
