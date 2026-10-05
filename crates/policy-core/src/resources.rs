//! Managed-memory and admission ledgers. These are the deterministic actuators
//! the joint catalog will drive. They do not wrap the kernel allocator.

use policy_types::{
    BATCH_ARENA_CAP_BYTES, CACHE_CAP_BYTES, CLEAR_HEADROOM_BYTES, CLEAR_HOLD_US, CLEAR_POOL_BP,
    EMERGENCY_HEADROOM_BYTES, EMERGENCY_POOL_BP, LATENCY_ARENA_CAP_BYTES, MAX_JOB_CHARGE_BYTES,
};

pub const QUEUE_SLOTS: usize = 32;
pub const QUEUED_BYTE_CAP: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pool {
    Latency,
    Batch,
    Cache,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkClass {
    Latency,
    Batch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceError {
    TooLarge,
    Cap,
    Unbalanced,
    Overflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Submit {
    Admitted,
    Queued,
    Deferred,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InflightLimits {
    pub latency: u16,
    pub batch: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArenaLedger {
    pub latency_used: u64,
    pub batch_used: u64,
    pub cache_used: u64,
    pub cache_soft_target: u64,
}

impl ArenaLedger {
    pub const fn new(cache_soft_target: u64) -> Self {
        Self {
            latency_used: 0,
            batch_used: 0,
            cache_used: 0,
            cache_soft_target,
        }
    }

    pub fn try_alloc(&mut self, pool: Pool, bytes: u64) -> Result<(), ResourceError> {
        if bytes == 0 {
            return Err(ResourceError::TooLarge);
        }
        let (used, cap) = self.slot(pool);
        let next = used.checked_add(bytes).ok_or(ResourceError::Overflow)?;
        if next > cap {
            return Err(ResourceError::Cap);
        }
        *self.used_mut(pool) = next;
        Ok(())
    }

    pub fn free(&mut self, pool: Pool, bytes: u64) -> Result<(), ResourceError> {
        let used = self.used_mut(pool);
        if *used < bytes {
            return Err(ResourceError::Unbalanced);
        }
        *used -= bytes;
        Ok(())
    }

    pub fn set_cache_target(&mut self, bytes: u64) {
        self.cache_soft_target = bytes.min(CACHE_CAP_BYTES);
    }

    pub fn eviction_backlog(&self) -> u64 {
        self.cache_used.saturating_sub(self.cache_soft_target)
    }

    /// Remove at most `max_batch` bytes from the evictable cache. Latency and
    /// batch arenas are never freed here.
    pub fn evict_cache(&mut self, max_batch: u64) -> u64 {
        let bytes = self.eviction_backlog().min(max_batch);
        self.cache_used -= bytes;
        bytes
    }

    fn slot(&self, pool: Pool) -> (u64, u64) {
        match pool {
            Pool::Latency => (self.latency_used, LATENCY_ARENA_CAP_BYTES),
            Pool::Batch => (self.batch_used, BATCH_ARENA_CAP_BYTES),
            Pool::Cache => (self.cache_used, CACHE_CAP_BYTES),
        }
    }

    fn used_mut(&mut self, pool: Pool) -> &mut u64 {
        match pool {
            Pool::Latency => &mut self.latency_used,
            Pool::Batch => &mut self.batch_used,
            Pool::Cache => &mut self.cache_used,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct JobQueue {
    jobs: [u64; QUEUE_SLOTS],
    head: usize,
    len: usize,
    bytes: u64,
}

impl JobQueue {
    const fn new() -> Self {
        Self {
            jobs: [0; QUEUE_SLOTS],
            head: 0,
            len: 0,
            bytes: 0,
        }
    }

    fn push(&mut self, bytes: u64) -> bool {
        if self.len == QUEUE_SLOTS {
            return false;
        }
        let tail = (self.head + self.len) % QUEUE_SLOTS;
        self.jobs[tail] = bytes;
        self.len += 1;
        self.bytes += bytes;
        true
    }

    fn pop(&mut self) -> Option<u64> {
        if self.len == 0 {
            return None;
        }
        let bytes = self.jobs[self.head];
        self.head = (self.head + 1) % QUEUE_SLOTS;
        self.len -= 1;
        self.bytes -= bytes;
        Some(bytes)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct AdmissionLedger {
    limits: [u16; 2],
    inflight: [u16; 2],
    queued: [JobQueue; 2],
}

impl AdmissionLedger {
    pub const fn new(limits: InflightLimits) -> Self {
        Self {
            limits: [limits.latency, limits.batch],
            inflight: [0, 0],
            queued: [JobQueue::new(), JobQueue::new()],
        }
    }

    pub fn set_limits(&mut self, limits: InflightLimits) {
        self.limits = [limits.latency, limits.batch];
        self.pump(WorkClass::Latency);
        self.pump(WorkClass::Batch);
    }

    pub fn inflight(&self, class: WorkClass) -> u16 {
        self.inflight[class.index()]
    }

    pub fn queued(&self, class: WorkClass) -> usize {
        self.queued[class.index()].len
    }

    pub fn submit(&mut self, class: WorkClass, bytes: u64) -> Result<Submit, ResourceError> {
        if bytes == 0 || bytes > MAX_JOB_CHARGE_BYTES {
            return Err(ResourceError::TooLarge);
        }
        let index = class.index();
        if self.queued[index].len == 0 && self.inflight[index] < self.limits[index] {
            self.inflight[index] += 1;
            return Ok(Submit::Admitted);
        }
        if self.queued[index].bytes.saturating_add(bytes) > QUEUED_BYTE_CAP {
            return Ok(Submit::Deferred);
        }
        if !self.queued[index].push(bytes) {
            return Ok(Submit::Deferred);
        }
        Ok(Submit::Queued)
    }

    pub fn complete(&mut self, class: WorkClass) -> Result<(), ResourceError> {
        let index = class.index();
        if self.inflight[index] == 0 {
            return Err(ResourceError::Unbalanced);
        }
        self.inflight[index] -= 1;
        self.pump(class);
        Ok(())
    }

    fn pump(&mut self, class: WorkClass) {
        let index = class.index();
        while self.inflight[index] < self.limits[index] {
            if self.queued[index].pop().is_none() {
                break;
            }
            self.inflight[index] += 1;
        }
    }
}

impl WorkClass {
    const fn index(self) -> usize {
        match self {
            Self::Latency => 0,
            Self::Batch => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PressureMonitor {
    clear_since: Option<u64>,
}

impl PressureMonitor {
    pub fn emergency_triggered(arenas: &ArenaLedger, headroom_bytes: u64) -> bool {
        ratio_at_least(arenas.latency_used, LATENCY_ARENA_CAP_BYTES, EMERGENCY_POOL_BP)
            || ratio_at_least(arenas.batch_used, BATCH_ARENA_CAP_BYTES, EMERGENCY_POOL_BP)
            || ratio_at_least(arenas.cache_used, CACHE_CAP_BYTES, EMERGENCY_POOL_BP)
            || headroom_bytes < EMERGENCY_HEADROOM_BYTES
    }

    pub fn clear_condition(arenas: &ArenaLedger, headroom_bytes: u64) -> bool {
        ratio_below(arenas.latency_used, LATENCY_ARENA_CAP_BYTES, CLEAR_POOL_BP)
            && ratio_below(arenas.batch_used, BATCH_ARENA_CAP_BYTES, CLEAR_POOL_BP)
            && ratio_below(arenas.cache_used, CACHE_CAP_BYTES, CLEAR_POOL_BP)
            && headroom_bytes > CLEAR_HEADROOM_BYTES
    }

    /// Returns true only after the clear condition has held continuously.
    pub fn observe(&mut self, now_us: u64, clear: bool) -> bool {
        if !clear {
            self.clear_since = None;
            return false;
        }
        match self.clear_since {
            None => {
                self.clear_since = Some(now_us);
                false
            }
            Some(start) => now_us.saturating_sub(start) >= CLEAR_HOLD_US,
        }
    }
}

fn ratio_at_least(used: u64, cap: u64, basis_points: u16) -> bool {
    used.saturating_mul(10_000) / cap >= u64::from(basis_points)
}

fn ratio_below(used: u64, cap: u64, basis_points: u16) -> bool {
    used.saturating_mul(10_000) / cap < u64::from(basis_points)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hard_caps_and_cache_eviction_do_not_free_live_buffers() {
        let mut arenas = ArenaLedger::new(64 * 1024 * 1024);
        arenas.try_alloc(Pool::Latency, 1024).unwrap();
        assert!(arenas.try_alloc(Pool::Latency, LATENCY_ARENA_CAP_BYTES).is_err());
        arenas.free(Pool::Latency, 1024).unwrap();
        assert!(arenas.free(Pool::Latency, 1).is_err());

        arenas.try_alloc(Pool::Cache, 96 * 1024 * 1024).unwrap();
        arenas.set_cache_target(32 * 1024 * 1024);
        let freed = arenas.evict_cache(8 * 1024 * 1024);
        assert_eq!(freed, 8 * 1024 * 1024);
        assert_eq!(arenas.cache_used, 88 * 1024 * 1024);
        arenas.try_alloc(Pool::Latency, 4096).unwrap();
        let before = arenas.latency_used;
        let _ = arenas.evict_cache(1024);
        assert_eq!(arenas.latency_used, before);
    }

    #[test]
    fn lowering_admission_drains_without_dropping_inflight() {
        let mut admission = AdmissionLedger::new(InflightLimits {
            latency: 4,
            batch: 2,
        });
        for _ in 0..4 {
            assert_eq!(admission.submit(WorkClass::Latency, 128).unwrap(), Submit::Admitted);
        }
        assert_eq!(admission.submit(WorkClass::Latency, 128).unwrap(), Submit::Queued);
        admission.set_limits(InflightLimits {
            latency: 1,
            batch: 2,
        });
        assert_eq!(admission.inflight(WorkClass::Latency), 4);
        assert_eq!(admission.queued(WorkClass::Latency), 1);
        admission.complete(WorkClass::Latency).unwrap();
        assert_eq!(admission.inflight(WorkClass::Latency), 3);
        assert_eq!(admission.queued(WorkClass::Latency), 1);
        admission.complete(WorkClass::Latency).unwrap();
        admission.complete(WorkClass::Latency).unwrap();
        assert_eq!(admission.inflight(WorkClass::Latency), 1);
        assert_eq!(admission.queued(WorkClass::Latency), 1);
        admission.complete(WorkClass::Latency).unwrap();
        assert_eq!(admission.inflight(WorkClass::Latency), 1);
        assert_eq!(admission.queued(WorkClass::Latency), 0);
    }

    #[test]
    fn emergency_hysteresis_requires_a_continuous_clear_window() {
        let mut hot = ArenaLedger::new(64 * 1024 * 1024);
        hot.cache_used = 120_795_956;
        assert!(PressureMonitor::emergency_triggered(&hot, 128 * 1024 * 1024));
        let cool = ArenaLedger::new(64 * 1024 * 1024);
        assert!(!PressureMonitor::clear_condition(&cool, 96 * 1024 * 1024));
        assert!(PressureMonitor::clear_condition(&cool, 96 * 1024 * 1024 + 1));

        let mut monitor = PressureMonitor::default();
        assert!(!monitor.observe(0, true));
        assert!(!monitor.observe(CLEAR_HOLD_US - 1, true));
        assert!(monitor.observe(CLEAR_HOLD_US, true));
        let mut reset = PressureMonitor::default();
        assert!(!reset.observe(0, true));
        assert!(!reset.observe(1_000, false));
        assert!(!reset.observe(1_000 + CLEAR_HOLD_US, true));
    }
}
