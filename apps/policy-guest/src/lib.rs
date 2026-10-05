//! One guest session against the mock controller.
//!
//! The lab pre-shared key matches `configs/lab-psk.hex`. It authenticates the
//! local controller. It is not a credential for any other deployment.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use policy_core::{ActivateOutcome, Capture, EngineConfig, PolicyEngine, ProposalOutcome};
use policy_types::{
    BootId, CatalogId, Hash32, Hello, ObjectiveId, Observation, ProfileId, RejectReason, RunMode,
    SessionId, CAP_CPU, GUEST_MEMORY_BYTES, MAX_WORKLOAD_THREADS, TELEMETRY_VERSION,
};
use policy_wire::{
    decode_message, encode_applied, encode_hello, encode_reject, encode_snapshot, seal, Direction,
    FrameDecoder, WireError, AppliedReport, Decoded, RejectReport,
};

/// 32 bytes of 0x11. Same bytes as configs/lab-psk.hex.
pub const LAB_KEY: [u8; 32] = [0x11; 32];

#[derive(Debug)]
pub enum SessionError {
    Io(std::io::Error),
    Wire(WireError),
    Protocol(&'static str),
}

impl From<std::io::Error> for SessionError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<WireError> for SessionError {
    fn from(err: WireError) -> Self {
        Self::Wire(err)
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "io: {err}"),
            Self::Wire(err) => write!(f, "wire: {err:?}"),
            Self::Protocol(msg) => write!(f, "{msg}"),
        }
    }
}

pub struct Applied {
    pub profile: String,
    pub generation: u64,
}

/// The one proposal the controller has issued for this connection.
///
/// On Hermit the scheduler acknowledgment is the `applied` or `reject` frame.
/// The host `exchange` helper still reports the userspace engine, which is the
/// reference model and has no kernel scheduler behind it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProposalTicket {
    pub boot_id: BootId,
    pub session_id: SessionId,
    pub request_seq: u64,
    pub profile: ProfileId,
    pub base_generation: u64,
}

struct Negotiated {
    ticket: ProposalTicket,
    engine: PolicyEngine,
}

pub fn open_proposal(
    stream: &mut TcpStream,
    boot_id: BootId,
    now_us: u64,
) -> Result<ProposalTicket, SessionError> {
    Ok(negotiate(stream, boot_id, now_us)?.ticket)
}

/// Report a scheduler activation. `guest_us` and `lease_until_guest_us` are
/// the kernel timer values from the acknowledgment, not the host engine clock.
pub fn report_applied(
    stream: &mut TcpStream,
    ticket: &ProposalTicket,
    previous_generation: u64,
    generation: u64,
    guest_us: u64,
    lease_until_guest_us: u64,
) -> Result<(), SessionError> {
    let report = AppliedReport {
        boot_id: ticket.boot_id,
        session_id: ticket.session_id,
        request_seq: ticket.request_seq,
        previous_generation,
        new_generation: generation,
        profile: ticket.profile,
        activated_guest_us: guest_us,
        lease_until_guest_us,
        desired_matches_actual: true,
    };
    send(stream, Direction::GuestToController, &encode_applied(&report)?)?;
    stream.flush()?;
    Ok(())
}

/// Report that the scheduler dropped the staged proposal.
pub fn report_reject(
    stream: &mut TcpStream,
    ticket: &ProposalTicket,
    reason: RejectReason,
) -> Result<(), SessionError> {
    let report = RejectReport {
        boot_id: ticket.boot_id,
        session_id: ticket.session_id,
        request_seq: ticket.request_seq,
        reason,
    };
    send(stream, Direction::GuestToController, &encode_reject(&report)?)?;
    stream.flush()?;
    Ok(())
}

pub fn exchange(stream: &mut TcpStream, boot_id: BootId, now_us: u64) -> Result<Applied, SessionError> {
    let mut negotiated = negotiate(stream, boot_id, now_us)?;
    let ActivateOutcome::Applied(ack) = negotiated.engine.activate(now_us) else {
        return Err(SessionError::Protocol("proposal was not applied"));
    };
    report_applied(
        stream,
        &negotiated.ticket,
        ack.previous_generation,
        ack.generation,
        ack.guest_us,
        ack.lease_until_guest_us,
    )?;
    Ok(Applied {
        profile: ack.profile.as_str().to_string(),
        generation: ack.generation,
    })
}

