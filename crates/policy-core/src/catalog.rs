use policy_types::{CatalogId, GroupId, Hash32, ProfileId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileParams {
    pub id: ProfileId,
    pub weight_latency: u32,
    pub weight_batch: u32,
    pub weight_maintenance: u32,
    pub cache_soft_target_bytes: u64,
    pub inflight_latency: u16,
    pub inflight_batch: u16,
}

impl ProfileParams {
    pub const fn weight(self, group: GroupId) -> u32 {
        match group {
            GroupId::Latency => self.weight_latency,
            GroupId::Batch => self.weight_batch,
            GroupId::Maintenance => self.weight_maintenance,
            GroupId::System => 0,
        }
    }

    pub const fn weights(self) -> [u32; 3] {
        [
            self.weight_latency,
            self.weight_batch,
            self.weight_maintenance,
        ]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogSpec {
    pub id: CatalogId,
    pub capabilities: u32,
    pub hash: Hash32,
    pub profiles: [ProfileParams; 4],
}

impl CatalogSpec {
    pub const fn has(self, capability: u32) -> bool {
        self.capabilities & capability != 0
    }

    pub fn profile(self, id: ProfileId) -> ProfileParams {
        for profile in self.profiles {
            if profile.id == id {
                return profile;
            }
        }
        unreachable!("every catalog contains the four named profiles");
    }
}

include!(concat!(env!("OUT_DIR"), "/catalog_gen.rs"));

pub fn catalog_spec(id: CatalogId) -> CatalogSpec {
    match id {
        CatalogId::CpuV1 => CPU_V1,
        CatalogId::JointV1 => JOINT_V1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use policy_types::{CAP_ADMISSION, CAP_CPU, CAP_MEMORY};

    #[test]
    fn catalogs_share_profile_tuples_and_differ_in_capability() {
        let cpu = catalog_spec(CatalogId::CpuV1);
        let joint = catalog_spec(CatalogId::JointV1);
        assert_eq!(cpu.profiles, joint.profiles);
        assert!(cpu.has(CAP_CPU));
        assert!(!cpu.has(CAP_MEMORY));
        assert!(!cpu.has(CAP_ADMISSION));
        assert!(joint.has(CAP_CPU | CAP_MEMORY | CAP_ADMISSION));
        assert_eq!(
            cpu.profile(ProfileId::Latency).weights(),
            [6, 2, 2]
        );
        assert_eq!(cpu.profile(ProfileId::Throughput).cache_soft_target_bytes, 100_663_296);
        assert_ne!(cpu.hash, joint.hash);
    }
}
