//! Host controller for the frozen pending-jobs threshold.
//!
//! It speaks the same authenticated frames as the Python controller and
//! proposes `latency` or `reclaim` from the latency queue length.

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use policy_core::{
    catalog_spec, choose_heuristic, choose_pending_threshold, evaluate_distilled,
    DISTILLED_JOINT_V1, HEURISTIC_V0, PENDING_THRESHOLD_V0,
};
use policy_types::{
    Abstain, AbstainReason, CatalogId, GroupId, Hash32, Observation, Proposal,
    ReasonCode, SessionId, Snapshot, TELEMETRY_VERSION,
};
use policy_wire::{
    decode_message, encode_abstain, encode_hello_ack, encode_proposal, open, seal, Decoded,
    Direction, FrameDecoder, WireError,
};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControllerPolicy {
    PendingThreshold,
    DistilledJoint,
    Heuristic,
}

struct Lab {
    key: [u8; 32],
    catalog: CatalogId,
    catalog_hash: Hash32,
    model_manifest_hash: Hash32,
    calibration_hash: Hash32,
    policy: ControllerPolicy,
}

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(1)
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let mut bind = "127.0.0.1:7777".to_string();
    let mut root = PathBuf::from(".");
    let mut catalog = CatalogId::CpuV1;
    let mut policy = ControllerPolicy::PendingThreshold;
    let mut once = false;
    let mut follow = false;
    let mut rounds = 0u32;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--once" {
            once = true;
        } else if arg == "--follow" {
            follow = true;
        } else if let Some(value) = arg.strip_prefix("--bind=") {
            bind = value.to_string();
        } else if let Some(value) = arg.strip_prefix("--root=") {
            root = PathBuf::from(value);
        } else if let Some(value) = arg.strip_prefix("--catalog=") {
            catalog = CatalogId::parse(value).ok_or_else(|| format!("bad catalog {value}"))?;
        } else if let Some(value) = arg.strip_prefix("--policy=") {
            policy = match value {
                "pending-threshold" => ControllerPolicy::PendingThreshold,
                "distilled-joint" => ControllerPolicy::DistilledJoint,
                "heuristic" => ControllerPolicy::Heuristic,
                _ => return Err(format!("bad policy {value}")),
            };
        } else if let Some(value) = arg.strip_prefix("--rounds=") {
            rounds = value.parse().map_err(|_| format!("bad rounds {value}"))?;
            if rounds == 0 {
                return Err("rounds must be positive".into());
            }
        } else {
            return Err(format!("unknown argument {arg}"));
        }
        index += 1;
    }
    if follow && rounds > 0 {
        return Err("follow and rounds are different sessions".into());
    }
    let lab = load_lab(&root, catalog, policy)?;
    let addr: SocketAddr = bind.parse().map_err(|_| format!("bad bind {bind}"))?;
    let listener = TcpListener::bind(addr).map_err(|err| format!("bind {addr}: {err}"))?;
    let bound = listener.local_addr().map_err(|err| err.to_string())?;
    println!("LISTENING {} {}", bound.ip(), bound.port());
    match policy {
        ControllerPolicy::PendingThreshold => {
            println!("CONTROLLER pending-threshold cut=14.5 below=latency above=reclaim");
        }
        ControllerPolicy::DistilledJoint => {
            println!(
                "CONTROLLER distilled-joint catalog={} cut=14.5 min_conf_bp={}",
                catalog.as_str(),
                DISTILLED_JOINT_V1.min_confidence_bp
            );
        }
        ControllerPolicy::Heuristic => {
            println!("CONTROLLER heuristic-v0 catalog={}", catalog.as_str());
        }
    }
    if follow || rounds > 0 || once {
        let mut stream = accept(&listener)?;
        return serve(&mut stream, &lab, if follow { u32::MAX } else { rounds.max(1) });
    }
    loop {
        let mut stream = accept(&listener)?;
        serve(&mut stream, &lab, 1)?;
    }
}

