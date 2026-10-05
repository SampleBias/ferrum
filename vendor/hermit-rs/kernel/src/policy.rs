//! Opt-in hierarchical fair scheduler for registered threads.
//!
//! Virtual runtime is `delta_us * 1_048_576 / weight`, with a minimum
//! increment of one. System work may take 5 ms of each 100 ms before
//! workload groups run. Sleeping groups are clamped up to the minimum
//! virtual runtime of the other runnable groups. Weight changes keep the
//! accumulated virtual runtime.
//!
//! This state is only used from the scheduler with interrupts disabled and
//! from the policy syscalls.

use crate::scheduler::task::TaskId;

pub const CLASS_LATENCY: u8 = 1;
pub const CLASS_BATCH: u8 = 2;
pub const CLASS_MAINTENANCE: u8 = 3;
pub const CLASS_SYSTEM: u8 = 4;

const SCALE: u128 = 1_048_576;
const QUANTUM_US: u64 = 2_000;
const INTERVAL_US: u64 = 100_000;
const SYSTEM_BUDGET_US: u64 = 5_000;
/// Minimum time between two different profiles. A renewal does not restart it.
const MIN_DWELL_US: u64 = 2_000_000;
const MAX_MEMBERS: usize = 12;
const MAX_WORKLOAD: usize = 8;
const MAX_SYSTEM: usize = 4;
const ACK_CAP: usize = 8;

pub const ACK_APPLIED: u8 = 1;
pub const ACK_LEASE_EXPIRED: u8 = 2;
pub const ACK_EMERGENCY: u8 = 3;
pub const ACK_REJECTED: u8 = 4;
pub const ACK_REASON_NONE: u8 = 0;
pub const ACK_REASON_EMERGENCY: u8 = 1;
pub const ACK_REASON_LATE: u8 = 2;
pub const ACK_REASON_STALE: u8 = 3;
pub const ACK_REASON_DWELL: u8 = 4;
pub const PROFILE_BALANCED: u8 = 0;
pub const PROFILE_LATENCY: u8 = 1;
pub const PROFILE_THROUGHPUT: u8 = 2;
pub const PROFILE_RECLAIM: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
	Fallback,
	Stay,
	Switch(TaskId),
}

#[derive(Clone, Copy)]
struct Member {
	id: Option<TaskId>,
	class: u8,
	ticket: u64,
	service_us: u64,
}

#[derive(Clone, Copy)]
pub struct SchedAck {
	pub kind: u8,
	pub reason: u8,
	pub profile: u8,
	pub previous_generation: u64,
	pub generation: u64,
	pub guest_us: u64,
	pub lease_until_guest_us: u64,
}

const EMPTY_ACK: SchedAck = SchedAck {
	kind: 0,
	reason: 0,
	profile: 0,
	previous_generation: 0,
	generation: 0,
	guest_us: 0,
	lease_until_guest_us: 0,
};

struct Pending {
	occupied: bool,
	weights: [u32; 3],
	accept_until: u64,
	lease_until: u64,
	base_generation: u64,
}

struct State {
	members: [Member; MAX_MEMBERS],
	weights: [u32; 3],
	vr: [u128; 3],
	group_service: [u64; 3],
	system_service: u64,
	next_ticket: u64,
	rr: [u64; 4],
	last_account_us: u64,
	interval_start: u64,
	system_charged: u64,
	was_runnable: [bool; 3],
	active: bool,
	/// Absolute guest time at which the active weights fall back to balanced.
	/// Zero means no lease is armed.
	lease_until: u64,
	lease_expired: bool,
	/// Local override. While set, expiry records the lease as expired and
	/// leaves the reclaim weights in place.
	emergency: bool,
	/// Scheduler-owned generation. Boot is 1. Activation, emergency, and
	/// balanced fallback each increment it. A dropped stage does not.
	generation: u64,
	/// Guest time of the last change to a different profile. Renewals skip this.
	last_profile_change_us: u64,
	/// The boot weights are not a dwell event. The first staged change arms it.
	dwell_armed: bool,
	staged: Pending,
	ack_head: usize,
	ack_len: usize,
	acks_dropped: u64,
	acks: [SchedAck; ACK_CAP],
}

