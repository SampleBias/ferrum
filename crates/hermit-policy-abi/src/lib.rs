//! Narrow C ABI the guest bridge will use once the kernel fork exists.
//!
//! Layout is part of the contract. The kernel does not link this crate yet;
//! these checks keep the structs stable until that patch lands.

#![no_std]
#![forbid(unsafe_code)]

pub const ABI_VERSION: u16 = 1;

pub const ABI_OK: i32 = 0;
pub const ABI_CLOSED: i32 = 1;
pub const ABI_REFUSED: i32 = 2;
pub const ABI_INVALID: i32 = 3;
pub const ABI_BUSY: i32 = 4;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbiHeader {
    pub version: u16,
    pub size: u16,
}

impl AbiHeader {
    pub const fn new<T>() -> Self {
        Self {
            version: ABI_VERSION,
            size: core::mem::size_of::<T>() as u16,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegisterClassIn {
    pub header: AbiHeader,
    pub class_id: u8,
    pub _reserved: [u8; 7],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbiResult {
    pub header: AbiHeader,
    pub code: i32,
    pub _reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbiGroupSample {
    pub class_id: u8,
    pub _pad0: [u8; 3],
    pub queue_len: u32,
    pub runnable: u32,
    pub wait_samples: u32,
    pub cpu_service_us: u64,
    pub max_wait_us: u64,
    pub completions: u64,
    pub managed_bytes: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbiSnapshot {
    pub header: AbiHeader,
    pub telemetry_version: u16,
    pub group_count: u16,
    pub generation: u64,
    pub captured_guest_us: u64,
    pub window_us: u64,
    pub groups: [AbiGroupSample; 4],
    pub managed_used_bytes: u64,
    pub managed_cap_bytes: u64,
    pub headroom_bytes: u64,
    pub evictable_backlog_bytes: u64,
    pub emergency: u8,
    pub override_active: u8,
    pub current_profile: u8,
    pub _pad1: [u8; 5],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbiStageIn {
    pub header: AbiHeader,
    pub profile_id: u8,
    pub _pad0: [u8; 3],
    pub base_generation: u64,
    pub request_seq: u64,
    pub accept_until_guest_us: u64,
    pub lease_until_guest_us: u64,
    pub snapshot_hash: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbiPolicyStatus {
    pub header: AbiHeader,
    pub phase: u8,
    pub profile_id: u8,
    pub override_active: u8,
    pub _pad0: u8,
    pub generation: u64,
    pub lease_until_guest_us: u64,
    pub last_profile_change_us: u64,
    pub last_renewal_us: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbiAck {
    pub request_seq: u64,
    pub previous_generation: u64,
    pub generation: u64,
    pub guest_us: u64,
    pub lease_until_guest_us: u64,
    pub kind: u8,
    pub profile_id: u8,
    pub desired_matches_actual: u8,
    pub _pad0: u8,
    pub reason: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbiAckBatch {
    pub header: AbiHeader,
    pub count: u16,
    pub _pad0: u16,
    pub dropped: u64,
    pub records: [AbiAck; 8],
}

/// S4 joint profile staging payload (`joint-v1`: CPU weights, cache target, and
/// per-class admission limits) for freestanding applications above the kernel.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbiStageJointIn {
    pub header: AbiHeader,
    pub profile_id: u8,
    pub _pad0: u8,
    pub inflight_latency: u16,
    pub inflight_batch: u16,
    pub _pad1: u16,
    pub weight_latency: u32,
    pub weight_batch: u32,
    pub weight_maintenance: u32,
    pub cache_soft_target_bytes: u64,
    pub accept_us: u64,
    pub lease_us: u64,
    pub base_generation: u64,
}

/// Authoritative kernel-side joint policy and power-residency status read back
/// by the guest bridge or a freestanding application.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbiJointStatus {
    pub header: AbiHeader,
    pub profile_id: u8,
    pub override_active: u8,
    pub tickless_enabled: u8,
    pub _pad0: u8,
    pub inflight_latency: u16,
    pub inflight_batch: u16,
    pub weight_latency: u32,
    pub weight_batch: u32,
    pub weight_maintenance: u32,
    pub generation: u64,
    pub cache_soft_target_bytes: u64,
    pub lease_until_guest_us: u64,
    pub idle_residency_us: u64,
    pub timer_wakeups: u64,
    pub coalesced_wakeups: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    #[test]
    fn layouts_are_explicit() {
        assert_eq!(size_of::<AbiHeader>(), 4);
        assert_eq!(offset_of!(AbiGroupSample, cpu_service_us), 16);
        assert_eq!(size_of::<AbiGroupSample>(), 48);
        assert_eq!(offset_of!(AbiSnapshot, groups), 32);
        assert_eq!(size_of::<AbiSnapshot>(), 264);
        assert_eq!(offset_of!(AbiStageIn, snapshot_hash), 40);
        assert_eq!(size_of::<AbiStageIn>(), 72);
        assert_eq!(size_of::<AbiPolicyStatus>(), 40);
        assert_eq!(size_of::<AbiAck>(), 48);
        assert_eq!(size_of::<RegisterClassIn>(), 12);
        assert!(size_of::<AbiAckBatch>() > size_of::<[AbiAck; 8]>());
        assert_eq!(size_of::<AbiStageJointIn>(), 56);
        assert_eq!(offset_of!(AbiStageJointIn, cache_soft_target_bytes), 24);
        assert_eq!(size_of::<AbiJointStatus>(), 72);
        assert_eq!(offset_of!(AbiJointStatus, idle_residency_us), 48);
    }
}
