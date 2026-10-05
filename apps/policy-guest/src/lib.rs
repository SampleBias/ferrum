//! One guest session against the mock controller.
//!
//! The lab pre-shared key matches `configs/lab-psk.hex`. It authenticates the
//! local controller. It is not a credential for any other deployment.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use policy_core::{
    AckKind, ActivateOutcome, Capture, EngineConfig, PolicyEngine, ProposalOutcome,
};
use policy_types::{
    BootId, CatalogId, ControlPhase, Hash32, Hello, ObjectiveId, Observation, ProfileId,
    RejectReason, RunMode, SessionId, CAP_CPU, GUEST_MEMORY_BYTES, MAX_WORKLOAD_THREADS,
    TELEMETRY_VERSION,
};
use policy_wire::{
    decode_message, encode_applied, encode_hello, encode_reject, encode_shadow, encode_snapshot,
    open, seal, AppliedReport, Decoded, Direction, FrameDecoder, RejectReport, ShadowReport,
    WireError,
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

/// A proposal the shadow-phase engine recorded without staging.
///
/// `noted` is the profile in the proposal. `active` is the scheduler profile,
/// which stays at the boot profile. `generation` does not advance.
pub struct ShadowNote {
    pub noted: String,
    pub active: String,
    pub generation: u64,
}

/// Result of reporting the scheduler generation on a new session and rejecting
/// the proposal from the session that never received its acknowledgment.
pub struct Recovered {
    pub generation: u64,
    pub profile: ProfileId,
    pub reason: RejectReason,
}

/// Why an inbound controller frame was refused before it could be staged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameFault {
    Unauthenticated,
    Oversized,
    Invalid,
    Duplicate,
}

impl FrameFault {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unauthenticated => "unauthenticated",
            Self::Oversized => "oversized",
            Self::Invalid => "invalid",
            Self::Duplicate => "duplicate",
        }
    }
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

/// One snapshot while the engine stays in shadow.
///
/// The controller still sends the heuristic proposal. The engine records it
/// and does not stage it, so the boot profile and generation stay put.
pub fn exchange_shadow(
    stream: &mut TcpStream,
    boot_id: BootId,
    now_us: u64,
) -> Result<ShadowNote, SessionError> {
    let mut session = ShadowSession::open(stream, boot_id)?;
    let mut observation = Observation::quiet(1_000_000);
    observation.groups[0].queue_len = 4;
    session.note(stream, now_us, observation)
}

/// Several snapshots on one shadow session.
///
/// Each observation is sent as its own round. The engine never promotes, so
/// every note keeps the boot profile and the boot generation.
pub fn exchange_shadow_trace(
    stream: &mut TcpStream,
    boot_id: BootId,
    mut now_us: u64,
    observations: &[Observation],
) -> Result<Vec<ShadowNote>, SessionError> {
    let mut session = ShadowSession::open(stream, boot_id)?;
    let mut notes = Vec::with_capacity(observations.len());
    for observation in observations {
        notes.push(session.note(stream, now_us, *observation)?);
        now_us = now_us.saturating_add(policy_types::MIN_DWELL_US);
    }
    Ok(notes)
}

/// A controller session that records proposals and does not stage them.
pub struct ShadowSession {
    engine: PolicyEngine,
}

impl ShadowSession {
    pub fn open(stream: &mut TcpStream, boot_id: BootId) -> Result<Self, SessionError> {
        prepare(stream);
        let mut engine = PolicyEngine::boot(EngineConfig {
            catalog: CatalogId::CpuV1,
            boot_id,
            objective: ObjectiveId::MixedLatencyV1,
            approved_model: Hash32::repeat(0xcc),
            approved_calibration: Hash32::repeat(0xdd),
            run_mode: RunMode::Shadow,
            manual_rearm: false,
        });
        exchange_hello(stream, &mut engine, boot_id, RunMode::Shadow)?;
        Ok(Self { engine })
    }