fn accept(listener: &TcpListener) -> Result<TcpStream, String> {
    let (stream, _) = listener.accept().map_err(|err| format!("accept: {err}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .map_err(|err| err.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(60)))
        .map_err(|err| err.to_string())?;
    Ok(stream)
}

fn load_lab(root: &PathBuf, catalog: CatalogId, policy: ControllerPolicy) -> Result<Lab, String> {
    let key_text = fs::read_to_string(root.join("configs/lab-psk.hex"))
        .map_err(|err| format!("lab key: {err}"))?;
    let key_bytes = parse_hex(key_text.trim()).map_err(|_| "lab key must be 32 bytes".to_string())?;
    let key: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| "lab key must be 32 bytes".to_string())?;
    #[derive(Deserialize)]
    struct Identity {
        model_manifest_hash: String,
        calibration_hash: String,
    }
    let identity: Identity = serde_json::from_str(
        &fs::read_to_string(root.join("configs/lab-identity.json"))
            .map_err(|err| format!("lab identity: {err}"))?,
    )
    .map_err(|err| format!("lab identity: {err}"))?;
    let spec = catalog_spec(catalog);
    Ok(Lab {
        key,
        catalog: spec.id,
        catalog_hash: spec.hash,
        model_manifest_hash: Hash32::from_hex(&identity.model_manifest_hash)
            .map_err(|_| "bad model manifest hash".to_string())?,
        calibration_hash: Hash32::from_hex(&identity.calibration_hash)
            .map_err(|_| "bad calibration hash".to_string())?,
        policy,
    })
}

fn snapshot_observation(snapshot: &Snapshot) -> Observation {
    Observation {
        window_us: snapshot.window_us,
        groups: snapshot.groups,
        pressure: snapshot.pressure,
    }
}

fn serve(stream: &mut TcpStream, lab: &Lab, rounds: u32) -> Result<(), String> {
    let mut link = Link {
        stream,
        decoder: FrameDecoder::new(),
        key: lab.key,
    };
    let hello = match link.recv(Direction::GuestToController)? {
        Decoded::Hello(hello) => hello,
        _ => return Err("rejected hello".into()),
    };
    if hello.catalog_id != lab.catalog
        || hello.catalog_hash != lab.catalog_hash
        || hello.telemetry_version != TELEMETRY_VERSION
    {
        return Err("rejected catalog".into());
    }
    let session_id = SessionId::new(random_bytes());
    println!("HELLO boot={boot} session={session_id}", boot = hello.boot_id);
    let ack = policy_types::HelloAck {
        boot_id: hello.boot_id,
        session_id,
        catalog_id: lab.catalog,
        catalog_hash: lab.catalog_hash,
        model_manifest_hash: lab.model_manifest_hash,
        calibration_hash: lab.calibration_hash,
        ready: true,
    };
    link.send(Direction::ControllerToGuest, &encode_hello_ack(&ack).map_err(wire)?)?;
    let mut done = 0u32;
    loop {
        let snapshot = match link.recv(Direction::GuestToController) {
            Ok(Decoded::Snapshot(snapshot)) => snapshot,
            Ok(_) => return Err("expected snapshot".into()),
            Err(err) if rounds == u32::MAX && done > 0 && err == "connection closed" => break,
            Err(err) => return Err(err),
        };
        if snapshot.boot_id != hello.boot_id || snapshot.session_id != session_id {
            return Err("snapshot identity mismatch".into());
        }
        if snapshot.catalog_id != lab.catalog || snapshot.catalog_hash != lab.catalog_hash {
            return Err("snapshot catalog mismatch".into());
        }
        let pending = queue_len(&snapshot, GroupId::Latency);
        let hash = policy_core::hash_snapshot(&snapshot).map_err(|_| "snapshot hash".to_string())?;
        let obs = snapshot_observation(&snapshot);
        let (profile, conf_bp, reason_code, abstain) = match lab.policy {
            ControllerPolicy::PendingThreshold => (
                choose_pending_threshold(pending, &PENDING_THRESHOLD_V0),
                10_000,
                ReasonCode::Heuristic,
                false,
            ),
            ControllerPolicy::Heuristic => (
                choose_heuristic(&obs, &HEURISTIC_V0),
                10_000,
                ReasonCode::Heuristic,
                false,
            ),
            ControllerPolicy::DistilledJoint => {
                let d = evaluate_distilled(&obs, &DISTILLED_JOINT_V1);
                println!(
                    "EXPLAIN seq={} controller=distilled-joint choice={} regime={} confidence_bp={} margin_jobs={:+.1} wait_margin_ms={:+.1} transient_spike={}",
                    snapshot.request_seq,
                    d.profile.as_str(),
                    d.regime.as_str(),
                    d.confidence_bp,
                    f64::from(d.margin_jobs_times_two) / 2.0,
                    d.wait_margin_us as f64 / 1000.0,
                    d.transient_spike_absorbed
                );
                (
                    d.profile,
                    d.confidence_bp,
                    ReasonCode::ModelChoice,
                    d.abstain_low_confidence,
                )
            }
        };
        if abstain {
            let msg = Abstain {
                boot_id: snapshot.boot_id,
                session_id,
                request_seq: snapshot.request_seq,
                snapshot_hash: hash,
                reason: AbstainReason::LowConfidence,
            };
            println!(
                "ABSTAIN reason=low_confidence seq={} confidence_bp={}",
                snapshot.request_seq, conf_bp
            );
            link.send(
                Direction::ControllerToGuest,
                &encode_abstain(&msg).map_err(wire)?,
            )?;
            done += 1;
            if rounds != u32::MAX && done >= rounds {
                break;
            }
            continue;
        }
        let proposal = Proposal {
            boot_id: snapshot.boot_id,
            session_id,
            request_seq: snapshot.request_seq,
            base_generation: snapshot.base_generation,
            catalog_id: lab.catalog,
            catalog_hash: lab.catalog_hash,
            snapshot_hash: hash,
            profile,
            answer_confidence_bp: conf_bp,
            model_manifest_hash: lab.model_manifest_hash,
            calibration_hash: lab.calibration_hash,
            reason_code,
        };
        println!(
            "PROPOSAL profile={} seq={} window_us={} service={} pending={}",
            profile.as_str(),
            snapshot.request_seq,
            snapshot.window_us,
            service_line(&snapshot),
            pending
        );
        link.send(
            Direction::ControllerToGuest,
            &encode_proposal(&proposal).map_err(wire)?,
        )?;
        match link.recv(Direction::GuestToController)? {
            Decoded::Applied(report) => {
                if report.boot_id != hello.boot_id
                    || report.session_id != session_id
                    || report.request_seq != snapshot.request_seq
                    || report.profile != profile
                {
                    return Err("applied report mismatch".into());
                }
                println!(
                    "APPLIED profile={} generation={}",
                    report.profile.as_str(),
                    report.new_generation
                );
            }
            Decoded::Reject(report) => {
                if report.boot_id != hello.boot_id
                    || report.session_id != session_id
                    || report.request_seq != snapshot.request_seq
                {
                    return Err("reject report mismatch".into());
                }
                println!("REJECT reason={}", report.reason.as_str());
            }
            _ => return Err("expected scheduler report".into()),
        }
        done += 1;
        if rounds != u32::MAX && done >= rounds {
            break;
        }
    }
    Ok(())
}

struct Link<'a> {
    stream: &'a mut TcpStream,
    decoder: FrameDecoder,
    key: [u8; 32],
}

