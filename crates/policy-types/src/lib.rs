//! Fixed-size policy identifiers and telemetry for the guest and the host.
//!
//! These types are `no_std` and do not allocate. The wire codec and the policy
//! engine both consume them so a JSON parser never becomes kernel state.

#![no_std]
#![forbid(unsafe_code)]

mod ids;

pub use ids::{encode_hex, parse_hex_array, BootId, Hash32, HexError, SessionId};

/// Project protocol version. Independent of any model SDK.
pub const PROTOCOL_VERSION: u16 = 1;
/// Telemetry schema carried inside snapshots.
pub const TELEMETRY_VERSION: u16 = 1;

pub const MAX_PAYLOAD_BYTES: usize = 16_384;
pub const MAX_JSON_DEPTH: usize = 6;
pub const MAX_WORKLOAD_THREADS: u8 = 8;
pub const BASIS_POINTS_MAX: u16 = 10_000;

/// Ordinary workload slice. The scheduler model and the future kernel share this seed.
pub const QUANTUM_US: u64 = 2_000;
pub const ACCOUNTING_INTERVAL_US: u64 = 100_000;
pub const SYSTEM_RESERVATION_US: u64 = 5_000;
pub const SNAPSHOT_INTERVAL_US: u64 = 1_000_000;
/// Inclusive guest deadline: a proposal is late when `now > accept_until`.
pub const ACCEPTANCE_DEADLINE_US: u64 = 750_000;
/// Lease length measured from snapshot capture, not from reply receipt.
pub const PROFILE_LEASE_US: u64 = 3_000_000;
pub const MIN_DWELL_US: u64 = 2_000_000;

pub const GUEST_MEMORY_BYTES: u64 = 512 * 1024 * 1024;
pub const MANAGED_ARENA_BYTES: u64 = 256 * 1024 * 1024;
pub const CACHE_CAP_BYTES: u64 = 128 * 1024 * 1024;
pub const LATENCY_ARENA_CAP_BYTES: u64 = 64 * 1024 * 1024;
pub const BATCH_ARENA_CAP_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_JOB_CHARGE_BYTES: u64 = 1024 * 1024;
pub const EMERGENCY_HEADROOM_BYTES: u64 = 64 * 1024 * 1024;
pub const CLEAR_HEADROOM_BYTES: u64 = 96 * 1024 * 1024;
pub const CLEAR_HOLD_US: u64 = 3_000_000;
pub const EMERGENCY_POOL_BP: u16 = 9_000;
pub const CLEAR_POOL_BP: u16 = 7_000;

pub const CAP_CPU: u32 = 1;
pub const CAP_MEMORY: u32 = 1 << 1;
pub const CAP_ADMISSION: u32 = 1 << 2;

pub const GROUP_ORDER: [GroupId; 4] = [
    GroupId::Latency,
    GroupId::Batch,
    GroupId::Maintenance,
    GroupId::System,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum GroupId {
    Latency = 0,
    Batch = 1,
    Maintenance = 2,
    System = 3,
}

impl GroupId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Latency => "latency",
            Self::Batch => "batch",
            Self::Maintenance => "maintenance",
            Self::System => "system",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "latency" => Some(Self::Latency),
            "batch" => Some(Self::Batch),
            "maintenance" => Some(Self::Maintenance),
            "system" => Some(Self::System),
            _ => None,
        }
    }

    pub const fn is_workload(self) -> bool {
        !matches!(self, Self::System)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ProfileId {
    Balanced = 0,
    Latency = 1,
    Throughput = 2,
    Reclaim = 3,
}

impl ProfileId {
    pub const ALL: [Self; 4] = [
        Self::Balanced,
        Self::Latency,
        Self::Throughput,
        Self::Reclaim,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Balanced => "balanced",
            Self::Latency => "latency",
            Self::Throughput => "throughput",
            Self::Reclaim => "reclaim",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "balanced" => Some(Self::Balanced),
            "latency" => Some(Self::Latency),
            "throughput" => Some(Self::Throughput),
            "reclaim" => Some(Self::Reclaim),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CatalogId {
    CpuV1 = 1,
    JointV1 = 2,
}

impl CatalogId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CpuV1 => "cpu-v1",
            Self::JointV1 => "joint-v1",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cpu-v1" => Some(Self::CpuV1),
            "joint-v1" => Some(Self::JointV1),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectiveId {
    MixedLatencyV1,
}

impl ObjectiveId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MixedLatencyV1 => "mixed-latency-v1",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "mixed-latency-v1" => Some(Self::MixedLatencyV1),
            _ => None,
        }
    }
}