    /// Send `observation` and report the proposal as a shadow note.
    pub fn note(
        &mut self,
        stream: &mut TcpStream,
        now_us: u64,
        observation: Observation,
    ) -> Result<ShadowNote, SessionError> {
        let Capture::Send { snapshot, .. } = self
            .engine
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
        let ticket = ProposalTicket {
            boot_id: snapshot.boot_id,
            session_id: proposal.session_id,
            request_seq: proposal.request_seq,
            profile: proposal.profile,
            base_generation: proposal.base_generation,
        };
        match self.engine.on_proposal(now_us, &proposal) {
            ProposalOutcome::ShadowNoted => {}
            ProposalOutcome::Rejected(reason) => {
                report_reject(stream, &ticket, reason)?;
                return Err(SessionError::Protocol(reason.as_str()));
            }
            ProposalOutcome::Staged | ProposalOutcome::Cached(_) => {
                return Err(SessionError::Protocol("proposal was not kept in shadow"));
            }
        }
        let status = self.engine.status();
        if status.phase != ControlPhase::Shadow
            || status.profile != ProfileId::Balanced
            || status.generation != snapshot.base_generation
            || status.lease_until_guest_us.is_some()
        {
            return Err(SessionError::Protocol("shadow moved the scheduler"));
        }
        let (acks, dropped) = self.engine.drain_acks();
        if dropped != 0 {
            return Err(SessionError::Protocol("shadow acknowledgment was dropped"));
        }
        let mut noted = None;
        for ack in acks.into_iter().flatten() {
            if noted.is_some() {
                return Err(SessionError::Protocol("shadow produced more than one acknowledgment"));
            }
            noted = Some(ack);
        }
        let ack = noted.ok_or(SessionError::Protocol("shadow produced no acknowledgment"))?;
        if !matches!(ack.kind, AckKind::Shadow)
            || ack.profile != proposal.profile
            || ack.generation != ack.previous_generation
            || ack.lease_until_guest_us != 0
        {
            return Err(SessionError::Protocol("shadow acknowledgment was an activation"));
        }
        let report = ShadowReport {
            boot_id: snapshot.boot_id,
            session_id: proposal.session_id,
            request_seq: proposal.request_seq,
            previous_generation: ack.previous_generation,
            generation: ack.generation,
            profile: ack.profile,
            guest_us: ack.guest_us,
            lease_until_guest_us: ack.lease_until_guest_us,
        };
        send(stream, Direction::GuestToController, &encode_shadow(&report)?)?;
        stream.flush()?;
        Ok(ShadowNote {
            noted: proposal.profile.as_str().to_string(),
            active: status.profile.as_str().to_string(),
            generation: status.generation,
        })
    }
}

/// Second session after the applied frame was lost.
///
/// `generation` and `profile` are the scheduler's values. The snapshot reports
/// them. The replayed proposal belongs to the previous session, so it is
/// rejected and the generation does not move.
pub fn recover_lost_ack(
    stream: &mut TcpStream,
    boot_id: BootId,
    now_us: u64,
    generation: u64,
    profile: ProfileId,
) -> Result<Recovered, SessionError> {
    prepare(stream);
    let mut engine = PolicyEngine::boot(EngineConfig {
        catalog: CatalogId::CpuV1,
        boot_id,
        objective: ObjectiveId::MixedLatencyV1,
        approved_model: Hash32::repeat(0xcc),
        approved_calibration: Hash32::repeat(0xdd),
        run_mode: RunMode::Mock,
        manual_rearm: false,
    });
    engine.adopt_scheduler(generation, profile);
    exchange_hello(stream, &mut engine, boot_id, RunMode::Mock)?;
    let observation = Observation::quiet(1_000_000);
    let Capture::Send { snapshot, .. } = engine
        .capture(now_us, observation)
        .map_err(|_| SessionError::Protocol("capture failed"))?
    else {
        return Err(SessionError::Protocol("snapshot was retained"));
    };
    if snapshot.base_generation != generation || snapshot.current_profile != profile {
        return Err(SessionError::Protocol("snapshot did not report the scheduler"));
    }
    send(stream, Direction::GuestToController, &encode_snapshot(&snapshot)?)?;
    let proposal = match decode_message(&recv(stream, Direction::ControllerToGuest)?)? {
        Decoded::Proposal(proposal) => proposal,
        _ => return Err(SessionError::Protocol("expected replayed proposal")),
    };
    let reason = match engine.on_proposal(now_us, &proposal) {
        ProposalOutcome::Rejected(reason) => reason,
        ProposalOutcome::Staged | ProposalOutcome::ShadowNoted | ProposalOutcome::Cached(_) => {
            return Err(SessionError::Protocol("replayed proposal was accepted"));
        }
    };
    if engine.status().generation != generation {
        return Err(SessionError::Protocol("replay moved the generation"));
    }
    report_reject(
        stream,
        &ProposalTicket {
            boot_id,
            session_id: proposal.session_id,
            request_seq: proposal.request_seq,
            profile: proposal.profile,
            base_generation: proposal.base_generation,
        },
        reason,
    )?;
    Ok(Recovered {
        generation,
        profile,
        reason,
    })
}

