//! Policy state machine. Rust accepts or rejects a named profile. Confidence
//! never changes that decision. A lease is measured from snapshot capture.

use policy_types::{
    checked_deadline, Abstain, CatalogId, ControlPhase, Hash32, HelloAck, ObjectiveId, Observation,
    ProfileId, Proposal, RejectReason, RunMode, SessionId, Snapshot, ACCEPTANCE_DEADLINE_US,
    CAP_ADMISSION, CAP_MEMORY, GROUP_ORDER, MIN_DWELL_US, PROFILE_LEASE_US, TELEMETRY_VERSION,
};

use crate::canonical::hash_snapshot;
use crate::catalog::{catalog_spec, CatalogSpec};

const ACK_CAP: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineConfig {
    pub catalog: CatalogId,
    pub boot_id: policy_types::BootId,
    pub objective: ObjectiveId,
    pub approved_model: Hash32,
    pub approved_calibration: Hash32,
    pub run_mode: RunMode,
    pub manual_rearm: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyError {
    Overflow,
    NoSession,
    BadObservation,
    NotEmergency,
    Reject(RejectReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capture {
    Send { snapshot: Snapshot, hash: Hash32 },
    Retained,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProposalOutcome {
    Staged,
    ShadowNoted,
    Cached(Ack),
    Rejected(RejectReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivateOutcome {
    Idle,
    Applied(Ack),
    Dropped(RejectReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollOutcome {
    None,
    Expired(Ack),
    Suppressed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AckKind {
    Applied { renewal: bool },
    Shadow,
    Rejected(RejectReason),
    LeaseExpired,
    Emergency,
    Fallback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ack {
    pub kind: AckKind,
    pub request_seq: u64,
    pub previous_generation: u64,
    pub generation: u64,
    pub profile: ProfileId,
    pub guest_us: u64,
    pub lease_until_guest_us: u64,
    pub desired_matches_actual: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PolicyStatus {
    pub phase: ControlPhase,
    pub profile: ProfileId,
    pub generation: u64,
    pub lease_until_guest_us: Option<u64>,
    pub override_active: bool,
    pub last_profile_change_us: u64,
    pub last_renewal_us: u64,
    pub catalog: CatalogId,
    pub catalog_hash: Hash32,
    pub shadow_streak: u8,
    pub desired_cache_bytes: u64,
    pub desired_inflight_latency: u16,
    pub desired_inflight_batch: u16,
    pub weights: [u32; 3],
}

#[derive(Clone, Copy)]
struct Pending {
    snapshot: Snapshot,
    hash: Hash32,
}

#[derive(Clone, Copy)]
struct Staged {
    proposal: Proposal,
    accept_until: u64,
    lease_until: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConsumedKind {
    Proposal,
    Abstain,
}

#[derive(Clone, Copy)]
struct Consumed {
    session: SessionId,
    seq: u64,
    snapshot_hash: Hash32,
    kind: ConsumedKind,
    profile: ProfileId,
    ack: Ack,
    settled: bool,
}

#[derive(Clone, Copy)]
struct Domains {
    memory: bool,
    admission: bool,
}

pub struct PolicyEngine {
    config: EngineConfig,
    spec: CatalogSpec,
    phase: ControlPhase,
    profile: ProfileId,
    generation: u64,
    lease_until: Option<u64>,
    last_profile_change_us: u64,
    last_renewal_us: u64,
    emergency: bool,
    weights: [u32; 3],
    desired_cache: u64,
    desired_inflight_latency: u16,
    desired_inflight_batch: u16,
    cache_used: u64,
    session: Option<SessionId>,
    next_seq: u64,
    inflight: Option<Pending>,
    unsent: Option<Observation>,
    staged: Option<Staged>,
    shadow_streak: u8,
    rearm_required: bool,
    manual_satisfied: bool,
    last_consumed: Option<Consumed>,
    acks: [Ack; ACK_CAP],
    ack_head: usize,
    ack_len: usize,
    acks_dropped: u64,
}

impl PolicyEngine {
    pub fn boot(config: EngineConfig) -> Self {
        let spec = catalog_spec(config.catalog);
        let mut engine = Self {
            config,
            spec,
            phase: ControlPhase::Baseline,
            profile: ProfileId::Balanced,
            generation: 1,
            lease_until: None,
            last_profile_change_us: 0,
            last_renewal_us: 0,
            emergency: false,
            weights: [1, 1, 1],
            desired_cache: 0,
            desired_inflight_latency: 0,
            desired_inflight_batch: 0,
            cache_used: 0,
            session: None,
            next_seq: 1,
            inflight: None,
            unsent: None,
            staged: None,
            shadow_streak: 0,
            rearm_required: false,
            manual_satisfied: false,
            last_consumed: None,
            acks: [blank_ack(); ACK_CAP],
            ack_head: 0,
            ack_len: 0,
            acks_dropped: 0,
        };
        engine.actuate(
            ProfileId::Balanced,
            Domains {
                memory: true,
                admission: true,
            },
        );
        engine
    }

    pub fn status(&self) -> PolicyStatus {
        PolicyStatus {
            phase: self.phase,
            profile: self.profile,
            generation: self.generation,
            lease_until_guest_us: self.lease_until,
            override_active: self.emergency,
            last_profile_change_us: self.last_profile_change_us,
            last_renewal_us: self.last_renewal_us,
            catalog: self.config.catalog,
            catalog_hash: self.spec.hash,
            shadow_streak: self.shadow_streak,
            desired_cache_bytes: self.desired_cache,
            desired_inflight_latency: self.desired_inflight_latency,
            desired_inflight_batch: self.desired_inflight_batch,
            weights: self.weights,
        }
    }

    pub fn report_cache_used(&mut self, bytes: u64) {
        self.cache_used = bytes;
    }

    pub fn accept_hello_ack(&mut self, ack: &HelloAck) -> Result<(), RejectReason> {
        if !ack.ready {
            return Err(RejectReason::NotReady);
        }
        if ack.boot_id != self.config.boot_id {
            return Err(RejectReason::IdentityMismatch);
        }
        if ack.catalog_id != self.config.catalog || ack.catalog_hash != self.spec.hash {
            return Err(RejectReason::CatalogMismatch);
        }
        if ack.model_manifest_hash != self.config.approved_model
            || ack.calibration_hash != self.config.approved_calibration
        {
            return Err(RejectReason::UnapprovedArtifact);
        }
        let reconnect = self.session.is_some() && self.session != Some(ack.session_id);
        self.session = Some(ack.session_id);
        self.inflight = None;
        self.unsent = None;
        self.shadow_streak = 0;
        if reconnect && self.phase == ControlPhase::Active {
            self.phase = ControlPhase::Shadow;
            self.rearm_required = true;
        } else if matches!(self.phase, ControlPhase::Baseline | ControlPhase::Degraded) {
            self.phase = ControlPhase::Shadow;
        }
        Ok(())
    }

    pub fn controller_lost(&mut self) {
        self.session = None;
        self.inflight = None;
        self.unsent = None;
    }

    pub fn note_controller_healthy(&mut self) {
        if self.phase == ControlPhase::Degraded && !self.emergency {
            self.phase = ControlPhase::Shadow;
            self.shadow_streak = 0;
        }
    }

    pub fn acknowledge_rearm(&mut self) {
        self.manual_satisfied = true;
    }

    pub fn promote_to_active(&mut self) -> Result<(), RejectReason> {
        if !self.config.run_mode.allows_network_activation() {
            return Err(RejectReason::InactiveMode);
        }
        if self.emergency {
            return Err(RejectReason::Emergency);
        }
        if self.phase != ControlPhase::Shadow {
            return Err(RejectReason::InactiveMode);
        }
        if self.rearm_required {
            if self.config.manual_rearm && !self.manual_satisfied {
                return Err(RejectReason::RearmRequired);
            }
            if self.shadow_streak < 2 {
                return Err(RejectReason::RecoveryIncomplete);
            }
        }
        self.phase = ControlPhase::Active;
        self.rearm_required = false;
        self.manual_satisfied = false;
        Ok(())
    }

    pub fn capture(&mut self, now_us: u64, obs: Observation) -> Result<Capture, PolicyError> {
        self.check_observation(&obs)?;
        if self.session.is_none() {
            return Err(PolicyError::NoSession);
        }
        if self.inflight.is_some() {
            self.unsent = Some(obs);
            return Ok(Capture::Retained);
        }
        let (snapshot, hash) = self.seal(now_us, obs)?;
        Ok(Capture::Send { snapshot, hash })
    }

    pub fn promote_unsent(&mut self, now_us: u64) -> Result<Option<Capture>, PolicyError> {
        let Some(obs) = self.unsent.take() else {
            return Ok(None);
        };
        if self.inflight.is_some() || self.session.is_none() {
            self.unsent = Some(obs);
            return Ok(None);
        }
        let (snapshot, hash) = self.seal(now_us, obs)?;
        Ok(Some(Capture::Send { snapshot, hash }))
    }

    pub fn on_proposal(&mut self, now_us: u64, proposal: &Proposal) -> ProposalOutcome {
        let _ = proposal.answer_confidence_bp;
        if let Some(outcome) = self.cached_proposal(proposal) {
            return outcome;
        }
        let pending = match self.pending_for(proposal.boot_id, proposal.session_id, proposal.request_seq)
        {
            Ok(pending) => pending,
            Err(reason) => return ProposalOutcome::Rejected(reason),
        };
        if let Some(reason) = self.semantic_reason(now_us, proposal, pending) {
            self.shadow_streak = 0;
            let ack = self.reject_ack(now_us, proposal.request_seq, reason);
            self.consume(
                proposal.session_id,
                proposal.request_seq,
                proposal.snapshot_hash,
                ConsumedKind::Proposal,
                proposal.profile,
                ack,
                true,
            );
            return ProposalOutcome::Rejected(reason);
        }
        if self.phase == ControlPhase::Shadow {
            let ack = self.push_ack(Ack {
                kind: AckKind::Shadow,
                request_seq: proposal.request_seq,
                previous_generation: self.generation,
                generation: self.generation,
                profile: proposal.profile,
                guest_us: now_us,
                lease_until_guest_us: self.lease_until.unwrap_or(0),
                desired_matches_actual: false,
            });
            self.consume(
                proposal.session_id,
                proposal.request_seq,
                proposal.snapshot_hash,
                ConsumedKind::Proposal,
                proposal.profile,
                ack,
                true,
            );
            self.shadow_streak = self.shadow_streak.saturating_add(1);
            return ProposalOutcome::ShadowNoted;
        }
        if self.phase != ControlPhase::Active || !self.config.run_mode.allows_network_activation() {
            self.shadow_streak = 0;
            let ack = self.reject_ack(now_us, proposal.request_seq, RejectReason::InactiveMode);
            self.consume(
                proposal.session_id,
                proposal.request_seq,
                proposal.snapshot_hash,
                ConsumedKind::Proposal,
                proposal.profile,
                ack,
                true,
            );
            return ProposalOutcome::Rejected(RejectReason::InactiveMode);
        }
        if self.staged.is_some() {
            return ProposalOutcome::Rejected(RejectReason::Busy);
        }
        self.staged = Some(Staged {
            proposal: *proposal,
            accept_until: pending.snapshot.accept_until_guest_us,
            lease_until: pending.snapshot.lease_until_guest_us,
        });
        self.consume(
            proposal.session_id,
            proposal.request_seq,
            proposal.snapshot_hash,
            ConsumedKind::Proposal,
            proposal.profile,
            blank_ack(),
            false,
        );
        ProposalOutcome::Staged
    }

    pub fn on_abstain(&mut self, now_us: u64, abstain: &Abstain) -> ProposalOutcome {
        if let Some(prev) = self.last_consumed {
            if abstain.session_id == prev.session
                && abstain.request_seq == prev.seq
                && prev.kind == ConsumedKind::Abstain
                && abstain.snapshot_hash == prev.snapshot_hash
            {
                return ProposalOutcome::Cached(prev.ack);
            }
        }
        if let Err(reason) = self.pending_for(abstain.boot_id, abstain.session_id, abstain.request_seq) {
            return ProposalOutcome::Rejected(reason);
        }
        let pending = self.inflight.expect("pending matched");
        if abstain.snapshot_hash != pending.hash {
            let ack = self.reject_ack(now_us, abstain.request_seq, RejectReason::SnapshotMismatch);
            self.consume(
                abstain.session_id,
                abstain.request_seq,
                abstain.snapshot_hash,
                ConsumedKind::Abstain,
                self.profile,
                ack,
                true,
            );
            return ProposalOutcome::Rejected(RejectReason::SnapshotMismatch);
        }
        self.shadow_streak = 0;
        let ack = self.push_ack(Ack {
            kind: AckKind::Shadow,
            request_seq: abstain.request_seq,
            previous_generation: self.generation,
            generation: self.generation,
            profile: self.profile,
            guest_us: now_us,
            lease_until_guest_us: self.lease_until.unwrap_or(0),
            desired_matches_actual: self.converged(),
        });
        self.consume(
            abstain.session_id,
            abstain.request_seq,
            abstain.snapshot_hash,
            ConsumedKind::Abstain,
            self.profile,
            ack,
            true,
        );
        ProposalOutcome::ShadowNoted
    }

    pub fn activate(&mut self, now_us: u64) -> ActivateOutcome {
        let Some(staged) = self.staged.take() else {
            return ActivateOutcome::Idle;
        };
        if self.emergency {
            return ActivateOutcome::Dropped(self.drop_staged(now_us, &staged, RejectReason::Emergency));
        }
        if self.generation != staged.proposal.base_generation {
            return ActivateOutcome::Dropped(self.drop_staged(now_us, &staged, RejectReason::StaleGeneration));
        }
        if now_us > staged.accept_until {
            return ActivateOutcome::Dropped(self.drop_staged(now_us, &staged, RejectReason::Late));
        }
        if self.phase != ControlPhase::Active {
            return ActivateOutcome::Dropped(self.drop_staged(now_us, &staged, RejectReason::InactiveMode));
        }
        let renewal = staged.proposal.profile == self.profile;
        let previous = self.generation;
        if !renewal {
            self.last_profile_change_us = now_us;
        }
        self.last_renewal_us = now_us;
        self.actuate(staged.proposal.profile, self.model_domains());
        self.generation = self.generation.saturating_add(1);
        self.lease_until = Some(staged.lease_until);
        let ack = self.push_ack(Ack {
            kind: AckKind::Applied { renewal },
            request_seq: staged.proposal.request_seq,
            previous_generation: previous,
            generation: self.generation,
            profile: self.profile,
            guest_us: now_us,
            lease_until_guest_us: staged.lease_until,
            desired_matches_actual: self.converged(),
        });
        if let Some(prev) = self.last_consumed.as_mut() {
            if prev.seq == staged.proposal.request_seq && prev.session == staged.proposal.session_id
            {
                prev.ack = ack;
                prev.settled = true;
            }
        }
        ActivateOutcome::Applied(ack)
    }

    pub fn poll(&mut self, now_us: u64) -> PollOutcome {
        let Some(until) = self.lease_until else {
            return PollOutcome::None;
        };
        if now_us < until {
            return PollOutcome::None;
        }
        self.lease_until = None;
        self.staged = None;
        self.inflight = None;
        self.unsent = None;
        if self.emergency {
            return PollOutcome::Suppressed;
        }
        let ack = self.install_fallback(now_us, AckKind::LeaseExpired);
        self.rearm_required = true;
        self.shadow_streak = 0;
        if self.session.is_some() && self.config.run_mode.allows_network_activation() {
            self.phase = ControlPhase::Degraded;
        } else {
            self.phase = ControlPhase::Baseline;
        }
        PollOutcome::Expired(ack)
    }

    pub fn install_emergency(&mut self, now_us: u64) -> Option<Ack> {
        if self.emergency {
            return None;
        }
        self.emergency = true;
        self.staged = None;
        let previous = self.generation;
        let changed = self.profile != ProfileId::Reclaim;
        self.actuate(ProfileId::Reclaim, self.model_domains());
        if changed {
            self.last_profile_change_us = now_us;
        }
        self.lease_until = None;
        self.generation = self.generation.saturating_add(1);
        self.rearm_required = true;
        self.shadow_streak = 0;
        if self.phase == ControlPhase::Active {
            self.phase = ControlPhase::Degraded;
        }
        Some(self.push_ack(Ack {
            kind: AckKind::Emergency,
            request_seq: 0,
            previous_generation: previous,
            generation: self.generation,
            profile: self.profile,
            guest_us: now_us,
            lease_until_guest_us: 0,
            desired_matches_actual: self.converged(),
        }))
    }

    pub fn clear_emergency(&mut self, now_us: u64) -> Result<Ack, PolicyError> {
        if !self.emergency {
            return Err(PolicyError::NotEmergency);
        }
        self.emergency = false;
        let ack = self.install_fallback(now_us, AckKind::Fallback);
        self.rearm_required = true;
        self.shadow_streak = 0;
        self.phase = if self.session.is_some() {
            ControlPhase::Shadow
        } else {
            ControlPhase::Baseline
        };
        Ok(ack)
    }

    pub fn disable_live(&mut self, now_us: u64) -> Ack {
        self.staged = None;
        self.rearm_required = true;
        self.shadow_streak = 0;
        self.phase = if self.session.is_some() {
            ControlPhase::Shadow
        } else {
            ControlPhase::Baseline
        };
        if self.emergency {
            return self.push_ack(Ack {
                kind: AckKind::Fallback,
                request_seq: 0,
                previous_generation: self.generation,
                generation: self.generation,
                profile: self.profile,
                guest_us: now_us,
                lease_until_guest_us: 0,
                desired_matches_actual: self.converged(),
            });
        }
        self.install_fallback(now_us, AckKind::Fallback)
    }

    pub fn apply_local(&mut self, now_us: u64, profile: ProfileId) -> Result<Ack, PolicyError> {
        if self.config.run_mode != RunMode::Heuristic {
            return Err(PolicyError::Reject(RejectReason::InactiveMode));
        }
        if self.emergency {
            return Err(PolicyError::Reject(RejectReason::Emergency));
        }
        self.dwell_ok(now_us, profile)
            .map_err(PolicyError::Reject)?;
        let lease = checked_deadline(now_us, PROFILE_LEASE_US).ok_or(PolicyError::Overflow)?;
        let renewal = profile == self.profile;
        let previous = self.generation;
        if !renewal {
            self.last_profile_change_us = now_us;
        }
        self.last_renewal_us = now_us;
        self.actuate(
            profile,
            Domains {
                memory: self.spec.has(CAP_MEMORY),
                admission: self.spec.has(CAP_ADMISSION),
            },
        );
        self.generation = self.generation.saturating_add(1);
        self.lease_until = Some(lease);
        Ok(self.push_ack(Ack {
            kind: AckKind::Applied { renewal },
            request_seq: 0,
            previous_generation: previous,
            generation: self.generation,
            profile: self.profile,
            guest_us: now_us,
            lease_until_guest_us: lease,
            desired_matches_actual: self.converged(),
        }))
    }

    pub fn drain_acks(&mut self) -> ([Option<Ack>; ACK_CAP], u64) {
        let mut out = [None; ACK_CAP];
        let mut count = 0;
        while self.ack_len > 0 && count < ACK_CAP {
            out[count] = Some(self.acks[self.ack_head]);
            self.ack_head = (self.ack_head + 1) % ACK_CAP;
            self.ack_len -= 1;
            count += 1;
        }
        let dropped = self.acks_dropped;
        self.acks_dropped = 0;
        (out, dropped)
    }

    fn seal(&mut self, now_us: u64, obs: Observation) -> Result<(Snapshot, Hash32), PolicyError> {
        let session = self.session.ok_or(PolicyError::NoSession)?;
        let accept = checked_deadline(now_us, ACCEPTANCE_DEADLINE_US).ok_or(PolicyError::Overflow)?;
        let lease = checked_deadline(now_us, PROFILE_LEASE_US).ok_or(PolicyError::Overflow)?;
        let snapshot = Snapshot {
            boot_id: self.config.boot_id,
            session_id: session,
            request_seq: self.next_seq,
            catalog_id: self.config.catalog,
            catalog_hash: self.spec.hash,
            telemetry_version: TELEMETRY_VERSION,
            captured_guest_us: now_us,
            window_us: obs.window_us,
            accept_until_guest_us: accept,
            lease_until_guest_us: lease,
            base_generation: self.generation,
            objective_id: self.config.objective,
            groups: obs.groups,
            pressure: obs.pressure,
            current_profile: self.profile,
            override_active: self.emergency,
        };
        let hash = hash_snapshot(&snapshot).map_err(|_| PolicyError::BadObservation)?;
        self.next_seq = self.next_seq.checked_add(1).ok_or(PolicyError::Overflow)?;
        self.inflight = Some(Pending { snapshot, hash });
        Ok((snapshot, hash))
    }

    fn check_observation(&self, obs: &Observation) -> Result<(), PolicyError> {
        if obs.window_us == 0 {
            return Err(PolicyError::BadObservation);
        }
        for (sample, expected) in obs.groups.iter().zip(GROUP_ORDER) {
            if sample.class != expected {
                return Err(PolicyError::BadObservation);
            }
            if sample.wait_samples == 0 && sample.max_wait_us != 0 {
                return Err(PolicyError::BadObservation);
            }
        }
        Ok(())
    }

    fn cached_proposal(&self, proposal: &Proposal) -> Option<ProposalOutcome> {
        let prev = self.last_consumed?;
        if proposal.boot_id != self.config.boot_id || proposal.session_id != prev.session {
            return None;
        }
        if proposal.request_seq != prev.seq || prev.kind != ConsumedKind::Proposal {
            return None;
        }
        if proposal.snapshot_hash == prev.snapshot_hash && proposal.profile == prev.profile && prev.settled
        {
            Some(ProposalOutcome::Cached(prev.ack))
        } else {
            Some(ProposalOutcome::Rejected(RejectReason::Duplicate))
        }
    }

    fn pending_for(
        &self,
        boot: policy_types::BootId,
        session: SessionId,
        seq: u64,
    ) -> Result<Pending, RejectReason> {
        if boot != self.config.boot_id {
            return Err(RejectReason::IdentityMismatch);
        }
        let Some(current) = self.session else {
            return Err(RejectReason::IdentityMismatch);
        };
        if session != current {
            return Err(RejectReason::IdentityMismatch);
        }
        let Some(pending) = self.inflight else {
            return Err(RejectReason::IdentityMismatch);
        };
        if pending.snapshot.request_seq != seq {
            return Err(RejectReason::IdentityMismatch);
        }
        Ok(pending)
    }

    fn semantic_reason(&self, now_us: u64, proposal: &Proposal, pending: Pending) -> Option<RejectReason> {
        if proposal.base_generation != pending.snapshot.base_generation {
            return Some(RejectReason::StaleGeneration);
        }
        if proposal.catalog_id != self.config.catalog || proposal.catalog_hash != self.spec.hash {
            return Some(RejectReason::CatalogMismatch);
        }
        if proposal.snapshot_hash != pending.hash {
            return Some(RejectReason::SnapshotMismatch);
        }
        if proposal.model_manifest_hash != self.config.approved_model
            || proposal.calibration_hash != self.config.approved_calibration
        {
            return Some(RejectReason::UnapprovedArtifact);
        }
        if now_us > pending.snapshot.accept_until_guest_us {
            return Some(RejectReason::Late);
        }
        if proposal.base_generation != self.generation {
            return Some(RejectReason::StaleGeneration);
        }
        if self.emergency {
            return Some(RejectReason::Emergency);
        }
        if let Err(reason) = self.dwell_ok(now_us, proposal.profile) {
            return Some(reason);
        }
        None
    }

    fn dwell_ok(&self, now_us: u64, profile: ProfileId) -> Result<(), RejectReason> {
        if profile == self.profile {
            return Ok(());
        }
        if now_us.saturating_sub(self.last_profile_change_us) < MIN_DWELL_US {
            return Err(RejectReason::Dwell);
        }
        Ok(())
    }

    fn model_domains(&self) -> Domains {
        Domains {
            memory: self.spec.has(CAP_MEMORY),
            admission: self.spec.has(CAP_ADMISSION),
        }
    }

    fn actuate(&mut self, profile: ProfileId, domains: Domains) {
        let params = self.spec.profile(profile);
        self.profile = profile;
        self.weights = params.weights();
        if domains.memory {
            self.desired_cache = params.cache_soft_target_bytes;
        }
        if domains.admission {
            self.desired_inflight_latency = params.inflight_latency;
            self.desired_inflight_batch = params.inflight_batch;
        }
    }

    fn install_fallback(&mut self, now_us: u64, kind: AckKind) -> Ack {
        let previous = self.generation;
        let changed = self.profile != ProfileId::Balanced;
        self.actuate(
            ProfileId::Balanced,
            Domains {
                memory: true,
                admission: true,
            },
        );
        if changed {
            self.last_profile_change_us = now_us;
        }
        self.lease_until = None;
        self.generation = self.generation.saturating_add(1);
        self.push_ack(Ack {
            kind,
            request_seq: 0,
            previous_generation: previous,
            generation: self.generation,
            profile: self.profile,
            guest_us: now_us,
            lease_until_guest_us: 0,
            desired_matches_actual: self.converged(),
        })
    }

    fn converged(&self) -> bool {
        !self.spec.has(CAP_MEMORY) || self.cache_used <= self.desired_cache
    }

    fn reject_ack(&mut self, now_us: u64, seq: u64, reason: RejectReason) -> Ack {
        self.push_ack(Ack {
            kind: AckKind::Rejected(reason),
            request_seq: seq,
            previous_generation: self.generation,
            generation: self.generation,
            profile: self.profile,
            guest_us: now_us,
            lease_until_guest_us: self.lease_until.unwrap_or(0),
            desired_matches_actual: self.converged(),
        })
    }

    fn drop_staged(&mut self, now_us: u64, staged: &Staged, reason: RejectReason) -> RejectReason {
        let ack = self.reject_ack(now_us, staged.proposal.request_seq, reason);
        if let Some(prev) = self.last_consumed.as_mut() {
            if prev.seq == staged.proposal.request_seq && prev.session == staged.proposal.session_id
            {
                prev.ack = ack;
                prev.settled = true;
            }
        }
        reason
    }

    fn consume(
        &mut self,
        session: SessionId,
        seq: u64,
        snapshot_hash: Hash32,
        kind: ConsumedKind,
        profile: ProfileId,
        ack: Ack,
        settled: bool,
    ) {
        self.inflight = None;
        self.last_consumed = Some(Consumed {
            session,
            seq,
            snapshot_hash,
            kind,
            profile,
            ack,
            settled,
        });
    }

    fn push_ack(&mut self, ack: Ack) -> Ack {
        if self.ack_len == ACK_CAP {
            self.ack_head = (self.ack_head + 1) % ACK_CAP;
            self.ack_len -= 1;
            self.acks_dropped = self.acks_dropped.saturating_add(1);
        }
        let index = (self.ack_head + self.ack_len) % ACK_CAP;
        self.acks[index] = ack;
        self.ack_len += 1;
        ack
    }
}

fn blank_ack() -> Ack {
    Ack {
        kind: AckKind::Fallback,
        request_seq: 0,
        previous_generation: 0,
        generation: 0,
        profile: ProfileId::Balanced,
        guest_us: 0,
        lease_until_guest_us: 0,
        desired_matches_actual: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use policy_types::{BootId, Hash32, ReasonCode, RunMode, SessionId, PROFILE_LEASE_US};

    fn ids() -> (BootId, SessionId) {
        (
            BootId::from_hex("0123456789abcdef0123456789abcdef").unwrap(),
            SessionId::from_hex("abcdef0123456789abcdef0123456789").unwrap(),
        )
    }

    fn engine(mode: RunMode, catalog: CatalogId) -> PolicyEngine {
        let (boot_id, _) = ids();
        PolicyEngine::boot(EngineConfig {
            catalog,
            boot_id,
            objective: ObjectiveId::MixedLatencyV1,
            approved_model: Hash32::repeat(0xcc),
            approved_calibration: Hash32::repeat(0xdd),
            run_mode: mode,
            manual_rearm: false,
        })
    }

    fn handshake_session(engine: &mut PolicyEngine, session_id: SessionId) {
        let status = engine.status();
        engine
            .accept_hello_ack(&HelloAck {
                boot_id: engine.config.boot_id,
                session_id,
                catalog_id: status.catalog,
                catalog_hash: status.catalog_hash,
                model_manifest_hash: Hash32::repeat(0xcc),
                calibration_hash: Hash32::repeat(0xdd),
                ready: true,
            })
            .unwrap();
    }

    fn handshake(engine: &mut PolicyEngine) {
        let (_, session_id) = ids();
        handshake_session(engine, session_id);
    }

    fn arm(engine: &mut PolicyEngine) {
        handshake(engine);
        engine.promote_to_active().unwrap();
    }

    fn proposal_at(engine: &mut PolicyEngine, now: u64, profile: ProfileId, confidence: u16) -> Proposal {
        let Capture::Send { snapshot, hash } = engine
            .capture(now, Observation::quiet(1_000_000))
            .unwrap()
        else {
            panic!("snapshot should be sent");
        };
        Proposal {
            boot_id: snapshot.boot_id,
            session_id: snapshot.session_id,
            request_seq: snapshot.request_seq,
            base_generation: snapshot.base_generation,
            catalog_id: snapshot.catalog_id,
            catalog_hash: snapshot.catalog_hash,
            snapshot_hash: hash,
            profile,
            answer_confidence_bp: confidence,
            model_manifest_hash: Hash32::repeat(0xcc),
            calibration_hash: Hash32::repeat(0xdd),
            reason_code: ReasonCode::MockScript,
        }
    }

    #[test]
    fn controller_absence_leaves_the_fallback_in_force() {
        let mut engine = engine(RunMode::Mock, CatalogId::CpuV1);
        assert_eq!(engine.status().profile, ProfileId::Balanced);
        assert_eq!(engine.status().generation, 1);
        assert!(engine.capture(0, Observation::quiet(1_000_000)).is_err());
        assert!(matches!(engine.poll(10_000_000), PollOutcome::None));
    }

    #[test]
    fn dwell_allows_renewal_without_restarting_the_change_timer() {
        let mut engine = engine(RunMode::Mock, CatalogId::CpuV1);
        arm(&mut engine);
        let change_at = MIN_DWELL_US;
        let proposal = proposal_at(&mut engine, change_at, ProfileId::Latency, 100);
        assert_eq!(engine.on_proposal(change_at, &proposal), ProposalOutcome::Staged);
        assert!(matches!(engine.activate(change_at), ActivateOutcome::Applied(_)));
        assert_eq!(engine.status().last_profile_change_us, change_at);

        let renew_at = change_at + 500_000;
        let renewal = proposal_at(&mut engine, renew_at, ProfileId::Latency, 100);
        assert_eq!(engine.on_proposal(renew_at, &renewal), ProposalOutcome::Staged);
        assert!(matches!(engine.activate(renew_at), ActivateOutcome::Applied(ack) if matches!(ack.kind, AckKind::Applied { renewal: true })));
        assert_eq!(engine.status().last_profile_change_us, change_at);
        assert_eq!(engine.status().last_renewal_us, renew_at);

        let too_soon = change_at + MIN_DWELL_US - 1;
        let other = proposal_at(&mut engine, too_soon, ProfileId::Throughput, 100);
        assert_eq!(
            engine.on_proposal(too_soon, &other),
            ProposalOutcome::Rejected(RejectReason::Dwell)
        );
        assert_eq!(engine.status().profile, ProfileId::Latency);
    }

    #[test]
    fn late_foreign_and_stale_activation_do_not_change_policy() {
        let mut engine = engine(RunMode::Mock, CatalogId::CpuV1);
        arm(&mut engine);
        let proposal = proposal_at(&mut engine, 0, ProfileId::Balanced, 9_900);
        assert_eq!(
            engine.on_proposal(ACCEPTANCE_DEADLINE_US + 1, &proposal),
            ProposalOutcome::Rejected(RejectReason::Late)
        );

        let staged = proposal_at(&mut engine, 1_000, ProfileId::Balanced, 1);
        assert_eq!(engine.on_proposal(1_000, &staged), ProposalOutcome::Staged);
        assert_eq!(
            engine.activate(1_000 + ACCEPTANCE_DEADLINE_US + 1),
            ActivateOutcome::Dropped(RejectReason::Late)
        );
        assert_eq!(engine.status().generation, 1);

        let real = proposal_at(&mut engine, 2_000, ProfileId::Balanced, 1);
        let mut foreign = real;
        foreign.boot_id = BootId::from_hex("ffffffffffffffffffffffffffffffff").unwrap();
        assert_eq!(
            engine.on_proposal(2_000, &foreign),
            ProposalOutcome::Rejected(RejectReason::IdentityMismatch)
        );
        assert_eq!(engine.on_proposal(2_000, &real), ProposalOutcome::Staged);
        assert!(matches!(engine.activate(2_000), ActivateOutcome::Applied(_)));

        let previous = proposal_at(&mut engine, 3_000, ProfileId::Balanced, 1);
        let other = SessionId::from_hex("00112233445566778899aabbccddeeff").unwrap();
        handshake_session(&mut engine, other);
        assert_eq!(
            engine.on_proposal(3_000, &previous),
            ProposalOutcome::Rejected(RejectReason::IdentityMismatch)
        );
    }

    #[test]
    fn confidence_does_not_admit_an_invalid_proposal() {
        let mut engine = engine(RunMode::Mock, CatalogId::CpuV1);
        arm(&mut engine);
        let mut high = proposal_at(&mut engine, MIN_DWELL_US, ProfileId::Throughput, 10_000);
        high.catalog_hash = Hash32::repeat(0x11);
        let reason = engine.on_proposal(MIN_DWELL_US, &high);
        assert_eq!(reason, ProposalOutcome::Rejected(RejectReason::CatalogMismatch));

        let mut low = proposal_at(&mut engine, MIN_DWELL_US, ProfileId::Throughput, 0);
        low.catalog_hash = Hash32::repeat(0x11);
        assert_eq!(engine.on_proposal(MIN_DWELL_US, &low), reason);
        assert_eq!(engine.status().profile, ProfileId::Balanced);
    }

    #[test]
    fn retransmit_after_apply_is_idempotent() {
        let mut engine = engine(RunMode::Mock, CatalogId::CpuV1);
        arm(&mut engine);
        let now = MIN_DWELL_US;
        let proposal = proposal_at(&mut engine, now, ProfileId::Latency, 8_000);
        assert_eq!(engine.on_proposal(now, &proposal), ProposalOutcome::Staged);
        assert!(matches!(engine.activate(now), ActivateOutcome::Applied(_)));
        let generation = engine.status().generation;
        let lease = engine.status().lease_until_guest_us;
        assert!(matches!(
            engine.on_proposal(now, &proposal),
            ProposalOutcome::Cached(ack) if matches!(ack.kind, AckKind::Applied { .. })
        ));
        assert_eq!(engine.status().generation, generation);
        assert_eq!(engine.status().lease_until_guest_us, lease);
    }

    #[test]
    fn emergency_between_stage_and_apply_keeps_the_override() {
        let mut engine = engine(RunMode::Live, CatalogId::JointV1);
        arm(&mut engine);
        let now = MIN_DWELL_US;
        let proposal = proposal_at(&mut engine, now, ProfileId::Throughput, 9_000);
        assert_eq!(engine.on_proposal(now, &proposal), ProposalOutcome::Staged);
        engine.install_emergency(now);
        assert_eq!(engine.status().profile, ProfileId::Reclaim);
        assert_eq!(engine.activate(now), ActivateOutcome::Idle);

        engine.staged = Some(Staged {
            proposal,
            accept_until: now + 1_000,
            lease_until: now + 1_000,
        });
        assert_eq!(
            engine.activate(now),
            ActivateOutcome::Dropped(RejectReason::Emergency)
        );
        assert_eq!(engine.status().profile, ProfileId::Reclaim);
        assert_eq!(engine.status().desired_cache_bytes, 33_554_432);
    }

    #[test]
    fn cpu_catalog_does_not_actuate_memory_or_admission() {
        let now = MIN_DWELL_US;
        let mut cpu = engine(RunMode::Mock, CatalogId::CpuV1);
        arm(&mut cpu);
        let proposal = proposal_at(&mut cpu, now, ProfileId::Throughput, 1);
        assert_eq!(cpu.on_proposal(now, &proposal), ProposalOutcome::Staged);
        assert!(matches!(cpu.activate(now), ActivateOutcome::Applied(_)));
        assert_eq!(cpu.status().weights, [2, 6, 2]);
        assert_eq!(cpu.status().desired_cache_bytes, 67_108_864);
        assert_eq!(cpu.status().desired_inflight_batch, 8);

        let mut joint = engine(RunMode::Mock, CatalogId::JointV1);
        arm(&mut joint);
        let proposal = proposal_at(&mut joint, now, ProfileId::Throughput, 1);
        assert_eq!(joint.on_proposal(now, &proposal), ProposalOutcome::Staged);
        assert!(matches!(joint.activate(now), ActivateOutcome::Applied(_)));
        assert_eq!(joint.status().desired_cache_bytes, 100_663_296);
        assert_eq!(joint.status().desired_inflight_latency, 8);
        assert_eq!(joint.status().desired_inflight_batch, 16);
    }

    #[test]
    fn lease_expiry_and_abstain_restore_fallback_without_extending_it() {
        let mut engine = engine(RunMode::Mock, CatalogId::CpuV1);
        arm(&mut engine);
        let now = MIN_DWELL_US;
        let proposal = proposal_at(&mut engine, now, ProfileId::Latency, 1);
        engine.on_proposal(now, &proposal);
        engine.activate(now);
        let lease = engine.status().lease_until_guest_us.expect("lease");
        assert_eq!(lease, now + PROFILE_LEASE_US);

        let Capture::Send { snapshot, hash } = engine
            .capture(now + 1_000, Observation::quiet(1_000_000))
            .unwrap()
        else {
            panic!("snapshot should be sent");
        };
        let abstain = Abstain {
            boot_id: snapshot.boot_id,
            session_id: snapshot.session_id,
            request_seq: snapshot.request_seq,
            snapshot_hash: hash,
            reason: policy_types::AbstainReason::LowConfidence,
        };
        assert_eq!(
            engine.on_abstain(now + 1_000, &abstain),
            ProposalOutcome::ShadowNoted
        );
        assert_eq!(engine.status().lease_until_guest_us, Some(lease));
        assert_eq!(engine.status().profile, ProfileId::Latency);

        engine.controller_lost();
        assert!(matches!(engine.poll(lease - 1), PollOutcome::None));
        assert_eq!(engine.status().profile, ProfileId::Latency);
        assert!(matches!(engine.poll(lease), PollOutcome::Expired(_)));
        assert_eq!(engine.status().profile, ProfileId::Balanced);
        assert_eq!(engine.status().phase, ControlPhase::Baseline);
    }

    #[test]
    fn recovery_requires_two_shadow_decisions_and_manual_rearm_when_configured() {
        let mut engine = engine(RunMode::Live, CatalogId::CpuV1);
        arm(&mut engine);
        let now = MIN_DWELL_US;
        let proposal = proposal_at(&mut engine, now, ProfileId::Latency, 1);
        engine.on_proposal(now, &proposal);
        engine.activate(now);
        let lease = engine.status().lease_until_guest_us.unwrap();
        assert!(matches!(engine.poll(lease), PollOutcome::Expired(_)));
        assert_eq!(engine.status().phase, ControlPhase::Degraded);
        engine.note_controller_healthy();
        assert_eq!(
            engine.promote_to_active(),
            Err(RejectReason::RecoveryIncomplete)
        );
        let first = proposal_at(&mut engine, lease, ProfileId::Balanced, 1);
        assert_eq!(engine.on_proposal(lease, &first), ProposalOutcome::ShadowNoted);
        assert_eq!(
            engine.promote_to_active(),
            Err(RejectReason::RecoveryIncomplete)
        );
        let second = proposal_at(&mut engine, lease + 1, ProfileId::Balanced, 1);
        assert_eq!(
            engine.on_proposal(lease + 1, &second),
            ProposalOutcome::ShadowNoted
        );
        engine.promote_to_active().unwrap();

        engine.config.manual_rearm = true;
        engine.disable_live(lease + 2);
        let third = proposal_at(&mut engine, lease + 2, ProfileId::Balanced, 1);
        assert_eq!(
            engine.on_proposal(lease + 2, &third),
            ProposalOutcome::ShadowNoted
        );
        let fourth = proposal_at(&mut engine, lease + 3, ProfileId::Balanced, 1);
        assert_eq!(
            engine.on_proposal(lease + 3, &fourth),
            ProposalOutcome::ShadowNoted
        );
        assert_eq!(engine.promote_to_active(), Err(RejectReason::RearmRequired));
        engine.acknowledge_rearm();
        engine.promote_to_active().unwrap();
    }

    #[test]
    fn a_full_mailbox_rejects_the_new_proposal_and_keeps_the_staged_one() {
        let mut engine = engine(RunMode::Mock, CatalogId::CpuV1);
        arm(&mut engine);
        let now = MIN_DWELL_US;
        let first = proposal_at(&mut engine, now, ProfileId::Throughput, 1);
        assert_eq!(engine.on_proposal(now, &first), ProposalOutcome::Staged);
        let second = proposal_at(&mut engine, now + 1, ProfileId::Reclaim, 1);
        assert_eq!(
            engine.on_proposal(now + 1, &second),
            ProposalOutcome::Rejected(RejectReason::Busy)
        );
        assert!(matches!(engine.activate(now + 1), ActivateOutcome::Applied(_)));
        assert_eq!(engine.status().profile, ProfileId::Throughput);
    }

    #[test]
    fn unread_acknowledgements_are_dropped_without_losing_generation() {
        let mut engine = engine(RunMode::Mock, CatalogId::CpuV1);
        arm(&mut engine);
        for step in 0..10 {
            let mut proposal = proposal_at(&mut engine, step, ProfileId::Balanced, 1);
            proposal.catalog_hash = Hash32::repeat(0x22);
            assert_eq!(
                engine.on_proposal(step, &proposal),
                ProposalOutcome::Rejected(RejectReason::CatalogMismatch)
            );
        }
        let (acks, dropped) = engine.drain_acks();
        assert!(acks.iter().flatten().count() <= 8);
        assert!(dropped >= 2);
        assert_eq!(engine.status().generation, 1);
    }

    #[test]
    fn malformed_observations_are_refused() {
        let mut engine = engine(RunMode::Mock, CatalogId::CpuV1);
        handshake(&mut engine);
        assert!(engine.capture(0, Observation::quiet(0)).is_err());
        let mut obs = Observation::quiet(1_000);
        obs.groups[0].max_wait_us = 50;
        obs.groups[0].wait_samples = 0;
        assert!(engine.capture(0, obs).is_err());
    }
}
