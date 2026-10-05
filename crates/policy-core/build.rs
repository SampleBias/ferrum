use std::env;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
struct CatalogFile {
    catalog_id: String,
    actuates: Vec<String>,
    profiles: Vec<ProfileFile>,
}

#[derive(Deserialize)]
struct ProfileFile {
    id: String,
    cpu_weights: Weights,
    cache_soft_target_bytes: u64,
    inflight_limits: Inflight,
}

#[derive(Deserialize)]
struct Weights {
    latency: u32,
    batch: u32,
    maintenance: u32,
}

#[derive(Deserialize)]
struct Inflight {
    latency: u16,
    batch: u16,
}

#[derive(Deserialize)]
struct HeuristicFile {
    latency_queue_high: u32,
    latency_wait_us_high: u64,
    batch_queue_high: u32,
    memory_pressure_bp: u16,
    evictable_backlog_bytes: u64,
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.join("../..");
    let cpu_path = root.join("configs/catalog-cpu-v1.json");
    let joint_path = root.join("configs/catalog-joint-v1.json");
    let heuristic_path = root.join("configs/heuristic-v0.json");
    for path in [&cpu_path, &joint_path, &heuristic_path] {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let cpu_bytes = fs::read(&cpu_path).expect("read cpu catalog");
    let joint_bytes = fs::read(&joint_path).expect("read joint catalog");
    let heuristic_bytes = fs::read(&heuristic_path).expect("read heuristic");
    let cpu: CatalogFile = serde_json::from_slice(&cpu_bytes).expect("parse cpu catalog");
    let joint: CatalogFile = serde_json::from_slice(&joint_bytes).expect("parse joint catalog");
    let heuristic: HeuristicFile =
        serde_json::from_slice(&heuristic_bytes).expect("parse heuristic");

    assert_eq!(cpu.catalog_id, "cpu-v1");
    assert_eq!(joint.catalog_id, "joint-v1");
    assert_eq!(cpu.actuates, ["cpu"]);
    assert_eq!(joint.actuates, ["cpu", "memory", "admission"]);
    let expected = ["balanced", "latency", "throughput", "reclaim"];
    assert_eq!(
        cpu.profiles.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        joint.profiles
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        expected
    );
    for (a, b) in cpu.profiles.iter().zip(joint.profiles.iter()) {
        assert_eq!(a.id, b.id);
        assert_eq!(a.cpu_weights.latency, b.cpu_weights.latency);
        assert_eq!(a.cpu_weights.batch, b.cpu_weights.batch);
        assert_eq!(a.cpu_weights.maintenance, b.cpu_weights.maintenance);
        assert_eq!(a.cache_soft_target_bytes, b.cache_soft_target_bytes);
        assert_eq!(a.inflight_limits.latency, b.inflight_limits.latency);
        assert_eq!(a.inflight_limits.batch, b.inflight_limits.batch);
        assert!(a.cpu_weights.latency >= 1);
        assert!(a.cpu_weights.batch >= 1);
        assert!(a.cpu_weights.maintenance >= 1);
    }

    let mut out = String::new();
    out.push_str(&emit_spec("CPU_V1", "CpuV1", 1, &cpu, &Sha256::digest(&cpu_bytes)));
    out.push_str(&emit_spec(
        "JOINT_V1",
        "JointV1",
        1 | 2 | 4,
        &joint,
        &Sha256::digest(&joint_bytes),
    ));
    out.push_str(&format!(
        r#"
pub const HEURISTIC_V0: crate::heuristic::HeuristicThresholds = crate::heuristic::HeuristicThresholds {{
    latency_queue_high: {lq},
    latency_wait_us_high: {lw},
    batch_queue_high: {bq},
    memory_pressure_bp: {mp},
    evictable_backlog_bytes: {eb},
}};
"#,
        lq = heuristic.latency_queue_high,
        lw = heuristic.latency_wait_us_high,
        bq = heuristic.batch_queue_high,
        mp = heuristic.memory_pressure_bp,
        eb = heuristic.evictable_backlog_bytes,
    ));

    let dest = PathBuf::from(env::var("OUT_DIR").unwrap()).join("catalog_gen.rs");
    fs::write(dest, out).unwrap();
}

fn emit_spec(const_name: &str, variant: &str, caps: u32, file: &CatalogFile, hash: &[u8]) -> String {
    let mut profiles = String::new();
    for profile in &file.profiles {
        let id = match profile.id.as_str() {
            "balanced" => "Balanced",
            "latency" => "Latency",
            "throughput" => "Throughput",
            "reclaim" => "Reclaim",
            other => panic!("unknown profile {other}"),
        };
        profiles.push_str(&format!(
            "ProfileParams {{ id: policy_types::ProfileId::{id}, weight_latency: {wl}, weight_batch: {wb}, weight_maintenance: {wm}, cache_soft_target_bytes: {cache}, inflight_latency: {il}, inflight_batch: {ib} }},",
            wl = profile.cpu_weights.latency,
            wb = profile.cpu_weights.batch,
            wm = profile.cpu_weights.maintenance,
            cache = profile.cache_soft_target_bytes,
            il = profile.inflight_limits.latency,
            ib = profile.inflight_limits.batch,
        ));
    }
    let hash_bytes = hash
        .iter()
        .map(|byte| format!("0x{byte:02x}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"
pub static {const_name}: CatalogSpec = CatalogSpec {{
    id: policy_types::CatalogId::{variant},
    capabilities: {caps},
    hash: policy_types::Hash32([{hash_bytes}]),
    profiles: [{profiles}],
}};
"#
    )
}