static STATE: hermit_sync::InterruptTicketMutex<State> = hermit_sync::InterruptTicketMutex::new(
	State {
		members: [Member {
			id: None,
			class: 0,
			ticket: 0,
			service_us: 0,
		}; MAX_MEMBERS],
		weights: [1, 1, 1],
		vr: [0; 3],
		group_service: [0; 3],
		system_service: 0,
		next_ticket: 1,
		rr: [0; 4],
		last_account_us: 0,
		interval_start: 0,
		system_charged: 0,
		was_runnable: [false; 3],
		active: false,
		lease_until: 0,
		lease_expired: false,
		emergency: false,
		generation: 1,
		last_profile_change_us: 0,
		dwell_armed: false,
		staged: Pending {
			occupied: false,
			weights: [0, 0, 0],
			accept_until: 0,
			lease_until: 0,
			base_generation: 0,
		},
		ack_head: 0,
		ack_len: 0,
		acks_dropped: 0,
		acks: [EMPTY_ACK; ACK_CAP],
	},
);

pub fn register(id: TaskId, class: u8) -> i32 {
	let mut state = STATE.lock();
	if class == 0 {
		clear_member(&mut state, id);
		state.active = state.members.iter().any(|member| member.id.is_some());
		return 0;
	}
	if !matches!(class, CLASS_LATENCY | CLASS_BATCH | CLASS_MAINTENANCE | CLASS_SYSTEM) {
		return -1;
	}
	if let Some(index) = state.members.iter().position(|member| member.id == Some(id)) {
		state.members[index].class = class;
		state.active = true;
		return 0;
	}
	let workload = class != CLASS_SYSTEM;
	let count = state
		.members
		.iter()
		.filter(|member| member.id.is_some() && (member.class != CLASS_SYSTEM) == workload)
		.count();
	if workload && count >= MAX_WORKLOAD || !workload && count >= MAX_SYSTEM {
		return -1;
	}
	let Some(index) = state.members.iter().position(|member| member.id.is_none()) else {
		return -1;
	};
	let ticket = state.next_ticket;
	state.next_ticket = ticket.saturating_add(1);
	state.members[index].id = Some(id);
	state.members[index].class = class;
	state.members[index].ticket = ticket;
	state.members[index].service_us = 0;
	state.active = true;
	0
}

pub fn set_weights(latency: u32, batch: u32, maintenance: u32) -> i32 {
	if latency == 0 || batch == 0 || maintenance == 0 {
		return -1;
	}
	let mut state = STATE.lock();
	if state.emergency {
		return -1;
	}
	state.weights = [latency, batch, maintenance];
	0
}

/// Arm a one-shot fallback to balanced weights. `duration_us` is measured
/// from `now` on the guest timer. The scheduler applies the fallback itself.
pub fn arm_lease(now: u64, duration_us: u64) -> i32 {
	if duration_us == 0 {
		return -1;
	}
	let mut state = STATE.lock();
	if state.emergency {
		return -1;
	}
	state.lease_until = now.saturating_add(duration_us);
	state.lease_expired = false;
	0
}

/// Install the catalog `reclaim` weights and hold them across lease expiry.
/// A second call leaves the override in place. There is no syscall that clears it.
/// Any staged profile is discarded here so a scheduling entry cannot apply it later.
pub fn install_emergency(now: u64) -> i32 {
	let mut state = STATE.lock();
	state.staged.occupied = false;
	if state.emergency {
		return 0;
	}
	let previous = state.generation;
	state.emergency = true;
	state.weights = [2, 2, 6];
	state.generation = state.generation.saturating_add(1);
	let generation = state.generation;
	let lease_until = state.lease_until;
	push_ack(
		&mut state,
		SchedAck {
			kind: ACK_EMERGENCY,
			reason: ACK_REASON_NONE,
			profile: PROFILE_RECLAIM,
			previous_generation: previous,
			generation,
			guest_us: now,
			lease_until_guest_us: lease_until,
		},
	);
	0
}

/// Record a profile for the scheduler to install at its next entry.
/// This does not change the weights in force. A stage is stored even during
/// an emergency; the scheduling entry drops it without applying it.
pub fn stage(
	now: u64,
	latency: u32,
	batch: u32,
	maintenance: u32,
	accept_us: u64,
	lease_us: u64,
	base_generation: u64,
) -> i32 {
	if latency == 0 || batch == 0 || maintenance == 0 || lease_us == 0 {
		return -1;
	}
	// A zero acceptance window is already closed at the staging instant, so the
	// next scheduling entry observes it as late even in the same microsecond.
	let accept_until = if accept_us == 0 {
		now.saturating_sub(1)
	} else {
		now.saturating_add(accept_us)
	};
	let mut state = STATE.lock();
	state.staged = Pending {
		occupied: true,
		weights: [latency, batch, maintenance],
		accept_until,
		lease_until: now.saturating_add(lease_us),
		base_generation,
	};
	0
}

pub fn staged() -> bool {
	STATE.lock().staged.occupied
}