impl Link<'_> {
    fn recv(&mut self, direction: Direction) -> Result<Decoded, String> {
        let payload = self.recv_payload(direction)?;
        decode_message(&payload).map_err(wire)
    }

    fn recv_payload(&mut self, direction: Direction) -> Result<Vec<u8>, String> {
        let mut buf = [0u8; 2048];
        loop {
            if let Some(frame) = self.decoder.push(&[]).map_err(wire)? {
                return open(direction, &self.key, &frame).map_err(wire);
            }
            let n = self.stream.read(&mut buf).map_err(|err| format!("read: {err}"))?;
            if n == 0 {
                return Err("connection closed".into());
            }
            if let Some(frame) = self.decoder.push(&buf[..n]).map_err(wire)? {
                return open(direction, &self.key, &frame).map_err(wire);
            }
        }
    }

    fn send(&mut self, direction: Direction, payload: &[u8]) -> Result<(), String> {
        let frame = seal(direction, &self.key, payload).map_err(wire)?;
        self.stream
            .write_all(&frame)
            .map_err(|err| format!("write: {err}"))
    }
}

fn queue_len(snapshot: &Snapshot, class: GroupId) -> u32 {
    snapshot
        .groups
        .iter()
        .find(|group| group.class == class)
        .map(|group| group.queue_len)
        .unwrap_or(0)
}

