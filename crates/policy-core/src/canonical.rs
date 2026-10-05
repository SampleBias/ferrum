//! Canonical snapshot encoding. The hash covers these bytes, not a reserialization
//! of a received frame. Field order is the schema order in the protocol document.

use policy_types::{
    encode_hex, CatalogId, GroupId, GroupSample, Pressure, ProfileId, Snapshot,
    GROUP_ORDER, PROTOCOL_VERSION,
};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanonicalError {
    Buffer,
    GroupOrder,
}

struct Writer<'a> {
    buf: &'a mut [u8],
    len: usize,
    overflow: bool,
}

impl Writer<'_> {
    fn push(&mut self, text: &str) {
        self.push_bytes(text.as_bytes());
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        if self.overflow || self.len + bytes.len() > self.buf.len() {
            self.overflow = true;
            return;
        }
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }

    fn u64(&mut self, value: u64) {
        let mut tmp = [0u8; 20];
        let digits = format_u64(value, &mut tmp);
        self.push_bytes(digits);
    }

    fn bool(&mut self, value: bool) {
        self.push(if value { "true" } else { "false" });
    }

    fn hex(&mut self, bytes: &[u8]) {
        let mut tmp = [0u8; 64];
        encode_hex(bytes, &mut tmp[..bytes.len() * 2]);
        self.push_bytes(&tmp[..bytes.len() * 2]);
    }
}

fn format_u64(value: u64, tmp: &mut [u8; 20]) -> &[u8] {
    if value == 0 {
        tmp[0] = b'0';
        return &tmp[..1];
    }
    let mut n = value;
    let mut i = tmp.len();
    while n > 0 {
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    &tmp[i..]
}

pub fn write_snapshot_canonical(snap: &Snapshot, out: &mut [u8]) -> Result<usize, CanonicalError> {
    for (sample, expected) in snap.groups.iter().zip(GROUP_ORDER) {
        if sample.class != expected {
            return Err(CanonicalError::GroupOrder);
        }
    }
    let mut w = Writer {
        buf: out,
        len: 0,
        overflow: false,
    };
    w.push("{\"protocol\":");
    w.u64(u64::from(PROTOCOL_VERSION));
    w.push(",\"kind\":\"snapshot\",\"boot_id\":\"");
    w.hex(&snap.boot_id.0);
    w.push("\",\"session_id\":\"");
    w.hex(&snap.session_id.0);
    w.push("\",\"request_seq\":");
    w.u64(snap.request_seq);
    w.push(",\"catalog_id\":\"");
    w.push(snap.catalog_id.as_str());
    w.push("\",\"catalog_hash\":\"");
    w.hex(&snap.catalog_hash.0);
    w.push("\",\"telemetry_version\":");
    w.u64(u64::from(snap.telemetry_version));
    w.push(",\"captured_guest_us\":");
    w.u64(snap.captured_guest_us);
    w.push(",\"window_us\":");
    w.u64(snap.window_us);
    w.push(",\"accept_until_guest_us\":");
    w.u64(snap.accept_until_guest_us);
    w.push(",\"lease_until_guest_us\":");
    w.u64(snap.lease_until_guest_us);
    w.push(",\"base_generation\":");
    w.u64(snap.base_generation);
    w.push(",\"objective_id\":\"");
    w.push(snap.objective_id.as_str());
    w.push("\",\"groups\":[");
    for (index, sample) in snap.groups.iter().enumerate() {
        if index > 0 {
            w.push(",");
        }
        write_group(&mut w, sample);
    }
    w.push("],\"pressure\":");
    write_pressure(&mut w, &snap.pressure);
    w.push(",\"current_profile\":\"");
    w.push(snap.current_profile.as_str());
    w.push("\",\"override_active\":");
    w.bool(snap.override_active);
    w.push("}");
    if w.overflow {
        return Err(CanonicalError::Buffer);
    }
    Ok(w.len)
}

fn write_group(w: &mut Writer<'_>, sample: &GroupSample) {
    w.push("{\"class\":\"");
    w.push(sample.class.as_str());
    w.push("\",\"queue_len\":");
    w.u64(u64::from(sample.queue_len));
    w.push(",\"runnable\":");
    w.u64(u64::from(sample.runnable));
    w.push(",\"cpu_service_us\":");
    w.u64(sample.cpu_service_us);
    w.push(",\"max_wait_us\":");
    w.u64(sample.max_wait_us);
    w.push(",\"wait_samples\":");
    w.u64(u64::from(sample.wait_samples));
    w.push(",\"completions\":");
    w.u64(sample.completions);
    w.push(",\"managed_bytes\":");
    w.u64(sample.managed_bytes);
    w.push("}");
}

fn write_pressure(w: &mut Writer<'_>, pressure: &Pressure) {
    w.push("{\"managed_used_bytes\":");
    w.u64(pressure.managed_used_bytes);
    w.push(",\"managed_cap_bytes\":");
    w.u64(pressure.managed_cap_bytes);
    w.push(",\"headroom_bytes\":");
    w.u64(pressure.headroom_bytes);
    w.push(",\"evictable_backlog_bytes\":");
    w.u64(pressure.evictable_backlog_bytes);
    w.push(",\"emergency\":");
    w.bool(pressure.emergency);
    w.push("}");
}

pub fn hash_snapshot(snap: &Snapshot) -> Result<policy_types::Hash32, CanonicalError> {
    let mut buf = [0u8; 4096];
    let len = write_snapshot_canonical(snap, &mut buf)?;
    let digest = Sha256::digest(&buf[..len]);
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&digest);
    Ok(policy_types::Hash32(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use policy_types::{
        BootId, GroupSample, Hash32, ObjectiveId, Pressure, SessionId, Snapshot, TELEMETRY_VERSION,
    };

    fn fixture_snapshot() -> Snapshot {
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
    fn canonical_snapshot_matches_cross_language_fixture() {
        let expected = include_str!("../../../tests/protocol-vectors/snapshot.canonical.json");
        let expected = expected.trim_end_matches('\n');
        let mut buf = [0u8; 4096];
        let n = write_snapshot_canonical(&fixture_snapshot(), &mut buf).unwrap();
        let encoded = core::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(encoded, expected);
        let hash = hash_snapshot(&fixture_snapshot()).unwrap();
        assert_eq!(
            hash.to_string(),
            "c8c5552a73e7bda9b8486d34cc93f643b5e8254b6ba8615f6bd06b38a4c76245"
        );
    }
}