pub fn generation() -> u64 {
	STATE.lock().generation
}

pub fn pop_ack() -> Option<SchedAck> {
	let mut state = STATE.lock();
	if state.ack_len == 0 {
		return None;
	}
	let ack = state.acks[state.ack_head];
	state.ack_head = (state.ack_head + 1) % ACK_CAP;
	state.ack_len -= 1;
	Some(ack)
}

pub fn lease_expired() -> bool {
	STATE.lock().lease_expired
}

pub fn service(class: u8) -> Option<u64> {
	let state = STATE.lock();
	match class {
		CLASS_LATENCY => Some(state.group_service[0]),
		CLASS_BATCH => Some(state.group_service[1]),
		CLASS_MAINTENANCE => Some(state.group_service[2]),
		CLASS_SYSTEM => Some(state.system_service),
		_ => None,
	}
}

/// Class and accumulated service for one registration slot.
/// `None` means the slot is empty or out of range.
pub fn member(slot: usize) -> Option<(u8, u64)> {
	let state = STATE.lock();
	let member = state.members.get(slot)?;
	if member.id.is_none() {
		return None;
	}
	Some((member.class, member.service_us))
}

pub fn decide(
	now: u64,
	blocked_deadline: Option<u64>,
	current_id: TaskId,
	current_class: u8,
	current_running: bool,
	bill_current: bool,
	ready: &[(TaskId, u8)],
) -> (Choice, Option<u64>) {
	let mut state = STATE.lock();
	if !state.active {
		enforce_lease(&mut state, now);
		activate_staged(&mut state, now);
		return (Choice::Fallback, lease_wakeup(now, &state));
	}
	account(
		&mut state,
		now,
		current_id,
		current_class,
		bill_current,
	);
	enforce_lease(&mut state, now);
	activate_staged(&mut state, now);

	let mut runnable = [(TaskId::from(-1), 0u8, false); MAX_MEMBERS];
	let mut n = 0;
	// Class 0 is the default system classification. It shares the system
	// reservation and does not enter a workload group.
	if current_running {
		runnable[n] = (current_id, current_class, true);
		n += 1;
	}
	for &(id, class) in ready {
		if id != current_id && n < MAX_MEMBERS {
			runnable[n] = (id, class, false);
			n += 1;
		}
	}
	if n == 0 {
		return (Choice::Fallback, soonest(blocked_deadline, lease_wakeup(now, &state)));
	}

	let mut workload_runnable = [false; 3];
	for &(_, class, _) in &runnable[..n] {
		if let Some(index) = workload_index(class) {
			workload_runnable[index] = true;
		}
	}
	clamp_woken(&mut state, workload_runnable);

	let runnable = &runnable[..n];
	let chosen = if system_due(&state, runnable) {
		pick_rr(&state, runnable, CLASS_SYSTEM).or_else(|| pick_workload(&state, runnable))
	} else {
		pick_workload(&state, runnable)
	};
	let Some((id, class)) = chosen else {
		return (Choice::Fallback, soonest(blocked_deadline, lease_wakeup(now, &state)));
	};
	if let Some(index) = rr_index(class) {
		if let Some(ticket) = ticket_of(&state, id) {
			state.rr[index] = ticket;
		}
	}
	let choice = if id == current_id && current_running {
		Choice::Stay
	} else {
		Choice::Switch(id)
	};
	let preempt_at = preempt_at(now, blocked_deadline, &state, class);
	(choice, Some(preempt_at))
}

fn preempt_at(now: u64, blocked_deadline: Option<u64>, state: &State, class: u8) -> u64 {
	let mut slice = QUANTUM_US;
	if class == CLASS_SYSTEM || class == 0 {
		let remain = SYSTEM_BUDGET_US.saturating_sub(state.system_charged);
		if remain > 0 {
			slice = slice.min(remain);
		}
	}
	let mut when = now.saturating_add(slice);
	if let Some(blocked) = blocked_deadline {
		when = when.min(blocked);
	}
	if state.lease_until > now {
		when = when.min(state.lease_until);
	}
	when
}

/// After the current slice has been charged, an elapsed lease installs
/// balanced weights unless an emergency override is already in force.
/// Virtual runtime is left in place either way.
fn enforce_lease(state: &mut State, now: u64) {
	if state.lease_until == 0 || now < state.lease_until {
		return;
	}
	state.staged.occupied = false;
	state.lease_until = 0;
	state.lease_expired = true;
	if state.emergency {
		return;
	}
	let previous = state.generation;
	state.weights = [1, 1, 1];
	state.generation = state.generation.saturating_add(1);
	let generation = state.generation;
	push_ack(
		state,
		SchedAck {
			kind: ACK_LEASE_EXPIRED,
			reason: ACK_REASON_NONE,
			profile: PROFILE_BALANCED,
			previous_generation: previous,
			generation,
			guest_us: now,
			lease_until_guest_us: 0,
		},
	);
}