fn service_line(snapshot: &Snapshot) -> String {
    snapshot
        .groups
        .iter()
        .take(3)
        .map(|group| group.cpu_service_us.to_string())
        .collect::<Vec<_>>()
        .join("/")
}

fn random_bytes() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    let mut file = fs::File::open("/dev/urandom").expect("urandom");
    file.read_exact(&mut bytes).expect("urandom");
    bytes
}

fn parse_hex(text: &str) -> Result<Vec<u8>, ()> {
    if text.len() % 2 != 0 {
        return Err(());
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).map_err(|_| ()))
        .collect()
}

fn wire(err: WireError) -> String {
    format!("wire {err:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use policy_types::{
        BootId, GroupSample, Hello, ObjectiveId, Pressure, ProfileId, RunMode, PROTOCOL_VERSION,
    };
    use policy_wire::{encode_applied, encode_hello, encode_snapshot, AppliedReport};
    use std::thread;

    fn lab() -> Lab {
        Lab {
            key: [0x11; 32],
            catalog: CatalogId::CpuV1,
            catalog_hash: catalog_spec(CatalogId::CpuV1).hash,
            model_manifest_hash: Hash32::repeat(0xcc),
            calibration_hash: Hash32::repeat(0xdd),
            policy: ControllerPolicy::PendingThreshold,
        }
    }

    fn snapshot(session: SessionId, pending: u32, seq: u64) -> Snapshot {
        let mut groups = [
            GroupSample::zero(GroupId::Latency),
            GroupSample::zero(GroupId::Batch),
            GroupSample::zero(GroupId::Maintenance),
            GroupSample::zero(GroupId::System),
        ];
        groups[0].queue_len = pending;
        Snapshot {
            boot_id: BootId::new([0xab; 16]),
            session_id: session,
            request_seq: seq,
            catalog_id: CatalogId::CpuV1,
            catalog_hash: catalog_spec(CatalogId::CpuV1).hash,
            telemetry_version: TELEMETRY_VERSION,
            captured_guest_us: 1_000,
            window_us: 1_000_000,
            accept_until_guest_us: 2_000,
            lease_until_guest_us: 4_000,
            base_generation: 1,
            objective_id: ObjectiveId::MixedLatencyV1,
            groups,
            pressure: Pressure::idle(),
            current_profile: ProfileId::Balanced,
            override_active: false,
        }
    }

    #[test]
    fn one_round_proposes_reclaim_above_the_cut() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let lab = lab();
        let server = thread::spawn(move || {
            let mut stream = accept(&listener).unwrap();
            serve(&mut stream, &lab, 1).unwrap();
        });
        let mut client = TcpStream::connect(addr).unwrap();
        let key = [0x11; 32];
        let hello = Hello {
            boot_id: BootId::new([0xab; 16]),
            catalog_id: CatalogId::CpuV1,
            catalog_hash: catalog_spec(CatalogId::CpuV1).hash,
            mode: RunMode::Mock,
            capabilities: 1,
            max_workload_threads: 8,
            guest_memory_bytes: 512 << 20,
            telemetry_version: TELEMETRY_VERSION,
        };
        let _ = PROTOCOL_VERSION;
        write_frame(&mut client, Direction::GuestToController, &key, &encode_hello(&hello).unwrap());
        let ack = read_decoded(&mut client, Direction::ControllerToGuest, &key);
        let Decoded::HelloAck(ack) = ack else { panic!("hello ack") };
        let snap = snapshot(ack.session_id, 56, 3);
        write_frame(
            &mut client,
            Direction::GuestToController,
            &key,
            &encode_snapshot(&snap).unwrap(),
        );
        let proposal = read_decoded(&mut client, Direction::ControllerToGuest, &key);
        let Decoded::Proposal(proposal) = proposal else { panic!("proposal") };
        assert_eq!(proposal.profile, ProfileId::Reclaim);
        assert_eq!(proposal.request_seq, 3);
        assert_eq!(proposal.reason_code, ReasonCode::Heuristic);
        let report = AppliedReport {
            boot_id: hello.boot_id,
            session_id: ack.session_id,
            request_seq: 3,
            previous_generation: 1,
            new_generation: 2,
            profile: ProfileId::Reclaim,
            activated_guest_us: 1_100,
            lease_until_guest_us: 4_100,
            desired_matches_actual: true,
        };
        write_frame(
            &mut client,
            Direction::GuestToController,
            &key,
            &encode_applied(&report).unwrap(),
        );
        server.join().unwrap();
    }

    #[test]
    fn nine_pending_jobs_propose_latency() {
        assert_eq!(
            choose_pending_threshold(9, &PENDING_THRESHOLD_V0),
            ProfileId::Latency
        );
    }

    #[test]
    fn joint_v1_distilled_proposes_throughput_under_confirmation_backlog() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let joint = catalog_spec(CatalogId::JointV1);
        let lab = Lab {
            key: [0x11; 32],
            catalog: CatalogId::JointV1,
            catalog_hash: joint.hash,
            model_manifest_hash: Hash32::repeat(0xcc),
            calibration_hash: Hash32::repeat(0xdd),
            policy: ControllerPolicy::DistilledJoint,
        };
        let server = thread::spawn(move || {
            let mut stream = accept(&listener).unwrap();
            serve(&mut stream, &lab, 1).unwrap();
        });
        let mut client = TcpStream::connect(addr).unwrap();
        let key = [0x11; 32];
        let hello = Hello {
            boot_id: BootId::new([0xab; 16]),
            catalog_id: CatalogId::JointV1,
            catalog_hash: joint.hash,
            mode: RunMode::Live,
            capabilities: joint.capabilities,
            max_workload_threads: 8,
            guest_memory_bytes: 512 << 20,
            telemetry_version: TELEMETRY_VERSION,
        };
        write_frame(
            &mut client,
            Direction::GuestToController,
            &key,
            &encode_hello(&hello).unwrap(),
        );
        let Decoded::HelloAck(ack) = read_decoded(&mut client, Direction::ControllerToGuest, &key)
        else {
            panic!("hello ack")
        };
        let mut snap = snapshot(ack.session_id, 0, 1);
        snap.catalog_id = CatalogId::JointV1;
        snap.catalog_hash = joint.hash;
        snap.groups[1].queue_len = 10;
        write_frame(
            &mut client,
            Direction::GuestToController,
            &key,
            &encode_snapshot(&snap).unwrap(),
        );
        let Decoded::Proposal(proposal) =
            read_decoded(&mut client, Direction::ControllerToGuest, &key)
        else {
            panic!("proposal")
        };
        assert_eq!(proposal.profile, ProfileId::Throughput);
        assert_eq!(proposal.reason_code, ReasonCode::ModelChoice);
        assert!(proposal.answer_confidence_bp >= DISTILLED_JOINT_V1.min_confidence_bp);
        let report = AppliedReport {
            boot_id: hello.boot_id,
            session_id: ack.session_id,
            request_seq: 1,
            previous_generation: 1,
            new_generation: 2,
            profile: ProfileId::Throughput,
            activated_guest_us: 1_100,
            lease_until_guest_us: 4_100,
            desired_matches_actual: true,
        };
        write_frame(
            &mut client,
            Direction::GuestToController,
            &key,
            &encode_applied(&report).unwrap(),
        );
        server.join().unwrap();
    }

    fn write_frame(stream: &mut TcpStream, direction: Direction, key: &[u8; 32], payload: &[u8]) {
        stream.write_all(&seal(direction, key, payload).unwrap()).unwrap();
    }

    fn read_decoded(stream: &mut TcpStream, direction: Direction, key: &[u8; 32]) -> Decoded {
        let mut decoder = FrameDecoder::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read(&mut buf).unwrap();
            if let Some(frame) = decoder.push(&buf[..n]).unwrap() {
                let payload = open(direction, key, &frame).unwrap();
                return decode_message(&payload).unwrap();
            }
        }
    }
}