/// Read the four controller frames that must not become a staged profile.
///
/// Order is unauthenticated, duplicate, invalid, then oversized. None of them
/// is passed to the policy engine.
pub fn reject_bad_frames(
    stream: &mut TcpStream,
    boot_id: BootId,
    now_us: u64,
) -> Result<[FrameFault; 4], SessionError> {
    prepare(stream);
    let mut engine = PolicyEngine::boot(EngineConfig {
        catalog: CatalogId::CpuV1,
        boot_id,
        objective: ObjectiveId::MixedLatencyV1,
        approved_model: Hash32::repeat(0xcc),
        approved_calibration: Hash32::repeat(0xdd),
        run_mode: RunMode::Mock,
        manual_rearm: false,
    });
    exchange_hello(stream, &mut engine, boot_id, RunMode::Mock)?;
    let observation = Observation::quiet(1_000_000);
    let Capture::Send { snapshot, .. } = engine
        .capture(now_us, observation)
        .map_err(|_| SessionError::Protocol("capture failed"))?
    else {
        return Err(SessionError::Protocol("snapshot was retained"));
    };
    send(stream, Direction::GuestToController, &encode_snapshot(&snapshot)?)?;
    let mut decoder = FrameDecoder::new();
    let mut faults = [FrameFault::Invalid; 4];
    for fault in &mut faults {
        *fault = next_fault(stream, &mut decoder)?;
    }
    Ok(faults)
}

fn next_fault(stream: &mut TcpStream, decoder: &mut FrameDecoder) -> Result<FrameFault, SessionError> {
    loop {
        match decoder.push(&[]) {
            Ok(Some(frame)) => return classify_frame(&frame),
            Ok(None) => {}
            Err(WireError::TooLong) => return Ok(FrameFault::Oversized),
            Err(err) => return Err(SessionError::Wire(err)),
        }
        let mut buf = [0u8; 1024];
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Err(SessionError::Protocol("connection closed"));
        }
        match decoder.push(&buf[..n]) {
            Ok(Some(frame)) => return classify_frame(&frame),
            Ok(None) => continue,
            Err(WireError::TooLong) => return Ok(FrameFault::Oversized),
            Err(err) => return Err(SessionError::Wire(err)),
        }
    }
}

fn classify_frame(frame: &[u8]) -> Result<FrameFault, SessionError> {
    let payload = match open(Direction::ControllerToGuest, &LAB_KEY, frame) {
        Ok(payload) => payload,
        Err(WireError::BadMac) => return Ok(FrameFault::Unauthenticated),
        Err(err) => return Err(SessionError::Wire(err)),
    };
    match decode_message(&payload) {
        Err(WireError::DuplicateKey) => Ok(FrameFault::Duplicate),
        Err(
            WireError::Schema
            | WireError::Float
            | WireError::Utf8
            | WireError::Structure
            | WireError::OutOfRange
            | WireError::UnsupportedVersion,
        ) => Ok(FrameFault::Invalid),
        Ok(_) => Err(SessionError::Protocol("controller frame was accepted")),
        Err(err) => Err(SessionError::Wire(err)),
    }
}

