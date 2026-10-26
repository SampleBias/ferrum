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

#[derive(Deserialize)]
struct HeuristicV1File {
    id: String,
    base: String,
    catalog: String,
    reference_profile: String,
    #[serde(flatten)]
    thresholds: HeuristicFile,
    contended_bp: u64,
    saturated_bp: u64,
    release_bp: u64,
    batch_floor_bp: u64,
    stall_wait_us: u64,
    #[serde(default = "default_backlog_min")]
    backlog_min: u32,
}

fn default_backlog_min() -> u32 {
    1
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.join("../..");
    let cpu_path = root.join("configs/catalog-cpu-v1.json");
    let joint_path = root.join("configs/catalog-joint-v1.json");
    let heuristic_path = root.join("configs/heuristic-v0.json");
    let v1_path = root.join("configs/heuristic-v1.json");
    let v2_path = root.join("configs/heuristic-v2.json");
    for path in [&cpu_path, &joint_path, &heuristic_path, &v1_path, &v2_path] {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let cpu_bytes = fs::read(&cpu_path).expect("read cpu catalog");
    let joint_bytes = fs::read(&joint_path).expect("read joint catalog");
    let heuristic_bytes = fs::read(&heuristic_path).expect("read heuristic");
    let v1_bytes = fs::read(&v1_path).expect("read heuristic-v1");
    let v2_bytes = fs::read(&v2_path).expect("read heuristic-v2");
    let cpu: CatalogFile = serde_json::from_slice(&cpu_bytes).expect("parse cpu catalog");
    let joint: CatalogFile = serde_json::from_slice(&joint_bytes).expect("parse joint catalog");
    let heuristic: HeuristicFile =
        serde_json::from_slice(&heuristic_bytes).expect("parse heuristic");
    let v1: HeuristicV1File = serde_json::from_slice(&v1_bytes).expect("parse heuristic-v1");
    let v2: HeuristicV1File = serde_json::from_slice(&v2_bytes).expect("parse heuristic-v2");
    assert_eq!(v1.id, "heuristic-v1");
    assert_eq!(v1.base, "heuristic-v0");
    assert_eq!(v1.backlog_min, 1, "heuristic-v1 stalls on any waiting job");
    assert_eq!(v2.id, "heuristic-v2");
    assert_eq!(v2.base, "heuristic-v1");
    assert!(v2.backlog_min > 1, "heuristic-v2 requires a real backlog");
    for file in [&v1, &v2] {
        assert!(file.release_bp < file.saturated_bp, "release must sit under saturation");
        assert!(file.stall_wait_us > 0, "stall_wait_us is derived from training");
    }

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
    out.push_str(&emit_heuristic("HEURISTIC_V1", &v1));
    out.push_str(&emit_heuristic("HEURISTIC_V2", &v2));

    let dest = PathBuf::from(env::var("OUT_DIR").unwrap()).join("catalog_gen.rs");
    fs::write(dest, out).unwrap();
}

fn emit_heuristic(const_name: &str, file: &HeuristicV1File) -> String {
    let t = &file.thresholds;
    format!(
        r#"
pub const {const_name}: crate::heuristic::HeuristicV1 = crate::heuristic::HeuristicV1 {{
    base: crate::heuristic::HeuristicThresholds {{
        latency_queue_high: {lq},
        latency_wait_us_high: {lw},
        batch_queue_high: {bq},
        memory_pressure_bp: {mp},
        evictable_backlog_bytes: {eb},
    }},
    catalog: policy_types::CatalogId::{catalog},
    reference: policy_types::ProfileId::{reference},
    contended_bp: {contended},
    saturated_bp: {saturated},
    release_bp: {release},
    batch_floor_bp: {floor},
    stall_wait_us: {stall},
    backlog_min: {backlog},
}};
"#,
        lq = t.latency_queue_high,
        lw = t.latency_wait_us_high,
        bq = t.batch_queue_high,
        mp = t.memory_pressure_bp,
        eb = t.evictable_backlog_bytes,
        catalog = match file.catalog.as_str() {
            "cpu-v1" => "CpuV1",
            "joint-v1" => "JointV1",
            other => panic!("unknown catalog {other}"),
        },
        reference = profile_variant(&file.reference_profile),
        contended = file.contended_bp,
        saturated = file.saturated_bp,
        release = file.release_bp,
        floor = file.batch_floor_bp,
        stall = file.stall_wait_us,
        backlog = file.backlog_min,
    )
}

fn profile_variant(id: &str) -> &'static str {
    match id {
        "balanced" => "Balanced",
        "latency" => "Latency",
        "throughput" => "Throughput",
        "reclaim" => "Reclaim",
        other => panic!("unknown profile {other}"),
    }
}

fn emit_spec(const_name: &str, variant: &str, caps: u32, file: &CatalogFile, hash: &[u8]) -> String {
    let mut profiles = String::new();
    for profile in &file.profiles {
        let id = profile_variant(&profile.id);
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