/// Experiment mode recorded in manifests. This is not the runtime control phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunMode {
    Baseline,
    Heuristic,
    Mock,
    Shadow,
    Live,
}

impl RunMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Heuristic => "heuristic",
            Self::Mock => "mock",
            Self::Shadow => "shadow",
            Self::Live => "live",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "baseline" => Some(Self::Baseline),
            "heuristic" => Some(Self::Heuristic),
            "mock" => Some(Self::Mock),
            "shadow" => Some(Self::Shadow),
            "live" => Some(Self::Live),
            _ => None,
        }
    }

    pub const fn allows_network_activation(self) -> bool {
        matches!(self, Self::Mock | Self::Live)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlPhase {
    Baseline,
    Shadow,
    Active,
    Degraded,
}

impl ControlPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Shadow => "shadow",
            Self::Active => "active",
            Self::Degraded => "degraded",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasonCode {
    ModelChoice,
    Heuristic,
    MockScript,
}

impl ReasonCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModelChoice => "model_choice",
            Self::Heuristic => "heuristic",
            Self::MockScript => "mock_script",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "model_choice" => Some(Self::ModelChoice),
            "heuristic" => Some(Self::Heuristic),
            "mock_script" => Some(Self::MockScript),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbstainReason {
    LowConfidence,
    OutOfDistribution,
    TruncatedInput,
    ModelBusy,
    BackendError,
}

impl AbstainReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LowConfidence => "low_confidence",
            Self::OutOfDistribution => "out_of_distribution",
            Self::TruncatedInput => "truncated_input",
            Self::ModelBusy => "model_busy",
            Self::BackendError => "backend_error",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "low_confidence" => Some(Self::LowConfidence),
            "out_of_distribution" => Some(Self::OutOfDistribution),
            "truncated_input" => Some(Self::TruncatedInput),
            "model_busy" => Some(Self::ModelBusy),
            "backend_error" => Some(Self::BackendError),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    IdentityMismatch,
    CatalogMismatch,
    SnapshotMismatch,
    UnapprovedArtifact,
    Late,
    StaleGeneration,
    UnknownProfile,
    Capability,
    Dwell,
    Emergency,
    InactiveMode,
    Busy,
    Duplicate,
    MalformedMeasurement,
    NotReady,
    RecoveryIncomplete,
    RearmRequired,
    UnsupportedVersion,
}

impl RejectReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IdentityMismatch => "identity_mismatch",
            Self::CatalogMismatch => "catalog_mismatch",
            Self::SnapshotMismatch => "snapshot_mismatch",
            Self::UnapprovedArtifact => "unapproved_artifact",
            Self::Late => "late",
            Self::StaleGeneration => "stale_generation",
            Self::UnknownProfile => "unknown_profile",
            Self::Capability => "capability",
            Self::Dwell => "dwell",
            Self::Emergency => "emergency",
            Self::InactiveMode => "inactive_mode",
            Self::Busy => "busy",
            Self::Duplicate => "duplicate",
            Self::MalformedMeasurement => "malformed_measurement",
            Self::NotReady => "not_ready",
            Self::RecoveryIncomplete => "recovery_incomplete",
            Self::RearmRequired => "rearm_required",
            Self::UnsupportedVersion => "unsupported_version",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "identity_mismatch" => Some(Self::IdentityMismatch),
            "catalog_mismatch" => Some(Self::CatalogMismatch),
            "snapshot_mismatch" => Some(Self::SnapshotMismatch),
            "unapproved_artifact" => Some(Self::UnapprovedArtifact),
            "late" => Some(Self::Late),
            "stale_generation" => Some(Self::StaleGeneration),
            "unknown_profile" => Some(Self::UnknownProfile),
            "capability" => Some(Self::Capability),
            "dwell" => Some(Self::Dwell),
            "emergency" => Some(Self::Emergency),
            "inactive_mode" => Some(Self::InactiveMode),
            "busy" => Some(Self::Busy),
            "duplicate" => Some(Self::Duplicate),
            "malformed_measurement" => Some(Self::MalformedMeasurement),
            "not_ready" => Some(Self::NotReady),
            "recovery_incomplete" => Some(Self::RecoveryIncomplete),
            "rearm_required" => Some(Self::RearmRequired),
            "unsupported_version" => Some(Self::UnsupportedVersion),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupSample {
    pub class: GroupId,
    pub queue_len: u32,
    pub runnable: u32,
    pub cpu_service_us: u64,
    pub max_wait_us: u64,
    pub wait_samples: u32,
    pub completions: u64,
    pub managed_bytes: u64,
}

