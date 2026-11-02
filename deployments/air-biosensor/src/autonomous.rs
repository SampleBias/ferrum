//! Offline heuristic-v1 control loop for the simulated air-sensor deployment.
//!
//! Queue, wait, and pressure features come from the sensor phase simulator;
//! CPU service deltas and the active generation/profile are read from Hermit.
//! The guest proposes only catalog profiles. Hermit's joint-policy syscall
//! validates dwell, generation, acceptance time, and lease.

use std::process::ExitCode;

#[cfg(target_os = "hermit")]
use air_biosensor::AUTONOMOUS_SAMPLE_US;
use air_biosensor::{autonomous_v1, biosensor_observation};
use policy_core::{HeuristicSession, HEURISTIC_V1};
use policy_types::ProfileId;
#[cfg(target_os = "hermit")]
use policy_types::{CatalogId, MIN_DWELL_US};

pub fn exit() -> ExitCode {
    match run() {
        Ok(()) => {
            println!("FERRUM_AUTONOMOUS_OK");
            println!("FERRUM_COMPLETE");
            ExitCode::SUCCESS
        }
        Err(err) => {
            println!("FERRUM_AUTONOMOUS_FAIL {err}");
            ExitCode::from(1)
        }
    }
}

#[cfg(not(target_os = "hermit"))]
fn run() -> Result<(), String> {
    let mut session = HeuristicSession::new(HEURISTIC_V1);
    let mut current = ProfileId::Balanced;
    for phase in autonomous_v1() {
        let observation = biosensor_observation(*phase, [100_000, 100_000, 50_000]);
        let decision = session.decide(&observation, current);
        println!(
            "FERRUM_LOCAL_PLAN phase={} profile={} rule={} regime={} cpu_source=simulated kernel=unavailable",
            phase.name,
            decision.profile.as_str(),
            decision.rule.as_str(),
            decision.regime.as_str()
        );
        current = decision.profile;
    }
    println!("FERRUM_LOCAL_PLAN_ONLY profiles=5 target=hermit-required");
    Ok(())
}