fn negotiate(stream: &mut TcpStream, boot_id: BootId, now_us: u64) -> Result<Negotiated, SessionError> {
    // Hermit rejects some POSIX socket options. The session still completes
    // without them; the host test keeps the timeouts when the calls succeed.
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let mut engine = PolicyEngine::boot(EngineConfig {
        catalog: CatalogId::CpuV1,
        boot_id,
        objective: ObjectiveId::MixedLatencyV1,
        approved_model: Hash32::repeat(0xcc),
        approved_calibration: Hash32::repeat(0xdd),
        run_mode: RunMode::Mock,
        manual_rearm: false,
    });
    let status = engine.status();
    let hello = Hello {
        boot_id,
        catalog_id: status.catalog,
        catalog_hash: status.catalog_hash,
        mode: RunMode::Mock,
        capabilities: CAP_CPU,
        max_workload_threads: MAX_WORKLOAD_THREADS,
        guest_memory_bytes: GUEST_MEMORY_BYTES,
        telemetry_version: TELEMETRY_VERSION,
    };
    send(stream, Direction::GuestToController, &encode_hello(&hello)?)?;
    let ack = match decode_message(&recv(stream, Direction::ControllerToGuest)?)? {
        Decoded::HelloAck(ack) => ack,
        _ => return Err(SessionError::Protocol("expected hello_ack")),
    };
    engine
        .accept_hello_ack(&ack)
        .map_err(|_| SessionError::Protocol("hello_ack rejected"))?;
    engine
        .promote_to_active()
        .map_err(|_| SessionError::Protocol("promote rejected"))?;

    let mut observation = Observation::quiet(1_000_000);
    observation.groups[0].queue_len = 4;
    let Capture::Send { snapshot, .. } = engine
        .capture(now_us, observation)
        .map_err(|_| SessionError::Protocol("capture failed"))?
    else {
        return Err(SessionError::Protocol("snapshot was retained"));
    };
    send(stream, Direction::GuestToController, &encode_snapshot(&snapshot)?)?;
    let proposal = match decode_message(&recv(stream, Direction::ControllerToGuest)?)? {
        Decoded::Proposal(proposal) => proposal,
        _ => return Err(SessionError::Protocol("expected proposal")),
    };
    if !matches!(engine.on_proposal(now_us, &proposal), ProposalOutcome::Staged) {
        return Err(SessionError::Protocol("proposal was not staged"));
    }
    Ok(Negotiated {
        ticket: ProposalTicket {
            boot_id,
            session_id: proposal.session_id,
            request_seq: proposal.request_seq,
            profile: proposal.profile,
            base_generation: proposal.base_generation,
        },
        engine,
    })
}

fn send(stream: &mut TcpStream, direction: Direction, payload: &[u8]) -> Result<(), SessionError> {
    let frame = seal(direction, &LAB_KEY, payload)?;
    stream.write_all(&frame)?;
    Ok(())
}

fn recv(stream: &mut TcpStream, direction: Direction) -> Result<Vec<u8>, SessionError> {
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Err(SessionError::Protocol("connection closed"));
        }
        if let Some(frame) = decoder.push(&buf[..n])? {
            return Ok(policy_wire::open(direction, &LAB_KEY, &frame)?);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use super::*;

    #[test]
    fn mock_controller_applies_latency() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut child = Command::new("python3")
            .arg("-m")
            .arg("aik_controller.server")
            .arg("--once")
            .arg("--bind")
            .arg("127.0.0.1:0")
            .arg("--root")
            .arg(&root)
            .env("PYTHONPATH", root.join("controller/src"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start mock controller");
        let stdout = child.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let listening = lines.next().unwrap().unwrap();
        let mut parts = listening.split_whitespace();
        assert_eq!(parts.next(), Some("LISTENING"));
        let _host = parts.next().unwrap();
        let port: u16 = parts.next().unwrap().parse().unwrap();

        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let boot = BootId::from_hex("00112233445566778899aabbccddeeff").unwrap();
        let applied = exchange(&mut stream, boot, 3_000_000).unwrap();
        assert_eq!(applied.profile, "latency");
        assert_eq!(applied.generation, 2);
        drop(stream);

        let status = child.wait().unwrap();
        let mut err = String::new();
        child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
        assert!(status.success(), "controller failed: {err}");
        let log: String = lines.map(|line| line.unwrap()).collect::<Vec<_>>().join("\n");
        assert!(
            log.contains(
                "APPLIED profile=latency generation=2 previous=1 guest_us=3000000 lease_until=6000000"
            ),
            "controller log did not record the activation clock: {log}"
        );
    }
}