impl GroupSample {
    pub const fn zero(class: GroupId) -> Self {
        Self {
            class,
            queue_len: 0,
            runnable: 0,
            cpu_service_us: 0,
            max_wait_us: 0,
            wait_samples: 0,
            completions: 0,
            managed_bytes: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pressure {
    pub managed_used_bytes: u64,
    pub managed_cap_bytes: u64,
    pub headroom_bytes: u64,
    pub evictable_backlog_bytes: u64,
    pub emergency: bool,
}

impl Pressure {
    pub const fn idle() -> Self {
        Self {
            managed_used_bytes: 0,
            managed_cap_bytes: MANAGED_ARENA_BYTES,
            headroom_bytes: 128 * 1024 * 1024,
            evictable_backlog_bytes: 0,
            emergency: false,
        }
    }
}

/// Counters supplied by the guest. Identity, deadlines, and the snapshot hash
/// are filled by the policy engine so the bridge cannot invent freshness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Observation {
    pub window_us: u64,
    pub groups: [GroupSample; 4],
    pub pressure: Pressure,
}

impl Observation {
    pub fn quiet(window_us: u64) -> Self {
        Self {
            window_us,
            groups: [
                GroupSample::zero(GroupId::Latency),
                GroupSample::zero(GroupId::Batch),
                GroupSample::zero(GroupId::Maintenance),
                GroupSample::zero(GroupId::System),
            ],
            pressure: Pressure::idle(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub boot_id: ids::BootId,
    pub session_id: ids::SessionId,
    pub request_seq: u64,
    pub catalog_id: CatalogId,
    pub catalog_hash: ids::Hash32,
    pub telemetry_version: u16,
    pub captured_guest_us: u64,
    pub window_us: u64,
    pub accept_until_guest_us: u64,
    pub lease_until_guest_us: u64,
    pub base_generation: u64,
    pub objective_id: ObjectiveId,
    pub groups: [GroupSample; 4],
    pub pressure: Pressure,
    pub current_profile: ProfileId,
    pub override_active: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proposal {
    pub boot_id: ids::BootId,
    pub session_id: ids::SessionId,
    pub request_seq: u64,
    pub base_generation: u64,
    pub catalog_id: CatalogId,
    pub catalog_hash: ids::Hash32,
    pub snapshot_hash: ids::Hash32,
    pub profile: ProfileId,
    pub answer_confidence_bp: u16,
    pub model_manifest_hash: ids::Hash32,
    pub calibration_hash: ids::Hash32,
    pub reason_code: ReasonCode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Abstain {
    pub boot_id: ids::BootId,
    pub session_id: ids::SessionId,
    pub request_seq: u64,
    pub snapshot_hash: ids::Hash32,
    pub reason: AbstainReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello {
    pub boot_id: ids::BootId,
    pub catalog_id: CatalogId,
    pub catalog_hash: ids::Hash32,
    pub mode: RunMode,
    pub capabilities: u32,
    pub max_workload_threads: u8,
    pub guest_memory_bytes: u64,
    pub telemetry_version: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelloAck {
    pub boot_id: ids::BootId,
    pub session_id: ids::SessionId,
    pub catalog_id: CatalogId,
    pub catalog_hash: ids::Hash32,
    pub model_manifest_hash: ids::Hash32,
    pub calibration_hash: ids::Hash32,
    pub ready: bool,
}

pub fn checked_deadline(capture_us: u64, budget_us: u64) -> Option<u64> {
    capture_us.checked_add(budget_us)
}