/// Install a staged profile after the outgoing slice has been charged.
/// An emergency, a stale base generation, or a missed acceptance deadline
/// drops the stage. Generation does not move on a drop.
fn activate_staged(state: &mut State, now: u64) {
	if !state.staged.occupied {
		return;
	}
	if state.emergency {
		reject_staged(state, now, ACK_REASON_EMERGENCY);
		return;
	}
	if state.staged.base_generation != state.generation {
		reject_staged(state, now, ACK_REASON_STALE);
		return;
	}
	if now > state.staged.accept_until {
		reject_staged(state, now, ACK_REASON_LATE);
		return;
	}
	let weights = state.staged.weights;
	let renewal = weights == state.weights;
	if !renewal
		&& state.dwell_armed
		&& now.saturating_sub(state.last_profile_change_us) < MIN_DWELL_US
	{
		reject_staged(state, now, ACK_REASON_DWELL);
		return;
	}
	let previous = state.generation;
	let lease_until = state.staged.lease_until;
	if !renewal {
		state.last_profile_change_us = now;
		state.dwell_armed = true;
	}
	state.weights = weights;
	state.lease_until = lease_until;
	state.lease_expired = false;
	state.generation = state.generation.saturating_add(1);
	state.staged.occupied = false;
	let generation = state.generation;
	push_ack(
		state,
		SchedAck {
			kind: ACK_APPLIED,
			reason: ACK_REASON_NONE,
			profile: profile_code(weights),
			previous_generation: previous,
			generation,
			guest_us: now,
			lease_until_guest_us: lease_until,
		},
	);
}

fn reject_staged(state: &mut State, now: u64, reason: u8) {
	let generation = state.generation;
	let profile = profile_code(state.weights);
	let lease_until = state.lease_until;
	state.staged.occupied = false;
	push_ack(
		state,
		SchedAck {
			kind: ACK_REJECTED,
			reason,
			profile,
			previous_generation: generation,
			generation,
			guest_us: now,
			lease_until_guest_us: lease_until,
		},
	);
}

fn profile_code(weights: [u32; 3]) -> u8 {
	match weights {
		[1, 1, 1] => PROFILE_BALANCED,
		[6, 2, 2] => PROFILE_LATENCY,
		[2, 6, 2] => PROFILE_THROUGHPUT,
		[2, 2, 6] => PROFILE_RECLAIM,
		_ => 255,
	}
}

fn push_ack(state: &mut State, ack: SchedAck) {
	if state.ack_len == ACK_CAP {
		state.ack_head = (state.ack_head + 1) % ACK_CAP;
		state.ack_len -= 1;
		state.acks_dropped = state.acks_dropped.saturating_add(1);
	}
	let index = (state.ack_head + state.ack_len) % ACK_CAP;
	state.acks[index] = ack;
	state.ack_len += 1;
}

fn lease_wakeup(now: u64, state: &State) -> Option<u64> {
	if state.lease_until > now {
		Some(state.lease_until)
	} else {
		None
	}
}

fn soonest(left: Option<u64>, right: Option<u64>) -> Option<u64> {
	match (left, right) {
		(Some(left), Some(right)) => Some(left.min(right)),
		(Some(left), None) => Some(left),
		(None, Some(right)) => Some(right),
		(None, None) => None,
	}
}

fn account(state: &mut State, now: u64, current_id: TaskId, current_class: u8, bill_current: bool) {
	if state.interval_start == 0 {
		state.interval_start = now;
	}
	if now.saturating_sub(state.interval_start) >= INTERVAL_US {
		state.interval_start = now;
		state.system_charged = 0;
	}
	if state.last_account_us == 0 {
		state.last_account_us = now;
		return;
	}
	let delta = now.saturating_sub(state.last_account_us);
	state.last_account_us = now;
	if delta == 0 || !bill_current {
		return;
	}
	if current_class == 0 {
		state.system_charged = state.system_charged.saturating_add(delta);
		return;
	}
	if let Some(member) = state
		.members
		.iter_mut()
		.find(|member| member.id == Some(current_id))
	{
		member.service_us = member.service_us.saturating_add(delta);
	}
	if current_class == CLASS_SYSTEM {
		state.system_service = state.system_service.saturating_add(delta);
		state.system_charged = state.system_charged.saturating_add(delta);
		return;
	}
	let Some(index) = workload_index(current_class) else {
		return;
	};
	state.group_service[index] = state.group_service[index].saturating_add(delta);
	let weight = u128::from(state.weights[index].max(1));
	let mut charge = u128::from(delta).saturating_mul(SCALE) / weight;
	if charge == 0 {
		charge = 1;
	}
	state.vr[index] = state.vr[index].saturating_add(charge);
	renormalize(state);
}

