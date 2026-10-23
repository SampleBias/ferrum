//! Opt-in hierarchical fair scheduler for registered threads.
//!
//! Virtual runtime is `delta_us * 1_048_576 / weight`, with a minimum
//! increment of one. System work may take 5 ms of each 100 ms before
//! workload groups run. Sleeping groups are clamped up to the minimum
//! virtual runtime of the other runnable groups. Weight changes keep the
//! accumulated virtual runtime.
//!
//! The wake mode is zero at boot. Bit 0 (`WAKE_CREDIT`) lets a waking group
//! sit up to `WAKE_CREDIT_SLICES` of its competitor's slices below that
//! minimum, so it reclaims at most that much of the share it left unused
//! while idle. Bit 1 (`WAKE_FIFO`) runs workload threads in queue order
//! inside their class: a thread that becomes runnable or yields joins the
//! back, and the running thread keeps its place and its slice until it has
//! taken `CLASS_TURN_US` of service.
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
/// A slice longer than the 10 ms expiry target includes time the vCPU was
/// stopped or descheduled after its 2 ms preemption timer had already elapsed.
const MAX_SLICE_US: u64 = 10_000;
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
pub const ACK_FALLBACK: u8 = 5;
pub const ACK_REASON_NONE: u8 = 0;
pub const ACK_REASON_EMERGENCY: u8 = 1;
pub const ACK_REASON_LATE: u8 = 2;
pub const ACK_REASON_STALE: u8 = 3;
pub const ACK_REASON_DWELL: u8 = 4;
pub const PROFILE_BALANCED: u8 = 0;
pub const PROFILE_LATENCY: u8 = 1;
pub const PROFILE_THROUGHPUT: u8 = 2;
pub const PROFILE_RECLAIM: u8 = 3;

const BALANCED_CACHE_BYTES: u64 = 67_108_864;
const THROUGHPUT_CACHE_BYTES: u64 = 100_663_296;
const RECLAIM_CACHE_BYTES: u64 = 33_554_432;
const BALANCED_INFLIGHT: [u16; 2] = [16, 8];
const LATENCY_INFLIGHT: [u16; 2] = [16, 4];
const THROUGHPUT_INFLIGHT: [u16; 2] = [8, 16];
const RECLAIM_INFLIGHT: [u16; 2] = [4, 2];
const COALESCED_QUANTUM_US: u64 = 8_000;
pub const WAKE_CREDIT: u8 = 1;
pub const WAKE_FIFO: u8 = 2;
const WAKE_CREDIT_SLICES: u64 = 4;
const CLASS_TURN_US: u64 = 2 * QUANTUM_US;

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
	/// Runnable at the previous scheduling entry.
	queued: bool,
	/// Service since this thread last took a place at the back of its class.
	turn_us: u64,
}

const EMPTY_MEMBER: Member = Member {
	id: None,
	class: 0,
	ticket: 0,
	service_us: 0,
	queued: false,
	turn_us: 0,
};

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

#[derive(Clone, Copy)]
pub struct JointStatus {
	pub profile: u8,
	pub override_active: u8,
	pub tickless_enabled: u8,
	pub inflight_latency: u16,
	pub inflight_batch: u16,
	pub weights: [u32; 3],
	pub generation: u64,
	pub cache_soft_target_bytes: u64,
	pub lease_until_guest_us: u64,
	pub idle_residency_us: u64,
	pub timer_wakeups: u64,
	pub coalesced_wakeups: u64,
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
	cache_soft_target_bytes: u64,
	inflight_limits: [u16; 2],
	accept_until: u64,
	lease_until: u64,
	base_generation: u64,
}

struct State {
	members: [Member; MAX_MEMBERS],
	weights: [u32; 3],
	cache_soft_target_bytes: u64,
	inflight_limits: [u16; 2],
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
	/// Next scheduler or lease wakeup armed by `decide`. Zero means none.
	next_timer_us: u64,
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
	/// Tickless idle coalescing (bit 0) and single-thread slice coalescing (bit 1).
	tickless_mode: u8,
	/// `WAKE_CREDIT` and `WAKE_FIFO` bits.
	wake_mode: u8,
	/// End of the running thread's slice, measured from its dispatch.
	slice_end_us: u64,
	/// The running thread asked to yield since the previous scheduling entry.
	yielded: bool,
	idle_residency_us: u64,
	timer_wakeups: u64,
	coalesced_wakeups: u64,
	staged: Pending,
	ack_head: usize,
	ack_len: usize,
	acks_dropped: u64,
	acks: [SchedAck; ACK_CAP],
}