fn negotiate(stream: &mut TcpStream, boot_id: BootId, now_us: u64) -> Result<Negotiated, SessionError> {
    prepare(stream);
    let mut engine = PolicyEngine::boot(EngineConfig {
        catalog: CatalogId::CpuV1,
        boot_id,
        objective: ObjectiveId::MixedLatencyV1,
        approved_model: Hash32::repeat(0xcc),
        approved_calibration: Hash32::repeat(0xdd),
        run_mode: RunMode::Mock,
        manual_rearm: false,
    });
    exchange_hello(stream, &mut engine, boot_id, RunMode::Mock)?;
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

fn prepare(stream: &mut TcpStream) {
    // Hermit rejects some POSIX socket options. The session still completes
    // without them; the host test keeps the timeouts when the calls succeed.
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
}

fn exchange_hello(
    stream: &mut TcpStream,
    engine: &mut PolicyEngine,
    boot_id: BootId,
    mode: RunMode,
) -> Result<(), SessionError> {
    let status = engine.status();
    let hello = Hello {
        boot_id,
        catalog_id: status.catalog,
        catalog_hash: status.catalog_hash,
        mode,
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
    Ok(())
}

fn send(stream: &mut TcpStream, direction: Direction, payload: &[u8]) -> Result<(), SessionError> {
    let frame = seal(direction, &LAB_KEY, payload)?;
    stream.write_all(&frame)?;
    Ok(())
}

/// What the guest decided after the proposal frame had arrived.
///
/// `decision_us` is the clock read used for that decision. A staged ticket
/// still has to be installed by the scheduler. A rejection has already been
/// reported. An abstention is not a profile and has no report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveOutcome {
    Staged {
        ticket: ProposalTicket,
        decision_us: u64,
    },
    Rejected {
        reason: RejectReason,
        profile: ProfileId,
        decision_us: u64,
    },
    Abstained {
        decision_us: u64,
    },
}

/// One controller session that can issue a proposal for every snapshot.
///
/// The engine stages each proposal that arrives inside the snapshot's
/// acceptance deadline. The caller installs it in the scheduler, then
/// [`LiveSession::commit_applied`] records the kernel's activation time so
/// the next snapshot's generation and dwell match the scheduler.
pub struct LiveSession {
    engine: PolicyEngine,
}

impl LiveSession {
    pub fn open(stream: &mut TcpStream, boot_id: BootId) -> Result<Self, SessionError> {
        Self::open_mode(stream, boot_id, RunMode::Mock)
    }

    /// Same session as [`LiveSession::open`], with hello mode `live`.
    pub fn open_live(stream: &mut TcpStream, boot_id: BootId) -> Result<Self, SessionError> {
        Self::open_mode(stream, boot_id, RunMode::Live)
    }

    fn open_mode(
        stream: &mut TcpStream,
        boot_id: BootId,
        mode: RunMode,
    ) -> Result<Self, SessionError> {
        if !mode.allows_network_activation() {
            return Err(SessionError::Protocol("mode cannot activate"));
        }
        prepare(stream);
        let mut engine = PolicyEngine::boot(EngineConfig {
            catalog: CatalogId::CpuV1,
            boot_id,
            objective: ObjectiveId::MixedLatencyV1,
            approved_model: Hash32::repeat(0xcc),
            approved_calibration: Hash32::repeat(0xdd),
            run_mode: mode,
            manual_rearm: false,
        });
        exchange_hello(stream, &mut engine, boot_id, mode)?;
        engine
            .promote_to_active()
            .map_err(|_| SessionError::Protocol("promote rejected"))?;
        Ok(Self { engine })
    }

    pub fn status(&self) -> policy_core::PolicyStatus {
        self.engine.status()
    }

    /// Send `observation` and stage the controller's proposal.
    ///
    /// `now_us` is both the capture clock and the decision clock. A caller
    /// that waited for inference must use [`LiveSession::request_judged`]
    /// and read the clock after the proposal arrives.
    pub fn request(
        &mut self,
        stream: &mut TcpStream,
        now_us: u64,
        observation: Observation,
    ) -> Result<ProposalTicket, SessionError> {
        match self.request_judged(stream, now_us, observation, || Ok(now_us))? {
            LiveOutcome::Staged { ticket, .. } => Ok(ticket),
            LiveOutcome::Rejected { reason, .. } => Err(SessionError::Protocol(reason.as_str())),
            LiveOutcome::Abstained { .. } => Err(SessionError::Protocol("abstain")),
        }
    }

    /// Capture at `captured_us`, then judge the reply with `decision_clock`.
    ///
    /// The clock runs after the frame has been received. Reusing `captured_us`
    /// would hide the time spent waiting for the controller.
    pub fn request_judged<F>(
        &mut self,
        stream: &mut TcpStream,
        captured_us: u64,
        observation: Observation,
        decision_clock: F,
    ) -> Result<LiveOutcome, SessionError>
    where
        F: FnOnce() -> Result<u64, SessionError>,
    {
        let Capture::Send { snapshot, .. } = self
            .engine
            .capture(captured_us, observation)
            .map_err(|_| SessionError::Protocol("capture failed"))?
        else {
            return Err(SessionError::Protocol("snapshot was retained"));
        };
        send(stream, Direction::GuestToController, &encode_snapshot(&snapshot)?)?;
        let frame = recv(stream, Direction::ControllerToGuest)?;
        let decision_us = decision_clock()?;
        match decode_message(&frame)? {
            Decoded::Proposal(proposal) => {
                let ticket = ProposalTicket {
                    boot_id: proposal.boot_id,
                    session_id: proposal.session_id,
                    request_seq: proposal.request_seq,
                    profile: proposal.profile,
                    base_generation: proposal.base_generation,
                };
                match self.engine.on_proposal(decision_us, &proposal) {
                    ProposalOutcome::Staged => Ok(LiveOutcome::Staged { ticket, decision_us }),
                    ProposalOutcome::Rejected(reason) => {
                        report_reject(stream, &ticket, reason)?;
                        Ok(LiveOutcome::Rejected {
                            reason,
                            profile: proposal.profile,
                            decision_us,
                        })
                    }
                    ProposalOutcome::ShadowNoted | ProposalOutcome::Cached(_) => {
                        Err(SessionError::Protocol("proposal was not staged"))
                    }
                }
            }
            Decoded::Abstain(abstain) => match self.engine.on_abstain(decision_us, &abstain) {
                ProposalOutcome::ShadowNoted | ProposalOutcome::Cached(_) => {
                    Ok(LiveOutcome::Abstained { decision_us })
                }
                ProposalOutcome::Rejected(reason) => Err(SessionError::Protocol(reason.as_str())),
                ProposalOutcome::Staged => Err(SessionError::Protocol("abstain was staged")),
            },
            _ => Err(SessionError::Protocol("expected proposal")),
        }
    }

    /// Record that the scheduler installed the staged proposal at `kernel_guest_us`.
    pub fn commit_applied(&mut self, kernel_guest_us: u64) -> Result<policy_core::Ack, SessionError> {
        match self.engine.activate(kernel_guest_us) {
            ActivateOutcome::Applied(ack) => Ok(ack),
            ActivateOutcome::Dropped(_) => Err(SessionError::Protocol("activation dropped")),
            ActivateOutcome::Idle => Err(SessionError::Protocol("nothing staged")),
        }
    }
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

    #[test]
    fn shadow_session_records_the_proposal_without_staging_it() {
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
        let port: u16 = listening.split_whitespace().nth(2).unwrap().parse().unwrap();

        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let boot = BootId::from_hex("00112233445566778899aabbccddeeff").unwrap();
        let note = exchange_shadow(&mut stream, boot, 3_000_000).unwrap();
        assert_eq!(note.noted, "latency");
        assert_eq!(note.active, "balanced");
        assert_eq!(note.generation, 1);
        drop(stream);

        let status = child.wait().unwrap();
        let mut err = String::new();
        child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
        assert!(status.success(), "controller failed: {err}");
        let log: String = lines.map(|line| line.unwrap()).collect::<Vec<_>>().join("\n");
        assert!(
            log.contains(
                "SHADOW_NOTED profile=latency generation=1 previous=1 guest_us=3000000 lease_until=0"
            ) && !log.contains("APPLIED"),
            "controller log did not keep the proposal in shadow: {log}"
        );
    }

    #[test]
    fn shadow_trace_keeps_the_boot_profile_across_mixed_v1() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut child = Command::new("python3")
            .arg("-m")
            .arg("aik_controller.server")
            .arg("--rounds")
            .arg("5")
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
        let port: u16 = listening.split_whitespace().nth(2).unwrap().parse().unwrap();

        let observations: Vec<_> = workloads::mixed_v1()
            .iter()
            .enumerate()
            .map(|(index, phase)| {
                let step = index as u64 + 1;
                workloads::measured(*phase, workloads::SAMPLE_US, [100_000 * step, 10_000 * step, 1_000 * step])
            })
            .collect();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let boot = BootId::from_hex("00112233445566778899aabbccddeeff").unwrap();
        let notes = exchange_shadow_trace(&mut stream, boot, 3_000_000, &observations).unwrap();
        let expected = ["latency", "throughput", "balanced", "reclaim", "balanced"];
        assert_eq!(notes.len(), expected.len());
        for (note, profile) in notes.iter().zip(expected) {
            assert_eq!(note.noted, profile);
            assert_eq!(note.active, "balanced");
            assert_eq!(note.generation, 1);
        }
        drop(stream);

        let status = child.wait().unwrap();
        let mut err = String::new();
        child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
        assert!(status.success(), "controller failed: {err}");
        let log: String = lines.map(|line| line.unwrap()).collect::<Vec<_>>().join("\n");
        let features: Vec<_> = log.lines().filter(|line| line.starts_with("FEATURES ")).collect();
        assert!(
            features.len() == 5 && features.iter().all(|line| line.contains("profile=balanced")),
            "snapshots left the boot profile: {log}"
        );
        assert!(
            log.contains("SHADOW_NOTED profile=latency generation=1 previous=1")
                && log.contains("SHADOW_NOTED profile=throughput generation=1 previous=1")
                && log.contains("SHADOW_NOTED profile=reclaim generation=1 previous=1")
                && log.contains("ROUNDS 5")
                && !log.contains("APPLIED"),
            "controller log staged a shadow trace: {log}"
        );
    }

    #[test]
    fn lost_ack_reconnect_reports_the_scheduler_generation() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut child = Command::new("python3")
            .arg("-m")
            .arg("aik_controller.server")
            .arg("--lose-ack")
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
        let port: u16 = listening.split_whitespace().nth(2).unwrap().parse().unwrap();
        let boot = BootId::from_hex("00112233445566778899aabbccddeeff").unwrap();

        let mut first = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let ticket = open_proposal(&mut first, boot, 3_000_000).unwrap();
        assert_eq!(ticket.base_generation, 1);
        assert_eq!(ticket.profile, ProfileId::Latency);
        drop(first);

        let mut second = None;
        for _ in 0..20 {
            if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
                second = Some(stream);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut second = second.expect("reconnect");
        second.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let recovered = recover_lost_ack(&mut second, boot, 4_000_000, 2, ProfileId::Latency).unwrap();
        assert_eq!(recovered.generation, 2);
        assert_eq!(recovered.profile, ProfileId::Latency);
        assert_eq!(recovered.reason, RejectReason::IdentityMismatch);
        drop(second);

        let status = child.wait().unwrap();
        let mut err = String::new();
        child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
        assert!(status.success(), "controller failed: {err}");
        let log: String = lines.map(|line| line.unwrap()).collect::<Vec<_>>().join("\n");
        assert!(
            log.contains("DROPPED_ACK seq=1")
                && log.contains("RECOVERED generation=2 profile=latency")
                && log.contains("REPLAY_REJECTED reason=identity_mismatch seq=1"),
            "controller log did not recover the generation: {log}"
        );
    }

    #[test]
    fn bad_frames_are_rejected_without_a_proposal() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut child = Command::new("python3")
            .arg("-m")
            .arg("aik_controller.server")
            .arg("--reject-frames")
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
        let port: u16 = listening.split_whitespace().nth(2).unwrap().parse().unwrap();
        let boot = BootId::from_hex("00112233445566778899aabbccddeeff").unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let faults = reject_bad_frames(&mut stream, boot, 3_000_000).unwrap();
        assert_eq!(
            faults,
            [
                FrameFault::Unauthenticated,
                FrameFault::Duplicate,
                FrameFault::Invalid,
                FrameFault::Oversized,
            ]
        );
        drop(stream);
        let status = child.wait().unwrap();
        let mut err = String::new();
        child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
        assert!(status.success(), "controller failed: {err}");
        let log: String = lines.map(|line| line.unwrap()).collect::<Vec<_>>().join("\n");
        assert!(
            log.contains("REJECTED_FRAMES unauthenticated duplicate invalid oversized"),
            "controller log did not list the rejected frames: {log}"
        );
    }

    #[test]
    fn mixed_v1_controller_proposes_each_phase() {
        use policy_types::MIN_DWELL_US;

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut child = Command::new("python3")
            .arg("-m")
            .arg("aik_controller.server")
            .arg("--rounds")
            .arg("5")
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
        let port: u16 = listening.split_whitespace().nth(2).unwrap().parse().unwrap();
        let boot = BootId::from_hex("00112233445566778899aabbccddeeff").unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut session = LiveSession::open(&mut stream, boot).unwrap();
        let expected = [
            ProfileId::Latency,
            ProfileId::Throughput,
            ProfileId::Balanced,
            ProfileId::Reclaim,
            ProfileId::Balanced,
        ];
        let mut now = MIN_DWELL_US;
        for (index, (phase, profile)) in workloads::mixed_v1().iter().zip(expected).enumerate() {
            let seen = [
                100_000 * (index as u64 + 1),
                10_000 * (index as u64 + 1),
                1_000 * (index as u64 + 1),
            ];
            let ticket = session
                .request(
                    &mut stream,
                    now,
                    workloads::measured(*phase, workloads::SAMPLE_US, seen),
                )
                .unwrap_or_else(|err| panic!("{} request: {err}", phase.name));
            assert_eq!(ticket.profile, profile, "{}", phase.name);
            let ack = session.commit_applied(now).unwrap();
            assert_eq!(ack.profile, profile);
            report_applied(
                &mut stream,
                &ticket,
                ack.previous_generation,
                ack.generation,
                ack.guest_us,
                ack.lease_until_guest_us,
            )
            .unwrap();
            now = now.saturating_add(MIN_DWELL_US);
        }
        drop(stream);
        let status = child.wait().unwrap();
        let mut err = String::new();
        child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
        assert!(status.success(), "controller failed: {err}");
        let log: String = lines.map(|line| line.unwrap()).collect::<Vec<_>>().join("\n");
        assert!(
            log.contains("PROPOSAL profile=latency seq=1 window_us=400000 service=100000/10000/1000")
                && log.contains(
                    "PROPOSAL profile=throughput seq=2 window_us=400000 service=200000/20000/2000"
                )
                && log.contains(
                    "PROPOSAL profile=balanced seq=3 window_us=400000 service=300000/30000/3000"
                )
                && log.contains(
                    "PROPOSAL profile=reclaim seq=4 window_us=400000 service=400000/40000/4000"
                )
                && log.contains(
                    "PROPOSAL profile=balanced seq=5 window_us=400000 service=500000/50000/5000"
                )
                && log.contains("ROUNDS 5"),
            "controller log did not follow the phases: {log}"
        );
    }

    #[test]
    fn mixed_v1_renews_inside_the_lease() {
        use policy_core::AckKind;
        use policy_types::MIN_DWELL_US;

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut child = Command::new("python3")
            .arg("-m")
            .arg("aik_controller.server")
            .arg("--rounds")
            .arg("2")
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
        let port: u16 = listening.split_whitespace().nth(2).unwrap().parse().unwrap();
        let boot = BootId::from_hex("00112233445566778899aabbccddeeff").unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut session = LiveSession::open(&mut stream, boot).unwrap();
        let phase = workloads::mixed_v1()[0];
        let now = MIN_DWELL_US;
        let ticket = session
            .request(
                &mut stream,
                now,
                workloads::measured(phase, workloads::SAMPLE_US, [11, 22, 33]),
            )
            .unwrap();
        assert_eq!(ticket.profile, ProfileId::Latency);
        let ack = session.commit_applied(now).unwrap();
        assert!(matches!(ack.kind, AckKind::Applied { renewal: false }));
        report_applied(
            &mut stream,
            &ticket,
            ack.previous_generation,
            ack.generation,
            ack.guest_us,
            ack.lease_until_guest_us,
        )
        .unwrap();
        let renew_at = now + 1_000_000;
        let ticket = session
            .request(
                &mut stream,
                renew_at,
                workloads::measured(phase, workloads::SAMPLE_US, [44, 55, 66]),
            )
            .unwrap();
        assert_eq!(ticket.profile, ProfileId::Latency);
        let ack = session.commit_applied(renew_at).unwrap();
        assert!(matches!(ack.kind, AckKind::Applied { renewal: true }));
        assert_eq!(ack.generation, 3);
        assert_eq!(ack.previous_generation, 2);
        report_applied(
            &mut stream,
            &ticket,
            ack.previous_generation,
            ack.generation,
            ack.guest_us,
            ack.lease_until_guest_us,
        )
        .unwrap();
        drop(stream);
        let status = child.wait().unwrap();
        let mut err = String::new();
        child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
        assert!(status.success(), "controller failed: {err}");
        let log: String = lines.map(|line| line.unwrap()).collect::<Vec<_>>().join("\n");
        assert!(
            log.contains("PROPOSAL profile=latency seq=1 window_us=400000 service=11/22/33")
                && log.contains("PROPOSAL profile=latency seq=2 window_us=400000 service=44/55/66")
                && log.contains("generation=3 previous=2")
                && log.contains("ROUNDS 2"),
            "controller log did not record the renewal: {log}"
        );
    }

    #[test]
    fn a_proposal_is_rejected_when_it_arrives_after_the_deadline() {
        use policy_types::{RejectReason, ACCEPTANCE_DEADLINE_US, MIN_DWELL_US};

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
        let port: u16 = listening.split_whitespace().nth(2).unwrap().parse().unwrap();
        let boot = BootId::from_hex("00112233445566778899aabbccddeeff").unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut session = LiveSession::open_live(&mut stream, boot).unwrap();
        let captured = MIN_DWELL_US;
        let outcome = session
            .request_judged(
                &mut stream,
                captured,
                workloads::measured(
                    workloads::mixed_v1()[0],
                    workloads::SAMPLE_US,
                    [100_000, 10_000, 1_000],
                ),
                || Ok(captured + ACCEPTANCE_DEADLINE_US + 50_000),
            )
            .unwrap();
        assert!(matches!(
            outcome,
            LiveOutcome::Rejected {
                reason: RejectReason::Late,
                profile: ProfileId::Latency,
                ..
            }
        ));
        let status = session.status();
        assert_eq!(status.generation, 1);
        assert_eq!(status.profile, ProfileId::Balanced);
        assert_eq!(status.lease_until_guest_us, None);
        drop(stream);
        let exit = child.wait().unwrap();
        let mut err = String::new();
        child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
        assert!(exit.success(), "controller failed: {err}");
        let log: String = lines.map(|line| line.unwrap()).collect::<Vec<_>>().join("\n");
        assert!(
            log.contains("REJECTED reason=late") && !log.contains("APPLIED"),
            "controller log staged a late proposal: {log}"
        );
    }
}
