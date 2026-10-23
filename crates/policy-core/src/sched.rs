//! Reference model of the opt-in hierarchical fair scheduler.
//!
//! Virtual runtime is charged as `delta_us * scale / weight` with a minimum
//! increment of one. Sleeping groups are clamped up to the minimum virtual
//! runtime of other runnable groups so they cannot bank credit. System work
//! takes at most 5 ms of each 100 ms interval before workload groups run.
//! This model is evidence for the algorithm, not a substitute for the kernel port.
//!
//! The wake mode mirrors `sys_policy_set_wake`. `WAKE_CREDIT` lets a waking
//! group sit up to `WAKE_CREDIT_SLICES` of its competitor's slices below the
//! clamp. `WAKE_FIFO` keeps workload threads in queue order inside their
//! class: a woken or yielding thread joins the back, and a thread keeps its
//! place across slice ends until it has taken `CLASS_TURN_US` of service.

use policy_types::{
    GroupId, ProfileId, ACCOUNTING_INTERVAL_US, QUANTUM_US, SYSTEM_RESERVATION_US,
};

use crate::catalog::catalog_spec;
use policy_types::CatalogId;

pub const VRUNTIME_SCALE: u128 = 1_048_576;
pub const COALESCED_QUANTUM_US: u64 = 8_000;
pub const WAKE_CREDIT: u8 = 1;
pub const WAKE_FIFO: u8 = 2;
pub const WAKE_CREDIT_SLICES: u64 = 4;
pub const CLASS_TURN_US: u64 = 2 * QUANTUM_US;
const MAX_THREADS: usize = 12;
const MAX_WORKLOAD: usize = 8;
const MAX_SYSTEM: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedError {
    Closed,
    TooMany,
    BadId,
    NotRunnable,
    NotBlocked,
    TimeReversal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Empty,
    Runnable,
    Blocked,
    Exited,
}

#[derive(Clone, Copy, Debug)]
struct Thread {
    state: State,
    class: GroupId,
    ticket: u64,
    service_us: u64,
    last_cpu_end: u64,
    turn_us: u64,
}