fn renormalize(state: &mut State) {
	let limit = u128::from(u64::MAX);
	if state.vr.iter().all(|value| *value <= limit) {
		return;
	}
	let Some(min) = state.vr.iter().copied().min() else {
		return;
	};
	if min == 0 {
		return;
	}
	for value in &mut state.vr {
		*value -= min;
	}
}

fn clamp_woken(state: &mut State, runnable: [bool; 3]) {
	for index in 0..3 {
		if runnable[index] && !state.was_runnable[index] {
			let mut min_other = None;
			for other in 0..3 {
				if other != index && runnable[other] {
					min_other = Some(match min_other {
						Some(value) => core::cmp::min(value, state.vr[other]),
						None => state.vr[other],
					});
				}
			}
			if let Some(min_other) = min_other {
				if state.vr[index] < min_other {
					state.vr[index] = min_other;
				}
			}
		}
		state.was_runnable[index] = runnable[index];
	}
}

fn system_due(state: &State, runnable: &[(TaskId, u8, bool)]) -> bool {
	state.system_charged < SYSTEM_BUDGET_US && runnable.iter().any(|(_, class, _)| is_system(*class))
}

fn is_system(class: u8) -> bool {
	class == CLASS_SYSTEM || class == 0
}

fn pick_workload(state: &State, runnable: &[(TaskId, u8, bool)]) -> Option<(TaskId, u8)> {
	let mut best: Option<(usize, u128)> = None;
	for index in 0..3 {
		let class = index as u8 + CLASS_LATENCY;
		if runnable.iter().any(|(_, candidate, _)| *candidate == class) {
			let vr = state.vr[index];
			best = Some(match best {
				Some((best_index, best_vr)) if best_vr < vr || (best_vr == vr && best_index < index) => {
					(best_index, best_vr)
				}
				_ => (index, vr),
			});
		}
	}
	let (index, _) = best?;
	pick_rr(state, runnable, index as u8 + CLASS_LATENCY)
}

fn pick_rr(state: &State, runnable: &[(TaskId, u8, bool)], class: u8) -> Option<(TaskId, u8)> {
	let index = rr_index(class)?;
	let cursor = state.rr[index];
	let mut wrapped: Option<(TaskId, u64)> = None;
	let mut after: Option<(TaskId, u64)> = None;
	for &(id, candidate, _) in runnable {
		if !in_class(candidate, class) {
			continue;
		}
		let Some(ticket) = ticket_of(state, id) else {
			continue;
		};
		if ticket > cursor {
			after = Some(match after {
				Some((best_id, best_ticket)) if best_ticket <= ticket => (best_id, best_ticket),
				_ => (id, ticket),
			});
		}
		wrapped = Some(match wrapped {
			Some((best_id, best_ticket)) if best_ticket <= ticket => (best_id, best_ticket),
			_ => (id, ticket),
		});
	}
	after.or(wrapped).map(|(id, _)| (id, class))
}

fn ticket_of(state: &State, id: TaskId) -> Option<u64> {
	if let Some(member) = state.members.iter().find(|member| member.id == Some(id)) {
		return Some(member.ticket);
	}
	Some(u64::from(id.into() as u32).saturating_add(0x1_0000))
}

fn in_class(candidate: u8, class: u8) -> bool {
	if class == CLASS_SYSTEM {
		is_system(candidate)
	} else {
		candidate == class
	}
}

fn rr_index(class: u8) -> Option<usize> {
	match class {
		CLASS_LATENCY => Some(0),
		CLASS_BATCH => Some(1),
		CLASS_MAINTENANCE => Some(2),
		CLASS_SYSTEM => Some(3),
		_ => None,
	}
}

fn workload_index(class: u8) -> Option<usize> {
	match class {
		CLASS_LATENCY => Some(0),
		CLASS_BATCH => Some(1),
		CLASS_MAINTENANCE => Some(2),
		_ => None,
	}
}

fn clear_member(state: &mut State, id: TaskId) {
	if let Some(member) = state.members.iter_mut().find(|member| member.id == Some(id)) {
		*member = Member {
			id: None,
			class: 0,
			ticket: 0,
			service_us: 0,
		};
	}
}