#[cfg(target_os = "hermit")]
fn run() -> Result<(), String> {
    use std::hint::spin_loop;
    use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    use hermit_policy_abi::{AbiHeader, AbiJointStatus, AbiStageJointIn};
    use policy_types::{ACCEPTANCE_DEADLINE_US, MAX_WORKLOAD_THREADS, PROFILE_LEASE_US};

    const LATENCY: u8 = 1;
    const BATCH: u8 = 2;
    const MAINTENANCE: u8 = 3;
    const ACK_APPLIED: u8 = 1;
    const ACK_REJECTED: u8 = 4;

    #[repr(C)]
    struct PolicyAck {
        kind: u8,
        reason: u8,
        profile: u8,
        _pad0: u8,
        _pad1: u32,
        previous_generation: u64,
        generation: u64,
        guest_us: u64,
        lease_until_guest_us: u64,
    }

    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i32,
    }

    unsafe extern "C" {
        fn sys_policy_register(class: u8) -> i32;
        fn sys_policy_read(class: u8, service_us: *mut u64) -> i32;
        fn sys_policy_stage_joint(input: *const AbiStageJointIn) -> i32;
        fn sys_policy_read_joint(out: *mut AbiJointStatus) -> i32;
        fn sys_policy_read_ack(out: *mut PolicyAck) -> i32;
        fn sys_policy_staged() -> i32;
        fn sys_clock_gettime(clock_id: i32, tp: *mut Timespec) -> i32;
        fn sys_usleep(usecs: u64);
        fn sys_yield();
    }

    struct WorkerControl {
        phase: AtomicU8,
        ready: AtomicUsize,
        failed: AtomicBool,
        stop: AtomicBool,
    }

    fn now_us() -> Result<u64, String> {
        let mut time = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let rc = unsafe { sys_clock_gettime(4, &mut time) };
        if rc != 0 || time.tv_sec < 0 || !(0..1_000_000_000).contains(&time.tv_nsec) {
            return Err(format!("monotonic clock returned {rc}"));
        }
        Ok((time.tv_sec as u64)
            .saturating_mul(1_000_000)
            .saturating_add((time.tv_nsec / 1000) as u64))
    }

    fn sleep_until(target_us: u64) -> Result<(), String> {
        loop {
            let now = now_us()?;
            if now >= target_us {
                return Ok(());
            }
            let rest = target_us - now;
            unsafe { sys_usleep(rest.min(100_000)) };
        }
    }

    fn read_joint() -> Result<AbiJointStatus, String> {
        let mut status = AbiJointStatus {
            header: AbiHeader::new::<AbiJointStatus>(),
            profile_id: 0,
            override_active: 0,
            tickless_enabled: 0,
            _pad0: 0,
            inflight_latency: 0,
            inflight_batch: 0,
            weight_latency: 0,
            weight_batch: 0,
            weight_maintenance: 0,
            generation: 0,
            cache_soft_target_bytes: 0,
            lease_until_guest_us: 0,
            idle_residency_us: 0,
            timer_wakeups: 0,
            coalesced_wakeups: 0,
        };
        let rc = unsafe { sys_policy_read_joint(&mut status) };
        if rc == 0 {
            Ok(status)
        } else {
            Err(format!("sys_policy_read_joint returned {rc}"))
        }
    }

    fn profile_from_kernel(code: u8) -> Option<ProfileId> {
        match code {
            0 => Some(ProfileId::Balanced),
            1 => Some(ProfileId::Latency),
            2 => Some(ProfileId::Throughput),
            3 => Some(ProfileId::Reclaim),
            _ => None,
        }
    }

    fn read_service(class: u8) -> Result<u64, String> {
        let mut service_us = 0u64;
        let rc = unsafe { sys_policy_read(class, &mut service_us) };
        if rc == 0 {
            Ok(service_us)
        } else {
            Err(format!("sys_policy_read({class}) returned {rc}"))
        }
    }

    fn service_counters() -> Result<[u64; 3], String> {
        Ok([
            read_service(LATENCY)?,
            read_service(BATCH)?,
            read_service(MAINTENANCE)?,
        ])
    }

    fn drain_acks() {
        loop {
            let mut ack = PolicyAck {
                kind: 0,
                reason: 0,
                profile: 0,
                _pad0: 0,
                _pad1: 0,
                previous_generation: 0,
                generation: 0,
                guest_us: 0,
                lease_until_guest_us: 0,
            };
            if unsafe { sys_policy_read_ack(&mut ack) } != 0 {
                break;
            }
        }
    }

    fn stage_profile(
        profile: ProfileId,
        base_generation: u64,
        deadline_us: u64,
    ) -> Result<(bool, u64, u8), String> {
        let spec = policy_core::catalog_spec(CatalogId::JointV1).profile(profile);
        let input = AbiStageJointIn {
            header: AbiHeader::new::<AbiStageJointIn>(),
            profile_id: profile as u8,
            _pad0: 0,
            inflight_latency: spec.inflight_latency,
            inflight_batch: spec.inflight_batch,
            _pad1: 0,
            weight_latency: spec.weight_latency,
            weight_batch: spec.weight_batch,
            weight_maintenance: spec.weight_maintenance,
            cache_soft_target_bytes: spec.cache_soft_target_bytes,
            accept_us: ACCEPTANCE_DEADLINE_US,
            lease_us: PROFILE_LEASE_US,
            base_generation,
        };
        drain_acks();
        let started_us = now_us()?;
        let rc = unsafe { sys_policy_stage_joint(&input) };
        if rc != 0 {
            return Ok((false, 0, rc.unsigned_abs().min(255) as u8));
        }

        loop {
            unsafe { sys_yield() };
            let mut ack = PolicyAck {
                kind: 0,
                reason: 0,
                profile: 0,
                _pad0: 0,
                _pad1: 0,
                previous_generation: 0,
                generation: 0,
                guest_us: 0,
                lease_until_guest_us: 0,
            };
            if unsafe { sys_policy_read_ack(&mut ack) } == 0 {
                let elapsed = now_us()?.saturating_sub(started_us);
                let applied = ack.kind == ACK_APPLIED
                    && ack.generation == base_generation.saturating_add(1)
                    && ack.profile == profile as u8;
                let reason = if ack.kind == ACK_REJECTED {
                    ack.reason
                } else {
                    0
                };
                return Ok((applied, elapsed, reason));
            }

            let status = read_joint()?;
            if status.generation != base_generation {
                let elapsed = now_us()?.saturating_sub(started_us);
                let applied = status.generation == base_generation.saturating_add(1)
                    && status.profile_id == profile as u8;
                return Ok((applied, elapsed, 0));
            }
            let now = now_us()?;
            if now.saturating_sub(started_us) >= deadline_us {
                return Ok((false, now.saturating_sub(started_us), 0));
            }
            if unsafe { sys_policy_staged() } == 0 {
                return Ok((false, now.saturating_sub(started_us), 0));
            }
            unsafe { sys_usleep(2_000) };
        }
    }

    let cycle_limit = std::env::args()
        .find_map(|arg| arg.strip_prefix("--cycles=").map(str::to_string))
        .map(|value| {
            value
                .parse::<u32>()
                .map_err(|_| format!("invalid --cycles value: {value}"))
        })
        .transpose()?
        .unwrap_or(1);
    if std::env::args().any(|arg| arg.starts_with("--controller=")) {
        return Err("--autonomous cannot be combined with --controller".to_string());
    }

    let workload_threads = workloads::LATENCY_WORKERS
        .saturating_add(workloads::BATCH_WORKERS)
        .saturating_add(workloads::MAINTENANCE_WORKERS);
    if workload_threads > MAX_WORKLOAD_THREADS {
        return Err(format!(
            "sensor workload has too many workers: {workload_threads}"
        ));
    }
    let control = Arc::new(WorkerControl {
        phase: AtomicU8::new(0),
        ready: AtomicUsize::new(0),
        failed: AtomicBool::new(false),
        stop: AtomicBool::new(false),
    });
    let mut workers = Vec::new();
    for (class, count) in [
        (LATENCY, workloads::LATENCY_WORKERS),
        (BATCH, workloads::BATCH_WORKERS),
        (MAINTENANCE, workloads::MAINTENANCE_WORKERS),
    ] {
        for _ in 0..count {
            let state = Arc::clone(&control);
            workers.push(thread::spawn(move || {
                if unsafe { sys_policy_register(class) } != 0 {
                    state.failed.store(true, Ordering::Release);
                    return;
                }
                state.ready.fetch_add(1, Ordering::AcqRel);
                loop {
                    if state.stop.load(Ordering::Acquire) {
                        let _ = unsafe { sys_policy_register(0) };
                        return;
                    }
                    let phase = state.phase.load(Ordering::Acquire);
                    let active =
                        matches!((phase, class), (1, LATENCY) | (2, BATCH) | (4, MAINTENANCE));
                    if active {
                        for _ in 0..4_000 {
                            spin_loop();
                        }
                        unsafe { sys_usleep(1_000) };
                    } else {
                        unsafe { sys_usleep(10_000) };
                    }
                }
            }));
        }
    }

    for _ in 0..100 {
        if control.ready.load(Ordering::Acquire) == usize::from(workload_threads)
            || control.failed.load(Ordering::Acquire)
        {
            break;
        }
        unsafe { sys_usleep(10_000) };
    }
    if control.failed.load(Ordering::Acquire)
        || control.ready.load(Ordering::Acquire) != usize::from(workload_threads)
    {
        control.stop.store(true, Ordering::Release);
        for worker in workers {
            let _ = worker.join();
        }
        return Err(
            "sensor workers did not register; kernel remains on its safe profile".to_string(),
        );
    }

    let mut session = HeuristicSession::new(HEURISTIC_V1);
    let mut last_profile_change_us = 0u64;
    let mut pending_profile = None;
    let mut pending_streak = 0u8;
    let mut accepted = 0u64;
    let mut rejected = 0u64;
    let mut switches = 0u64;
    let mut event_start_us = 0u64;
    let mut capture_policy_activation_us = None;
    let mut cycle = 0u32;
    let mut failure = None;

    'cycles: loop {
        if cycle_limit != 0 && cycle >= cycle_limit {
            break;
        }
        cycle = cycle.saturating_add(1);
        for phase in autonomous_v1() {
            let phase_index = autonomous_v1()
                .iter()
                .position(|candidate| candidate.name == phase.name)
                .unwrap_or(0) as u8;
            control.phase.store(phase_index, Ordering::Release);
            let phase_start = match now_us() {
                Ok(now) => now,
                Err(err) => {
                    failure = Some(err);
                    break 'cycles;
                }
            };
            let mut sample_start = phase_start;
            let mut service_before = match service_counters() {
                Ok(counters) => counters,
                Err(err) => {
                    println!("FERRUM_TELEMETRY_MISSING phase={} reason={err}", phase.name);
                    [0, 0, 0]
                }
            };
            if phase.name == "capture" {
                event_start_us = phase_start;
                capture_policy_activation_us = None;
            }

            loop {
                let phase_now = match now_us() {
                    Ok(now) => now,
                    Err(err) => {
                        failure = Some(err);
                        break 'cycles;
                    }
                };
                if phase_now >= phase_start.saturating_add(phase.duration_us) {
                    break;
                }
                let target = sample_start.saturating_add(AUTONOMOUS_SAMPLE_US);
                if let Err(err) = sleep_until(target) {
                    failure = Some(err);
                    break 'cycles;
                }
                let sample_end = match now_us() {
                    Ok(now) => now,
                    Err(err) => {
                        failure = Some(err);
                        break 'cycles;
                    }
                };
                let service_after = match service_counters() {
                    Ok(counters) => counters,
                    Err(err) => {
                        println!("FERRUM_TELEMETRY_MISSING phase={} reason={err}", phase.name);
                        sample_start = sample_end;
                        continue;
                    }
                };
                let window_us = sample_end.saturating_sub(sample_start);
                if window_us == 0 || window_us > AUTONOMOUS_SAMPLE_US.saturating_mul(2) {
                    println!(
                        "FERRUM_TELEMETRY_STALE phase={} window_us={window_us} action=skip",
                        phase.name
                    );
                    sample_start = sample_end;
                    service_before = service_after;
                    continue;
                }
                let service = [
                    service_after[0].saturating_sub(service_before[0]),
                    service_after[1].saturating_sub(service_before[1]),
                    service_after[2].saturating_sub(service_before[2]),
                ];
                sample_start = sample_end;
                service_before = service_after;
                if service
                    .iter()
                    .any(|value| *value > window_us.saturating_mul(2))
                {
                    println!(
                        "FERRUM_TELEMETRY_INVALID phase={} window_us={window_us} service={}:{}:{} action=skip",
                        phase.name, service[0], service[1], service[2]
                    );
                    continue;
                }

                let mut observation = biosensor_observation(*phase, service);
                observation.window_us = window_us;
                let status = match read_joint() {
                    Ok(status) => status,
                    Err(err) => {
                        println!(
                            "FERRUM_POLICY_STATUS_UNAVAILABLE phase={} reason={err} action=skip",
                            phase.name
                        );
                        continue;
                    }
                };
                let Some(current) = profile_from_kernel(status.profile_id) else {
                    println!(
                        "FERRUM_POLICY_STATUS_INVALID profile_id={} action=skip",
                        status.profile_id
                    );
                    continue;
                };
                if status.override_active != 0 {
                    println!(
                        "FERRUM_POLICY_OVERRIDE phase={} profile={} action=kernel-owned",
                        phase.name,
                        current.as_str()
                    );
                    continue;
                }
                let decision = session.decide(&observation, current);
                let now = sample_end;
                if decision.profile == current {
                    pending_profile = None;
                    pending_streak = 0;
                } else if decision.profile == ProfileId::Reclaim {
                    // Reclaim is the catalog's pressure response. Do not delay
                    // it behind the stability filter.
                    pending_profile = None;
                    pending_streak = 0;
                } else if pending_profile == Some(decision.profile) {
                    pending_streak = pending_streak.saturating_add(1);
                } else {
                    pending_profile = Some(decision.profile);
                    pending_streak = 1;
                }
                if decision.profile != current
                    && decision.profile != ProfileId::Reclaim
                    && pending_streak < 2
                {
                    println!(
                        "FERRUM_LOCAL_DECISION phase={} choice={} rule={} regime={} action=stability-deferred streak={}",
                        phase.name,
                        decision.profile.as_str(),
                        decision.rule.as_str(),
                        decision.regime.as_str(),
                        pending_streak
                    );
                    continue;
                }
                if decision.profile != current
                    && now.saturating_sub(last_profile_change_us) < MIN_DWELL_US
                    && last_profile_change_us != 0
                {
                    println!(
                        "FERRUM_LOCAL_DECISION phase={} choice={} rule={} regime={} action=dwell-deferred generation={}",
                        phase.name,
                        decision.profile.as_str(),
                        decision.rule.as_str(),
                        decision.regime.as_str(),
                        status.generation
                    );
                    continue;
                }

                let (applied, activation_us, reason) = match stage_profile(
                    decision.profile,
                    status.generation,
                    ACCEPTANCE_DEADLINE_US,
                ) {
                    Ok(result) => result,
                    Err(err) => {
                        println!(
                            "FERRUM_POLICY_STAGE_ERROR phase={} reason={err} action=keep-current",
                            phase.name
                        );
                        continue;
                    }
                };
                if applied {
                    accepted = accepted.saturating_add(1);
                    if decision.profile != current {
                        switches = switches.saturating_add(1);
                        last_profile_change_us = now_us().unwrap_or(now);
                        pending_profile = None;
                        pending_streak = 0;
                    }
                    if phase.name == "capture"
                        && decision.profile == ProfileId::Latency
                        && capture_policy_activation_us.is_none()
                    {
                        capture_policy_activation_us =
                            Some(now_us().unwrap_or(now).saturating_sub(event_start_us));
                    }
                    println!(
                        "FERRUM_LOCAL_ACK phase={} profile={} generation={} activation_us={} lease_until={} rule={} regime={}",
                        phase.name,
                        decision.profile.as_str(),
                        read_joint().map(|s| s.generation).unwrap_or(status.generation),
                        activation_us,
                        read_joint().map(|s| s.lease_until_guest_us).unwrap_or(0),
                        decision.rule.as_str(),
                        decision.regime.as_str()
                    );
                } else {
                    rejected = rejected.saturating_add(1);
                    println!(
                        "FERRUM_LOCAL_REJECT phase={} proposed={} reason={} generation={} action=kernel-retains-current",
                        phase.name,
                        decision.profile.as_str(),
                        reason,
                        read_joint().map(|s| s.generation).unwrap_or(status.generation)
                    );
                }
            }
        }
    }

    control.stop.store(true, Ordering::Release);
    for worker in workers {
        let _ = worker.join();
    }
    if let Some(err) = failure {
        return Err(err);
    }
    let final_status = read_joint()?;
    println!(
        "FERRUM_AUTONOMOUS_SUMMARY cycles={} accepted={} rejected={} switches={} capture_policy_activation_us={} final_profile={} generation={} offline=true",
        cycle,
        accepted,
        rejected,
        switches,
        capture_policy_activation_us
            .map_or_else(|| "none".to_string(), |value| value.to_string()),
        profile_from_kernel(final_status.profile_id)
            .map_or("unknown", ProfileId::as_str),
        final_status.generation
    );
    Ok(())
}