static STATE: hermit_sync::InterruptTicketMutex<State> = hermit_sync::InterruptTicketMutex::new(
	State {
		members: [EMPTY_MEMBER; MAX_MEMBERS],
		weights: [1, 1, 1],
		cache_soft_target_bytes: BALANCED_CACHE_BYTES,
		inflight_limits: BALANCED_INFLIGHT,
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
		next_timer_us: 0,
		lease_expired: false,
		emergency: false,
		generation: 1,
		last_profile_change_us: 0,
		dwell_armed: false,
		tickless_mode: 1,
		wake_mode: 0,
		slice_end_us: 0,
		yielded: false,
		idle_residency_us: 0,
		timer_wakeups: 0,
		coalesced_wakeups: 0,
		staged: Pending {
			occupied: false,
			weights: [0, 0, 0],
			cache_soft_target_bytes: BALANCED_CACHE_BYTES,
			inflight_limits: BALANCED_INFLIGHT,
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
	state.members[index] = Member {
		id: Some(id),
		class,
		ticket,
		..EMPTY_MEMBER
	};
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
	let weights = [latency, batch, maintenance];
	let (cache, inflight) = default_joint_params(weights);
	state.weights = weights;
	state.cache_soft_target_bytes = cache;
	state.inflight_limits = inflight;
	0
}

pub fn set_tickless(mode: u8) -> i32 {
	if mode > 3 {
		return -1;
	}
	let mut state = STATE.lock();
	state.tickless_mode = mode;
	0
}

pub fn set_wake(mode: u8) -> i32 {
	if mode > (WAKE_CREDIT | WAKE_FIFO) {
		return -1;
	}
	STATE.lock().wake_mode = mode;
	0
}

/// The running thread is about to give up the CPU while still runnable.
pub fn note_yield() {
	STATE.lock().yielded = true;
}

pub fn joint_status() -> JointStatus {
	let state = STATE.lock();
	JointStatus {
		profile: profile_code(state.weights),
		override_active: u8::from(state.emergency),
		tickless_enabled: state.tickless_mode,
		inflight_latency: state.inflight_limits[0],
		inflight_batch: state.inflight_limits[1],
		weights: state.weights,
		generation: state.generation,
		cache_soft_target_bytes: state.cache_soft_target_bytes,
		lease_until_guest_us: state.lease_until,
		idle_residency_us: state.idle_residency_us,
		timer_wakeups: state.timer_wakeups,
		coalesced_wakeups: state.coalesced_wakeups,
	}
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
/// A second call leaves the override in place.
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
	state.cache_soft_target_bytes = RECLAIM_CACHE_BYTES;
	state.inflight_limits = RECLAIM_INFLIGHT;
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

/// Clear emergency override after pressure hysteresis is satisfied, restoring
/// balanced fallback and incrementing generation.
pub fn clear_emergency(now: u64) -> i32 {
	let mut state = STATE.lock();
	if !state.emergency {
		return -1;
	}
	let previous = state.generation;
	state.emergency = false;
	state.staged.occupied = false;
	state.lease_until = 0;
	state.weights = [1, 1, 1];
	state.cache_soft_target_bytes = BALANCED_CACHE_BYTES;
	state.inflight_limits = BALANCED_INFLIGHT;
	state.last_profile_change_us = now;
	state.generation = state.generation.saturating_add(1);
	let generation = state.generation;
	push_ack(
		&mut state,
		SchedAck {
			kind: ACK_FALLBACK,
			reason: ACK_REASON_NONE,
			profile: PROFILE_BALANCED,
			previous_generation: previous,
			generation,
			guest_us: now,
			lease_until_guest_us: 0,
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
	let weights = [latency, batch, maintenance];
	let (cache, inflight) = default_joint_params(weights);
	stage_joint(
		now,
		latency,
		batch,
		maintenance,
		cache,
		inflight[0],
		inflight[1],
		accept_us,
		lease_us,
		base_generation,
	)
}

#[allow(clippy::too_many_arguments)]
pub fn stage_joint(
	now: u64,
	latency: u32,
	batch: u32,
	maintenance: u32,
	cache_soft_target_bytes: u64,
	inflight_latency: u16,
	inflight_batch: u16,
	accept_us: u64,
	lease_us: u64,
	base_generation: u64,
) -> i32 {
	if latency == 0
		|| batch == 0
		|| maintenance == 0
		|| cache_soft_target_bytes == 0
		|| inflight_latency == 0
		|| inflight_batch == 0
		|| lease_us == 0
	{
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
		cache_soft_target_bytes,
		inflight_limits: [inflight_latency, inflight_batch],
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
	let yielded = core::mem::take(&mut state.yielded);
	if !state.active {
		enforce_lease(&mut state, now);
		activate_staged(&mut state, now);
		let wakeup = soonest(blocked_deadline, lease_wakeup(now, &state));
		state.next_timer_us = wakeup.unwrap_or(0);
		return (Choice::Fallback, wakeup);
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
		let wakeup = if state.tickless_mode & 1 != 0 {
			soonest(blocked_deadline, lease_wakeup(now, &state))
		} else {
			let periodic = now.saturating_add(QUANTUM_US);
			Some(
				soonest(blocked_deadline, lease_wakeup(now, &state))
					.map_or(periodic, |deadline| deadline.min(periodic)),
			)
		};
		state.next_timer_us = wakeup.unwrap_or(0);
		if wakeup.is_some() {
			state.timer_wakeups = state.timer_wakeups.saturating_add(1);
		}
		return (Choice::Fallback, wakeup);
	}

	let mut workload_runnable = [false; 3];
	for &(_, class, _) in &runnable[..n] {
		if let Some(index) = workload_index(class) {
			workload_runnable[index] = true;
		}
	}
	clamp_woken(&mut state, workload_runnable);

	let runnable = &runnable[..n];
	let fifo = state.wake_mode & WAKE_FIFO != 0;
	if fifo {
		let slice_over = now >= state.slice_end_us;
		let current = current_running.then_some(current_id);
		requeue(&mut state, current, yielded, slice_over, runnable);
	}
	let chosen = if system_due(&state, runnable) {
		pick_rr(&state, runnable, CLASS_SYSTEM).or_else(|| pick_workload(&state, runnable))
	} else {
		pick_workload(&state, runnable)
	};
	let Some((id, class)) = chosen else {
		let wakeup = soonest(blocked_deadline, lease_wakeup(now, &state));
		state.next_timer_us = wakeup.unwrap_or(0);
		return (Choice::Fallback, wakeup);
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
	let keep_slice = fifo && !yielded && choice == Choice::Stay && now < state.slice_end_us;
	let (preempt_at, coalesced) = if keep_slice {
		state.slice_end_us = state.slice_end_us.min(now.saturating_add(QUANTUM_US));
		(deadline(now, blocked_deadline, &state, state.slice_end_us), 0)
	} else {
		let (slice_end, preempt_at, coalesced) = preempt_at(now, blocked_deadline, &state, class, n);
		state.slice_end_us = slice_end;
		(preempt_at, coalesced)
	};
	state.next_timer_us = preempt_at;
	state.timer_wakeups = state.timer_wakeups.saturating_add(1);
	state.coalesced_wakeups = state.coalesced_wakeups.saturating_add(coalesced);
	(choice, Some(preempt_at))
}

/// Preserve the active preemption or lease deadline when another subsystem
/// re-arms the APIC oneshot timer between scheduler entries.
pub fn timer_deadline(wakeup: Option<u64>) -> Option<u64> {
	let state = STATE.lock();
	let next = (state.next_timer_us > 0).then_some(state.next_timer_us);
	let lease = (state.lease_until > 0).then_some(state.lease_until);
	soonest(wakeup, soonest(next, lease))
}

fn preempt_at(
	now: u64,
	blocked_deadline: Option<u64>,
	state: &State,
	class: u8,
	runnable_count: usize,
) -> (u64, u64, u64) {
	let mut slice = if state.tickless_mode & 2 != 0
		&& runnable_count == 1
		&& class != CLASS_SYSTEM
		&& class != 0
	{
		COALESCED_QUANTUM_US
	} else {
		QUANTUM_US
	};
	if class == CLASS_SYSTEM || class == 0 {
		let remain = SYSTEM_BUDGET_US.saturating_sub(state.system_charged);
		if remain > 0 {
			slice = slice.min(remain);
		}
	}
	let slice_end = now.saturating_add(slice);
	let when = deadline(now, blocked_deadline, state, slice_end);
	let span = when.saturating_sub(now);
	let coalesced = if state.tickless_mode & 2 != 0 && span > QUANTUM_US {
		(span / QUANTUM_US).saturating_sub(1)
	} else {
		0
	};
	(slice_end, when, coalesced)
}

fn deadline(now: u64, blocked_deadline: Option<u64>, state: &State, slice_end: u64) -> u64 {
	let mut when = slice_end;
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
	state.cache_soft_target_bytes = BALANCED_CACHE_BYTES;
	state.inflight_limits = BALANCED_INFLIGHT;
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
	let cache_target = state.staged.cache_soft_target_bytes;
	let inflight = state.staged.inflight_limits;
	let renewal = weights == state.weights
		&& cache_target == state.cache_soft_target_bytes
		&& inflight == state.inflight_limits;
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
	state.cache_soft_target_bytes = cache_target;
	state.inflight_limits = inflight;
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

fn default_joint_params(weights: [u32; 3]) -> (u64, [u16; 2]) {
	match weights {
		[6, 2, 2] => (BALANCED_CACHE_BYTES, LATENCY_INFLIGHT),
		[2, 6, 2] => (THROUGHPUT_CACHE_BYTES, THROUGHPUT_INFLIGHT),
		[2, 2, 6] => (RECLAIM_CACHE_BYTES, RECLAIM_INFLIGHT),
		_ => (BALANCED_CACHE_BYTES, BALANCED_INFLIGHT),
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
	let raw_delta = now.saturating_sub(state.last_account_us);
	state.last_account_us = now;
	if raw_delta == 0 {
		return;
	}
	if !bill_current {
		state.idle_residency_us = state.idle_residency_us.saturating_add(raw_delta);
		if state.tickless_mode & 1 != 0 && raw_delta >= QUANTUM_US {
			state.coalesced_wakeups = state
				.coalesced_wakeups
				.saturating_add(raw_delta / QUANTUM_US);
		}
		return;
	}
	let delta = if raw_delta > MAX_SLICE_US {
		QUANTUM_US
	} else {
		raw_delta
	};
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
		member.turn_us = member.turn_us.saturating_add(delta);
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
			let mut min_other: Option<(u128, usize)> = None;
			for other in 0..3 {
				if other != index
					&& runnable[other]
					&& min_other.is_none_or(|(value, _)| state.vr[other] < value)
				{
					min_other = Some((state.vr[other], other));
				}
			}
			if let Some((min_other, other)) = min_other {
				let credit = if state.wake_mode & WAKE_CREDIT != 0 {
					u128::from(WAKE_CREDIT_SLICES * QUANTUM_US) * SCALE
						/ u128::from(state.weights[other].max(1))
				} else {
					0
				};
				let floor = min_other.saturating_sub(credit);
				if state.vr[index] < floor {
					state.vr[index] = floor;
				}
			}
		}
		state.was_runnable[index] = runnable[index];
	}
}

/// A workload member that was not runnable at the previous entry takes a
/// place at the back of its class, and so does the running thread when it
/// yields or its slice ends with a full turn of service behind it.
fn requeue(
	state: &mut State,
	current: Option<TaskId>,
	yielded: bool,
	slice_over: bool,
	runnable: &[(TaskId, u8, bool)],
) {
	for slot in 0..MAX_MEMBERS {
		let member = state.members[slot];
		let Some(id) = member.id else {
			continue;
		};
		if member.class == CLASS_SYSTEM {
			continue;
		}
		if !runnable.iter().any(|(candidate, _, _)| *candidate == id) {
			state.members[slot].queued = false;
			continue;
		}
		let turn_over = current == Some(id)
			&& (yielded || (slice_over && member.turn_us >= CLASS_TURN_US));
		if !member.queued || turn_over {
			state.members[slot].ticket = state.next_ticket;
			state.members[slot].turn_us = 0;
			state.next_ticket = state.next_ticket.saturating_add(1);
		}
		state.members[slot].queued = true;
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

/// Under `WAKE_FIFO`, workload classes take the oldest queue place instead
/// of the next ticket after the cursor.
fn pick_rr(state: &State, runnable: &[(TaskId, u8, bool)], class: u8) -> Option<(TaskId, u8)> {
	let index = rr_index(class)?;
	let cursor = if state.wake_mode & WAKE_FIFO != 0 && class != CLASS_SYSTEM {
		u64::MAX
	} else {
		state.rr[index]
	};
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
		*member = EMPTY_MEMBER;
	}
}
