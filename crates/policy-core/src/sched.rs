//! Reference model of the opt-in hierarchical fair scheduler.
//!
//! Virtual runtime is charged as `delta_us * scale / weight` with a minimum
//! increment of one. Sleeping groups are clamped up to the minimum virtual
//! runtime of other runnable groups so they cannot bank credit. System work
//! takes at most 5 ms of each 100 ms interval before workload groups run.
//! This model is evidence for the algorithm, not a substitute for the kernel port.

use policy_types::{
    GroupId, ProfileId, ACCOUNTING_INTERVAL_US, QUANTUM_US, SYSTEM_RESERVATION_US,
};

use crate::catalog::catalog_spec;
use policy_types::CatalogId;

pub const VRUNTIME_SCALE: u128 = 1_048_576;
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
}

impl Thread {
    const fn empty() -> Self {
        Self {
            state: State::Empty,
            class: GroupId::System,
            ticket: 0,
            service_us: 0,
            last_cpu_end: 0,
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
        };
        sched.install_weights(ProfileId::Balanced);
        sched
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
        self.threads[index].ticket = self.alloc_ticket();
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
        self.threads[index].ticket = self.alloc_ticket();
        index
    }

    fn dispatch(&mut self, id: usize) {
        let gap = self.now.saturating_sub(self.threads[id].last_cpu_end);
        if gap > self.max_gap {
            self.max_gap = gap;
        }
        self.running = Some(id);
        self.slice_start = self.now;
    }

    fn allowed_slice(&self, id: usize) -> u64 {
        if self.threads[id].class == GroupId::System {
            SYSTEM_RESERVATION_US
                .saturating_sub(self.system_charged)
                .min(QUANTUM_US)
        } else {
            QUANTUM_US
        }
    }

    fn account(&mut self, id: usize, delta: u64) {
        if delta == 0 {
            return;
        }
        self.threads[id].service_us = self.threads[id].service_us.saturating_add(delta);
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
        self.threads[id].ticket = self.alloc_ticket();
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
        let mut min_other: Option<u128> = None;
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
            min_other = Some(min_other.map(|current| current.min(*vr)).unwrap_or(*vr));
        }
        if let Some(min_other) = min_other {
            if self.vr[index] < min_other {
                self.vr[index] = min_other;
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

    #[test]
    fn runnable_threads_receive_service_within_500ms() {
        let mut sched = FairScheduler::new();
        sched.set_profile(ProfileId::Latency, 0).unwrap();
        let mut ids = spawn_class(&mut sched, GroupId::Latency, 3);
        ids.extend(spawn_class(&mut sched, GroupId::Batch, 3));
        ids.extend(spawn_class(&mut sched, GroupId::Maintenance, 2));
        sched.close_registration();
        assert!(sched.spawn(GroupId::Latency).is_err());
        sched.advance_to(500_000);
        for id in ids {
            assert!(sched.service_us(id).unwrap() > 0);
        }
        assert!(sched.max_dispatch_gap_us() <= 500_000);
    }

    #[test]
    fn profile_direction_follows_weights() {
        let mut sched = FairScheduler::new();
        sched.spawn(GroupId::Latency).unwrap();
        sched.spawn(GroupId::Batch).unwrap();
        sched.spawn(GroupId::Maintenance).unwrap();
        sched.set_profile(ProfileId::Latency, 0).unwrap();
        sched.advance_to(2_000_000);
        let latency = sched.group_service_us(GroupId::Latency);
        let batch = sched.group_service_us(GroupId::Batch);
        let maintenance = sched.group_service_us(GroupId::Maintenance);
        assert!(latency > batch);
        assert!(latency > maintenance);
        assert!(latency > 1_000_000);

        sched.set_profile(ProfileId::Throughput, 2_000_000).unwrap();
        let base_l = latency;
        let base_b = batch;
        sched.advance_to(4_000_000);
        let d_latency = sched.group_service_us(GroupId::Latency) - base_l;
        let d_batch = sched.group_service_us(GroupId::Batch) - base_b;
        assert!(d_batch > d_latency);
        assert!(d_batch > 1_000_000);
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
}