impl Thread {
    const fn empty() -> Self {
        Self {
            state: State::Empty,
            class: GroupId::System,
            ticket: 0,
            service_us: 0,
            last_cpu_end: 0,
            turn_us: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FairScheduler {
    threads: [Thread; MAX_THREADS],
    vr: [u128; 3],
    weights: [u32; 3],
    group_service: [u64; 3],
    system_service: u64,
    profile: ProfileId,
    now: u64,
    interval_start: u64,
    system_charged: u64,
    running: Option<usize>,
    slice_start: u64,
    next_ticket: u64,
    registration_open: bool,
    max_gap: u64,
    system_deferrals: u64,
    system_demand_exceeded: bool,
    tickless_mode: u8,
    wake_mode: u8,
    idle_residency_us: u64,
    timer_wakeups: u64,
    coalesced_wakeups: u64,
}

impl FairScheduler {
    pub fn new() -> Self {
        let mut sched = Self {
            threads: [Thread::empty(); MAX_THREADS],
            vr: [0; 3],
            weights: [1, 1, 1],
            group_service: [0; 3],
            system_service: 0,
            profile: ProfileId::Balanced,
            now: 0,
            interval_start: 0,
            system_charged: 0,
            running: None,
            slice_start: 0,
            next_ticket: 0,
            registration_open: true,
            max_gap: 0,
            system_deferrals: 0,
            system_demand_exceeded: false,
            tickless_mode: 1,
            wake_mode: 0,
            idle_residency_us: 0,
            timer_wakeups: 0,
            coalesced_wakeups: 0,
        };
        sched.install_weights(ProfileId::Balanced);
        sched
    }

    pub fn set_tickless_mode(&mut self, mode: u8) {
        self.tickless_mode = mode & 3;
    }

    pub fn set_wake_mode(&mut self, mode: u8) {
        self.wake_mode = mode & (WAKE_CREDIT | WAKE_FIFO);
    }

    pub fn idle_residency_us(&self) -> u64 {
        self.idle_residency_us
    }

    pub fn timer_wakeups(&self) -> u64 {
        self.timer_wakeups
    }

    pub fn coalesced_wakeups(&self) -> u64 {
        self.coalesced_wakeups
    }

    pub fn now_us(&self) -> u64 {
        self.now
    }

    pub fn profile(&self) -> ProfileId {
        self.profile
    }

    pub fn service_us(&self, id: u8) -> Result<u64, SchedError> {
        Ok(self.thread(id)?.service_us)
    }

    pub fn group_service_us(&self, class: GroupId) -> u64 {
        match workload_index(class) {
            Some(index) => self.group_service[index],
            None => self.system_service,
        }
    }

    pub fn group_vruntime(&self, class: GroupId) -> u128 {
        self.vr[workload_index(class).expect("workload group")]
    }

    pub fn max_dispatch_gap_us(&self) -> u64 {
        self.max_gap
    }

    pub fn system_demand_exceeded(&self) -> bool {
        self.system_demand_exceeded
    }

    pub fn system_deferrals(&self) -> u64 {
        self.system_deferrals
    }

    pub fn close_registration(&mut self) {
        self.registration_open = false;
    }

    pub fn spawn(&mut self, class: GroupId) -> Result<u8, SchedError> {
        if !self.registration_open {
            return Err(SchedError::Closed);
        }
        if class.is_workload() && self.live(true) >= MAX_WORKLOAD {
            return Err(SchedError::TooMany);
        }
        if !class.is_workload() && self.live(false) >= MAX_SYSTEM {
            return Err(SchedError::TooMany);
        }
        let index = self
            .threads
            .iter()
            .position(|thread| thread.state == State::Empty)
            .ok_or(SchedError::TooMany)?;
        self.threads[index] = Thread {
            state: State::Runnable,
            class,
            ticket: self.alloc_ticket(),
            service_us: 0,
            last_cpu_end: self.now,
            turn_us: 0,
        };
        Ok(index as u8)
    }

    pub fn advance_to(&mut self, target: u64) {
        assert!(target >= self.now, "scheduler time went backwards");
        while self.now < target {
            if self.running.is_none() {
                match self.pick() {
                    Some(id) => self.dispatch(id),
                    None => {
                        let idle_delta = target - self.now;
                        self.idle_residency_us = self.idle_residency_us.saturating_add(idle_delta);
                        if self.tickless_mode & 1 != 0 {
                            self.coalesced_wakeups = self
                                .coalesced_wakeups
                                .saturating_add(idle_delta / QUANTUM_US);
                        } else {
                            self.timer_wakeups = self
                                .timer_wakeups
                                .saturating_add(idle_delta.div_ceil(QUANTUM_US));
                        }
                        self.now = target;
                        self.realign_interval();
                        return;
                    }
                }
            }
            let id = self.running.expect("a thread was dispatched");
            let allowed = self.allowed_slice(id);
            assert!(allowed > 0, "dispatched a thread with an empty slice");
            let quantum_end = self.slice_start + allowed;
            let interval_end = self.interval_start + ACCOUNTING_INTERVAL_US;
            let stop = target.min(quantum_end).min(interval_end);
            assert!(stop > self.now, "scheduler made no progress");
            self.account(id, stop - self.now);
            self.now = stop;
            if self.now == interval_end {
                self.interval_start = interval_end;
                self.system_charged = 0;
            }
            if self.now == quantum_end || self.now == interval_end {
                self.finish_slice(id);
            }
        }
    }

    pub fn block(&mut self, id: u8, at_us: u64) -> Result<(), SchedError> {
        self.seek(at_us)?;
        let index = id as usize;
        if self.threads[index].state != State::Runnable {
            return Err(SchedError::NotRunnable);
        }
        self.leave_cpu(index);
        self.threads[index].state = State::Blocked;
        Ok(())
    }

    pub fn wake(&mut self, id: u8, at_us: u64) -> Result<(), SchedError> {
        self.seek(at_us)?;
        let index = id as usize;
        if self.threads[index].state != State::Blocked {
            return Err(SchedError::NotBlocked);
        }
        let class = self.threads[index].class;
        let group_was_idle = self.runnable_in(class) == 0;
        self.threads[index].state = State::Runnable;
        self.threads[index].last_cpu_end = self.now;
        if self.fifo(class) {
            self.requeue(index);
        }
        if group_was_idle {
            self.clamp_group(class);
        }
        Ok(())
    }

    pub fn yield_thread(&mut self, id: u8, at_us: u64) -> Result<(), SchedError> {
        self.seek(at_us)?;
        let index = id as usize;
        if self.threads[index].state != State::Runnable {
            return Err(SchedError::NotRunnable);
        }
        self.leave_cpu(index);
        self.requeue(index);
        Ok(())
    }

    pub fn exit(&mut self, id: u8, at_us: u64) -> Result<(), SchedError> {
        self.seek(at_us)?;
        let index = id as usize;
        if matches!(self.threads[index].state, State::Empty | State::Exited) {
            return Err(SchedError::BadId);
        }
        self.leave_cpu(index);
        self.threads[index].state = State::Exited;
        Ok(())
    }

    pub fn set_profile(&mut self, profile: ProfileId, at_us: u64) -> Result<(), SchedError> {
        self.seek(at_us)?;
        self.install_weights(profile);
        Ok(())
    }

    /// Model a hypervisor stop/resume or severe host deschedule of `duration_us`
    /// while the guest clock (`rdtsc`) keeps advancing. At most the active
    /// slice's remaining quantum is billed to the interrupted thread.
    pub fn pause(&mut self, duration_us: u64) {
        if duration_us == 0 {
            return;
        }
        if let Some(id) = self.running {
            let allowed = self.allowed_slice(id);
            let elapsed = self.now.saturating_sub(self.slice_start);
            let billable = allowed.saturating_sub(elapsed).min(duration_us);
            self.account(id, billable);
            self.now = self.now.saturating_add(duration_us);
            self.realign_interval();
            self.finish_slice(id);
            for thread in &mut self.threads {
                if thread.state == State::Runnable {
                    thread.last_cpu_end = self.now;
                }
            }
        } else {
            self.now = self.now.saturating_add(duration_us);
            self.realign_interval();
        }
    }

    fn seek(&mut self, at_us: u64) -> Result<(), SchedError> {
        if at_us < self.now {
            return Err(SchedError::TimeReversal);
        }
        self.advance_to(at_us);
        Ok(())
    }

    fn install_weights(&mut self, profile: ProfileId) {
        let params = catalog_spec(CatalogId::CpuV1).profile(profile);
        self.profile = profile;
        self.weights = params.weights();
    }

    fn alloc_ticket(&mut self) -> u64 {
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        ticket
    }

    fn requeue(&mut self, index: usize) {
        self.threads[index].ticket = self.alloc_ticket();
        self.threads[index].turn_us = 0;
    }

    fn fifo(&self, class: GroupId) -> bool {
        self.wake_mode & WAKE_FIFO != 0 && class.is_workload()
    }

    fn live(&self, workload: bool) -> usize {
        self.threads
            .iter()
            .filter(|thread| {
                !matches!(thread.state, State::Empty | State::Exited)
                    && thread.class.is_workload() == workload
            })
            .count()
    }

    fn thread(&self, id: u8) -> Result<&Thread, SchedError> {
        self.threads
            .get(id as usize)
            .filter(|thread| thread.state != State::Empty)
            .ok_or(SchedError::BadId)
    }

    fn runnable_in(&self, class: GroupId) -> usize {
        self.threads
            .iter()
            .filter(|thread| thread.state == State::Runnable && thread.class == class)
            .count()
    }

    fn pick(&mut self) -> Option<usize> {
        if self.system_charged < SYSTEM_RESERVATION_US && self.runnable_in(GroupId::System) > 0 {
            return Some(self.pick_rr(GroupId::System));
        }
        let mut best: Option<(u128, u8, GroupId)> = None;
        for class in [GroupId::Latency, GroupId::Batch, GroupId::Maintenance] {
            if self.runnable_in(class) == 0 {
                continue;
            }
            let vr = self.vr[workload_index(class).expect("workload")];
            let key = (vr, class as u8);
            if best.map(|current| key < (current.0, current.1)).unwrap_or(true) {
                best = Some((vr, class as u8, class));
            }
        }
        best.map(|(_, _, class)| self.pick_rr(class))
    }

    fn pick_rr(&mut self, class: GroupId) -> usize {
        let mut best: Option<(u64, usize)> = None;
        for (index, thread) in self.threads.iter().enumerate() {
            if thread.state == State::Runnable && thread.class == class {
                match best {
                    Some((ticket, _)) if thread.ticket >= ticket => {}
                    _ => best = Some((thread.ticket, index)),
                }
            }
        }
        let index = best.expect("class had a runnable thread").1;
        if !self.fifo(class) {
            self.threads[index].ticket = self.alloc_ticket();
        }
        index
    }

    fn total_runnable(&self) -> usize {
        self.threads
            .iter()
            .filter(|thread| thread.state == State::Runnable)
            .count()
    }

    fn dispatch(&mut self, id: usize) {
        let gap = self.now.saturating_sub(self.threads[id].last_cpu_end);
        if gap > self.max_gap {
            self.max_gap = gap;
        }
        self.running = Some(id);
        self.slice_start = self.now;
        self.timer_wakeups = self.timer_wakeups.saturating_add(1);
        let allowed = self.allowed_slice(id);
        if self.tickless_mode & 2 != 0 && allowed > QUANTUM_US {
            self.coalesced_wakeups = self
                .coalesced_wakeups
                .saturating_add((allowed / QUANTUM_US).saturating_sub(1));
        }
    }

    fn allowed_slice(&self, id: usize) -> u64 {
        if self.threads[id].class == GroupId::System {
            SYSTEM_RESERVATION_US
                .saturating_sub(self.system_charged)
                .min(QUANTUM_US)
        } else if self.tickless_mode & 2 != 0 && self.total_runnable() == 1 {
            COALESCED_QUANTUM_US
        } else {
            QUANTUM_US
        }
    }

    fn account(&mut self, id: usize, delta: u64) {
        if delta == 0 {
            return;
        }
        self.threads[id].service_us = self.threads[id].service_us.saturating_add(delta);
        self.threads[id].turn_us = self.threads[id].turn_us.saturating_add(delta);
        if self.threads[id].class == GroupId::System {
            self.system_charged = self.system_charged.saturating_add(delta);
            self.system_service = self.system_service.saturating_add(delta);
            return;
        }
        let index = workload_index(self.threads[id].class).expect("workload");
        self.group_service[index] = self.group_service[index].saturating_add(delta);
        self.charge(index, delta);
    }

    fn charge(&mut self, index: usize, delta_us: u64) {
        let weight = u128::from(self.weights[index].max(1));
        let increment = (u128::from(delta_us) * VRUNTIME_SCALE / weight).max(1);
        self.vr[index] = self.vr[index].saturating_add(increment);
        self.renormalize_if_large();
    }

    fn renormalize_if_large(&mut self) {
        let limit = u128::from(u64::MAX);
        if self.vr.iter().all(|value| *value <= limit) {
            return;
        }
        let min = self.vr.iter().copied().min().unwrap_or(0);
        if min == 0 {
            return;
        }
        for value in &mut self.vr {
            *value -= min;
        }
    }

    fn finish_slice(&mut self, id: usize) {
        if self.threads[id].class == GroupId::System && self.system_charged >= SYSTEM_RESERVATION_US
        {
            self.system_deferrals += 1;
            self.system_demand_exceeded = true;
        }
        self.threads[id].last_cpu_end = self.now;
        if !self.fifo(self.threads[id].class) || self.threads[id].turn_us >= CLASS_TURN_US {
            self.requeue(id);
        }
        self.running = None;
    }

    fn leave_cpu(&mut self, index: usize) {
        if self.running == Some(index) {
            self.threads[index].last_cpu_end = self.now;
            self.running = None;
        }
    }

    fn clamp_group(&mut self, class: GroupId) {
        let Some(index) = workload_index(class) else {
            return;
        };
        let mut min_other: Option<(u128, usize)> = None;
        for (other, vr) in self.vr.iter().enumerate() {
            if other == index {
                continue;
            }
            let other_class = match other {
                0 => GroupId::Latency,
                1 => GroupId::Batch,
                _ => GroupId::Maintenance,
            };
            if self.runnable_in(other_class) == 0 {
                continue;
            }
            if min_other.is_none_or(|(current, _)| *vr < current) {
                min_other = Some((*vr, other));
            }
        }
        if let Some((min_other, other)) = min_other {
            let credit = if self.wake_mode & WAKE_CREDIT != 0 {
                u128::from(WAKE_CREDIT_SLICES * QUANTUM_US) * VRUNTIME_SCALE
                    / u128::from(self.weights[other].max(1))
            } else {
                0
            };
            let floor = min_other.saturating_sub(credit);
            if self.vr[index] < floor {
                self.vr[index] = floor;
            }
        }
    }

    fn realign_interval(&mut self) {
        let new_start = self.now - (self.now % ACCOUNTING_INTERVAL_US);
        if new_start != self.interval_start {
            self.interval_start = new_start;
            self.system_charged = 0;
        }
    }
}

impl Default for FairScheduler {
    fn default() -> Self {
        Self::new()
    }
}

fn workload_index(class: GroupId) -> Option<usize> {
    match class {
        GroupId::Latency => Some(0),
        GroupId::Batch => Some(1),
        GroupId::Maintenance => Some(2),
        GroupId::System => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn_class(sched: &mut FairScheduler, class: GroupId, count: usize) -> Vec<u8> {
        (0..count).map(|_| sched.spawn(class).unwrap()).collect()
    }

    #[test]
    fn timer_preemption_shares_a_non_yielding_group() {
        let mut sched = FairScheduler::new();
        let ids = spawn_class(&mut sched, GroupId::Latency, 2);
        sched.advance_to(10_000);
        let a = sched.service_us(ids[0]).unwrap();
        let b = sched.service_us(ids[1]).unwrap();
        assert!(a > 0 && b > 0);
        assert_eq!(a + b, 10_000);
    }

    #[test]
    fn yield_charges_elapsed_time_only() {
        let mut sched = FairScheduler::new();
        let first = sched.spawn(GroupId::Latency).unwrap();
        let second = sched.spawn(GroupId::Latency).unwrap();
        sched.yield_thread(first, 100).unwrap();
        sched.advance_to(2_100);
        assert_eq!(sched.service_us(first).unwrap(), 100);
        assert_eq!(sched.service_us(second).unwrap(), 2_000);
    }

    #[test]
    fn sleeping_group_cannot_bank_credit() {
        let mut sched = FairScheduler::new();
        let latency = sched.spawn(GroupId::Latency).unwrap();
        let batch = sched.spawn(GroupId::Batch).unwrap();
        sched.block(latency, 0).unwrap();
        sched.advance_to(100_000);
        let batch_before = sched.service_us(batch).unwrap();
        sched.wake(latency, 100_000).unwrap();
        assert!(sched.group_vruntime(GroupId::Latency) >= sched.group_vruntime(GroupId::Batch));
        sched.advance_to(110_000);
        let batch_delta = sched.service_us(batch).unwrap() - batch_before;
        let latency_delta = sched.service_us(latency).unwrap();
        assert!(batch_delta > 0, "woken group took the whole window");
        assert!(latency_delta < 10_000);
    }

    #[test]
    fn weight_change_preserves_virtual_runtime() {
        let mut sched = FairScheduler::new();
        sched.spawn(GroupId::Latency).unwrap();
        sched.advance_to(4_000);
        let before = sched.group_vruntime(GroupId::Latency);
        sched.set_profile(ProfileId::Throughput, 4_000).unwrap();
        assert_eq!(sched.group_vruntime(GroupId::Latency), before);
        assert_eq!(sched.profile(), ProfileId::Throughput);
    }

    #[test]
    fn system_reservation_is_five_percent_when_demand_is_continuous() {
        let mut sched = FairScheduler::new();
        let system = sched.spawn(GroupId::System).unwrap();
        let work = sched.spawn(GroupId::Latency).unwrap();
        sched.advance_to(1_000_000);
        assert_eq!(sched.service_us(system).unwrap(), 50_000);
        assert_eq!(sched.service_us(work).unwrap(), 950_000);
        assert!(sched.system_demand_exceeded());
    }

    #[test]
    fn idle_system_budget_goes_to_workloads() {
        let mut sched = FairScheduler::new();
        let work = sched.spawn(GroupId::Batch).unwrap();
        sched.advance_to(100_000);
        assert_eq!(sched.service_us(work).unwrap(), 100_000);
        assert_eq!(sched.system_deferrals(), 0);
    }

    const WAKE_MODES: [u8; 4] = [0, WAKE_CREDIT, WAKE_FIFO, WAKE_CREDIT | WAKE_FIFO];

    #[test]
    fn runnable_threads_receive_service_within_500ms() {
        for mode in WAKE_MODES {
            let mut sched = FairScheduler::new();
            sched.set_wake_mode(mode);
            sched.set_profile(ProfileId::Latency, 0).unwrap();
            let mut ids = spawn_class(&mut sched, GroupId::Latency, 3);
            ids.extend(spawn_class(&mut sched, GroupId::Batch, 3));
            ids.extend(spawn_class(&mut sched, GroupId::Maintenance, 2));
            sched.close_registration();
            assert!(sched.spawn(GroupId::Latency).is_err());
            sched.advance_to(500_000);
            for id in ids {
                assert!(sched.service_us(id).unwrap() > 0, "mode {mode}");
            }
            assert!(sched.max_dispatch_gap_us() <= 500_000, "mode {mode}");
        }
    }

    #[test]
    fn profile_direction_follows_weights() {
        for mode in WAKE_MODES {
            let mut sched = FairScheduler::new();
            sched.set_wake_mode(mode);
            sched.spawn(GroupId::Latency).unwrap();
            sched.spawn(GroupId::Batch).unwrap();
            sched.spawn(GroupId::Maintenance).unwrap();
            sched.set_profile(ProfileId::Latency, 0).unwrap();
            sched.advance_to(2_000_000);
            let latency = sched.group_service_us(GroupId::Latency);
            let batch = sched.group_service_us(GroupId::Batch);
            let maintenance = sched.group_service_us(GroupId::Maintenance);
            assert!(latency > batch, "mode {mode}");
            assert!(latency > maintenance, "mode {mode}");
            assert!(latency > 1_000_000, "mode {mode}");

            sched.set_profile(ProfileId::Throughput, 2_000_000).unwrap();
            let base_l = latency;
            let base_b = batch;
            sched.advance_to(4_000_000);
            let d_latency = sched.group_service_us(GroupId::Latency) - base_l;
            let d_batch = sched.group_service_us(GroupId::Batch) - base_b;
            assert!(d_batch > d_latency, "mode {mode}");
            assert!(d_batch > 1_000_000, "mode {mode}");
        }
    }

    #[test]
    fn wake_credit_is_bounded_regardless_of_sleep_length() {
        for sleep_us in [100_000, 10_000_000] {
            for (mode, lead_us) in [(0, 0), (WAKE_CREDIT, WAKE_CREDIT_SLICES * QUANTUM_US)] {
                let mut sched = FairScheduler::new();
                sched.set_wake_mode(mode);
                let latency = sched.spawn(GroupId::Latency).unwrap();
                let batch = sched.spawn(GroupId::Batch).unwrap();
                sched.block(latency, 0).unwrap();
                sched.advance_to(sleep_us);
                let batch_before = sched.service_us(batch).unwrap();
                sched.wake(latency, sleep_us).unwrap();
                sched.advance_to(sleep_us + 40_000);
                let latency_got = sched.service_us(latency).unwrap();
                let batch_got = sched.service_us(batch).unwrap() - batch_before;
                assert_eq!(latency_got + batch_got, 40_000);
                assert_eq!(latency_got - batch_got, lead_us, "sleep {sleep_us} mode {mode}");
            }
        }
    }

    #[test]
    fn fifo_runs_a_full_turn_before_rotating() {
        for (mode, turn_us) in [(0, QUANTUM_US), (WAKE_FIFO, CLASS_TURN_US)] {
            let mut sched = FairScheduler::new();
            sched.set_wake_mode(mode);
            let ids = spawn_class(&mut sched, GroupId::Latency, 2);
            sched.advance_to(turn_us);
            assert_eq!(sched.service_us(ids[0]).unwrap(), turn_us, "mode {mode}");
            assert_eq!(sched.service_us(ids[1]).unwrap(), 0, "mode {mode}");
            sched.advance_to(2 * turn_us);
            assert_eq!(sched.service_us(ids[1]).unwrap(), turn_us, "mode {mode}");
        }
    }

    #[test]
    fn fifo_woken_thread_queues_behind_waiting_class_mates() {
        let mut sched = FairScheduler::new();
        sched.set_wake_mode(WAKE_FIFO);
        let [woken, running, waiting] = spawn_class(&mut sched, GroupId::Latency, 3)[..] else {
            unreachable!()
        };
        sched.block(woken, 0).unwrap();
        sched.wake(woken, 1_000).unwrap();
        sched.advance_to(2 * CLASS_TURN_US);
        assert_eq!(sched.service_us(running).unwrap(), CLASS_TURN_US);
        assert_eq!(sched.service_us(waiting).unwrap(), CLASS_TURN_US);
        assert_eq!(sched.service_us(woken).unwrap(), 0);
        sched.advance_to(3 * CLASS_TURN_US);
        assert_eq!(sched.service_us(woken).unwrap(), CLASS_TURN_US);
    }

    #[test]
    fn fifo_yield_gives_up_the_rest_of_the_turn() {
        let mut sched = FairScheduler::new();
        sched.set_wake_mode(WAKE_FIFO);
        let first = sched.spawn(GroupId::Latency).unwrap();
        let second = sched.spawn(GroupId::Latency).unwrap();
        sched.yield_thread(first, 1_000).unwrap();
        sched.advance_to(1_000 + CLASS_TURN_US);
        assert_eq!(sched.service_us(first).unwrap(), 1_000);
        assert_eq!(sched.service_us(second).unwrap(), CLASS_TURN_US);
    }

    #[test]
    fn blocked_thread_does_not_keep_running_across_a_profile_change() {
        let mut sched = FairScheduler::new();
        let latency = sched.spawn(GroupId::Latency).unwrap();
        let batch = sched.spawn(GroupId::Batch).unwrap();
        sched.block(latency, 1_000).unwrap();
        sched.set_profile(ProfileId::Throughput, 1_000).unwrap();
        sched.advance_to(20_000);
        assert_eq!(sched.service_us(latency).unwrap(), 1_000);
        assert_eq!(sched.service_us(batch).unwrap(), 19_000);
    }

    #[test]
    fn virtual_runtime_renormalizes_without_reordering() {
        let mut sched = FairScheduler::new();
        sched.vr = [u128::from(u64::MAX), u128::from(u64::MAX), u128::from(u64::MAX)];
        sched.charge(0, 2_000);
        assert!(sched.group_vruntime(GroupId::Latency) >= sched.group_vruntime(GroupId::Batch));
        assert!(sched.vr.iter().all(|value| *value <= u128::from(u64::MAX)));
    }

    #[test]
    fn vm_pause_does_not_bill_stopped_wall_time_or_starve_interrupted_group() {
        let mut sched = FairScheduler::new();
        let latency = sched.spawn(GroupId::Latency).unwrap();
        let batch = sched.spawn(GroupId::Batch).unwrap();
        let maint = sched.spawn(GroupId::Maintenance).unwrap();
        // Start a slice on latency, then pause the VM for 5.05 seconds.
        sched.advance_to(500);
        sched.pause(5_050_000);
        assert_eq!(sched.service_us(latency).unwrap(), QUANTUM_US);
        let before = (
            sched.service_us(latency).unwrap(),
            sched.service_us(batch).unwrap(),
            sched.service_us(maint).unwrap(),
        );
        let resumed_at = sched.now_us();
        sched.advance_to(resumed_at + 300_000);
        let d_lat = sched.service_us(latency).unwrap() - before.0;
        let d_bat = sched.service_us(batch).unwrap() - before.1;
        let d_mnt = sched.service_us(maint).unwrap() - before.2;
        assert!(d_lat >= 95_000 && d_bat >= 95_000 && d_mnt >= 95_000);
    }

    #[test]
    fn tickless_coalescing_reduces_wakeups_during_idle_and_solo_patrol() {
        let mut periodic = FairScheduler::new();
        periodic.set_tickless_mode(0);
        let sensor_p = periodic.spawn(GroupId::Latency).unwrap();
        periodic.block(sensor_p, 2_000).unwrap();
        periodic.advance_to(100_000);
        assert_eq!(periodic.idle_residency_us(), 98_000);
        assert_eq!(periodic.coalesced_wakeups(), 0);
        assert_eq!(periodic.timer_wakeups(), 50);

        let mut tickless = FairScheduler::new();
        tickless.set_tickless_mode(3);
        let sensor_t = tickless.spawn(GroupId::Latency).unwrap();
        tickless.block(sensor_t, 2_000).unwrap();
        tickless.advance_to(100_000);
        assert_eq!(tickless.idle_residency_us(), 98_000);
        assert_eq!(tickless.timer_wakeups(), 1);
        assert!(tickless.coalesced_wakeups() >= 49);

        // Waking a single solo thread under mode=3 coalesces 8 ms slices (4x fewer timer wakeups).
        tickless.wake(sensor_t, 100_000).unwrap();
        tickless.advance_to(116_000);
        assert_eq!(tickless.service_us(sensor_t).unwrap(), 18_000);
        assert_eq!(tickless.timer_wakeups(), 3);
    }
}
