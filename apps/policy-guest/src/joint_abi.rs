//! `--joint-abi`: Generic S4 Joint Kernel ABI & Tickless Residency Verification.
//!
//! Exercises the base kernel's narrow application ABI (`hermit_policy_abi::AbiStageJointIn`
//! and `AbiJointStatus`), tickless idle/quantum coalescing (`sys_policy_set_tickless`),
//! and live emergency hysteresis (`sys_policy_install_emergency` / `sys_policy_clear_emergency`)
//! across the generic `mixed-v1` workload phases without any domain-specific assumptions.

#[cfg(target_os = "hermit")]
use std::env;
use std::process::ExitCode;

pub fn exit() -> ExitCode {
    match run() {
        Ok(()) => {
            println!("FERRUM_JOINT_ABI_OK");
            println!("FERRUM_COMPLETE");
            ExitCode::SUCCESS
        }
        Err(err) => {
            println!("FERRUM_JOINT_ABI_FAIL {err}");
            ExitCode::from(1)
        }
    }
}

/// Run the in-guest mixed workload with the local heuristic and refuse an
/// external endpoint so this mode cannot silently become a connected run.
pub fn offline_exit() -> ExitCode {
    #[cfg(target_os = "hermit")]
    if env::args().any(|arg| arg.starts_with("--controller=")) {
        println!("FERRUM_AUTONOMOUS_FAIL --autonomous cannot be combined with --controller");
        return ExitCode::from(1);
    }
    exit()
}

#[cfg(not(target_os = "hermit"))]
fn run() -> Result<(), String> {
    Err("joint-abi runs inside the hermit guest".to_string())
}

#[cfg(target_os = "hermit")]
fn run() -> Result<(), String> {
    use std::net::TcpStream;

    use hermit_policy_abi::{AbiHeader, AbiJointStatus, AbiStageJointIn};
    use policy_core::entries::EntryTable;
    use policy_core::resources::{
        AdmissionLedger, ArenaLedger, InflightLimits, Pool, PressureMonitor, SmallDeviceEnvelope,
        Submit, WorkClass,
    };
    use policy_core::{
        catalog_spec, choose_heuristic, evaluate_distilled, HeuristicSession,
        DISTILLED_JOINT_V1, HEURISTIC_V0, HEURISTIC_V1,
    };
    use policy_guest::LiveSession;
    use policy_types::{
        BootId, CatalogId, ProfileId, RunMode, ACCEPTANCE_DEADLINE_US, CLEAR_HEADROOM_BYTES,
        CLEAR_HOLD_US, MIN_DWELL_US, PROFILE_LEASE_US,
    };

    const LATENCY: u8 = 1;
    const MIB: u64 = 1024 * 1024;

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
        fn sys_policy_set_tickless(mode: u8) -> i32;
        fn sys_policy_install_emergency() -> i32;
        fn sys_policy_clear_emergency() -> i32;
        fn sys_policy_stage_joint(input: *const AbiStageJointIn) -> i32;
        fn sys_policy_read_joint(out: *mut AbiJointStatus) -> i32;
        fn sys_policy_read_ack(ack: *mut PolicyAck) -> i32;
        fn sys_clock_gettime(clock_id: i32, tp: *mut Timespec) -> i32;
        fn sys_usleep(usecs: u64);
        fn sys_yield();
    }

    fn now_us() -> u64 {
        let mut time = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { sys_clock_gettime(4, &mut time) } != 0 {
            return 0;
        }
        (time.tv_sec as u64)
            .saturating_mul(1_000_000)
            .saturating_add((time.tv_nsec / 1000) as u64)
    }

    fn read_joint() -> Result<AbiJointStatus, String> {
        let mut out = AbiJointStatus {
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
        let rc = unsafe { sys_policy_read_joint(&mut out) };
        if rc == 0 {
            Ok(out)
        } else {
            Err(format!("sys_policy_read_joint returned {rc}"))
        }
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

    fn stage_joint_profile(profile: ProfileId, base_generation: u64) -> Result<AbiJointStatus, String> {
        let spec = catalog_spec(CatalogId::JointV1).profile(profile);
        let cmd = AbiStageJointIn {
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
        let rc = unsafe { sys_policy_stage_joint(&cmd) };
        if rc != 0 {
            return Err(format!("sys_policy_stage_joint returned {rc}"));
        }
        unsafe { sys_yield() };
        let status = read_joint()?;
        if status.generation != base_generation + 1
            || status.cache_soft_target_bytes != spec.cache_soft_target_bytes
            || status.inflight_latency != spec.inflight_latency
            || status.inflight_batch != spec.inflight_batch
        {
            return Err(format!(
                "joint activation mismatch: gen={} cache={} inflight={}/{}",
                status.generation,
                status.cache_soft_target_bytes,
                status.inflight_latency,
                status.inflight_batch
            ));
        }
        drain_acks();
        Ok(status)
    }

    let controller_addr = env::args()
        .skip(1)
        .find_map(|arg| arg.strip_prefix("--controller=").map(str::to_string));

    // 1. Verify tickless idle coalescing across a 200 ms low-duty window
    if unsafe { sys_policy_set_tickless(3) } != 0 {
        return Err("sys_policy_set_tickless(3) failed".to_string());
    }
    let emb_env = SmallDeviceEnvelope::EMBEDDED_64M;
    let before_idle = read_joint()?;
    if unsafe { sys_policy_register(LATENCY) } != 0 {
        return Err("register failed".to_string());
    }
    for _ in 0..5 {
        let t0 = now_us();
        while now_us().saturating_sub(t0) < 500 {
            std::hint::spin_loop();
        }
        unsafe { sys_usleep(39_500) };
    }
    let _ = unsafe { sys_policy_register(0) };
    let after_idle = read_joint()?;
    let idle_us = after_idle
        .idle_residency_us
        .saturating_sub(before_idle.idle_residency_us);
    let wakeups = after_idle
        .timer_wakeups
        .saturating_sub(before_idle.timer_wakeups);
    let coalesced = after_idle
        .coalesced_wakeups
        .saturating_sub(before_idle.coalesced_wakeups);
    if idle_us < 150_000 || coalesced < 50 {
        return Err(format!(
            "tickless coalescing failed: idle_us={idle_us} wakeups={wakeups} coalesced={coalesced}"
        ));
    }
    println!(
        "FERRUM_TICKLESS window_us=200000 idle_residency_us={idle_us} timer_wakeups={wakeups} coalesced_wakeups={coalesced} embedded_ram_mib={}",
        emb_env.total_guest_bytes / MIB
    );

    // 2. Verify S4 JointV1 ABI across all 5 mixed-v1 phases
    let joint = catalog_spec(CatalogId::JointV1);
    let balanced = joint.profile(ProfileId::Balanced);
    let mut arenas = ArenaLedger::new(balanced.cache_soft_target_bytes);
    let mut entries = EntryTable::empty();
    let mut admission = AdmissionLedger::new(InflightLimits {
        latency: balanced.inflight_latency,
        batch: balanced.inflight_batch,
    });
    let mut monitor = PressureMonitor::default();

    let live_lat = entries
        .insert(&mut arenas, Pool::Latency, 4 * MIB, false)
        .map_err(|e| format!("latency insert: {e:?}"))?;
    let mut cache_entries = Vec::with_capacity(10);
    for idx in 0..10 {
        let id = entries
            .insert(&mut arenas, Pool::Cache, 8 * MIB, true)
            .map_err(|e| format!("cache insert: {e:?}"))?;
        if idx < 9 {
            entries
                .set_live(id, false)
                .map_err(|e| format!("set_live: {e:?}"))?;
        }
        cache_entries.push(id);
    }

    let mut connected_session = if let Some(ref addr) = controller_addr {
        let mut stream =
            TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
        let boot = BootId::from_hex("00112233445566778899aabbccddeeff").unwrap();
        let session = LiveSession::open_joint(&mut stream, boot, RunMode::Live)
            .map_err(|e| format!("open_joint: {e}"))?;
        Some((stream, session))
    } else {
        None
    };

    let mut reclaimed_bytes = 0u64;
    let mut current_gen = read_joint()?.generation;
    let mut last_non_renewal_us: Option<u64> = None;
    let mut local_heuristic = HeuristicSession::new(HEURISTIC_V1);

    for (step, phase) in workloads::mixed_v1().iter().enumerate() {
        let obs = workloads::measured(*phase, workloads::SAMPLE_US, [120_000, 120_000, 40_000]);
        let distilled = evaluate_distilled(&obs, &DISTILLED_JOINT_V1);
        let heuristic_choice = choose_heuristic(&obs, &HEURISTIC_V0);
        if distilled.profile != heuristic_choice {
            return Err(format!(
                "phase {} distilled {} != heuristic {}",
                phase.name,
                distilled.profile.as_str(),
                heuristic_choice.as_str()
            ));
        }
        let active_before = read_joint()?;
        let current_profile = match active_before.profile_id {
            0 => ProfileId::Balanced,
            1 => ProfileId::Latency,
            2 => ProfileId::Throughput,
            3 => ProfileId::Reclaim,
            other => return Err(format!("kernel returned unknown profile id {other}")),
        };
        let local_decision = local_heuristic.decide(&obs, current_profile);

        if phase.emergency {
            if unsafe { sys_policy_install_emergency() } != 0 {
                return Err("sys_policy_install_emergency failed".to_string());
            }
            drain_acks();
            let j_status = read_joint()?;
            arenas.set_cache_target(j_status.cache_soft_target_bytes);
            admission.set_limits(InflightLimits {
                latency: j_status.inflight_latency,
                batch: j_status.inflight_batch,
            });
            while arenas.eviction_backlog() > 0 {
                let freed = entries
                    .evict(&mut arenas, 16 * MIB)
                    .map_err(|e| format!("evict: {e:?}"))?;
                if freed == 0 {
                    break;
                }
                reclaimed_bytes += freed;
            }
            println!(
                "FERRUM_JOINT_ABI phase={} profile={} gen={} weights={}:{}:{} cache_target_mib={} cache_used_mib={} inflight={}/{} override={} reclaimed_mib={}",
                phase.name,
                distilled.profile.as_str(),
                j_status.generation,
                j_status.weight_latency,
                j_status.weight_batch,
                j_status.weight_maintenance,
                j_status.cache_soft_target_bytes / MIB,
                arenas.cache_used / MIB,
                j_status.inflight_latency,
                j_status.inflight_batch,
                j_status.override_active,
                reclaimed_bytes / MIB
            );
            let healthy_headroom = CLEAR_HEADROOM_BYTES + 16 * MIB;
            if !PressureMonitor::clear_condition(&arenas, healthy_headroom) {
                return Err("arenas did not reach clear condition after reclamation".to_string());
            }
            let t0 = (step as u64 + 1) * 10_000_000;
            let _ = monitor.observe(t0, true);
            if !monitor.observe(t0 + CLEAR_HOLD_US, true) {
                return Err("hysteresis monitor did not clear after 3s hold".to_string());
            }
            if unsafe { sys_policy_clear_emergency() } != 0 {
                return Err("sys_policy_clear_emergency failed".to_string());
            }
            drain_acks();
            current_gen = read_joint()?.generation;
            last_non_renewal_us = Some(now_us());
        } else {
            if active_before.profile_id != local_decision.profile as u8 {
                if let Some(changed_at) = last_non_renewal_us {
                    let elapsed = now_us().saturating_sub(changed_at);
                    if elapsed < MIN_DWELL_US + 25_000 {
                        unsafe { sys_usleep((MIN_DWELL_US + 25_000) - elapsed) };
                    }
                }
            }
            let chosen_profile = if let Some((ref mut stream, ref mut session)) = connected_session
            {
                session.report_cache_used(arenas.cache_used);
                let now_wire = now_us().max((step as u64 + 1) * 3_000_000);
                let ticket = session
                    .request(stream, now_wire, obs)
                    .map_err(|e| format!("joint wire request {}: {e}", phase.name))?;
                let j_status = stage_joint_profile(ticket.profile, current_gen)?;
                current_gen = j_status.generation;
                let ack = session
                    .commit_applied(now_wire)
                    .map_err(|e| format!("commit_applied: {e}"))?;
                policy_guest::report_applied(
                    stream,
                    &ticket,
                    ack.previous_generation,
                    ack.generation,
                    now_wire,
                    now_wire + PROFILE_LEASE_US,
                )
                .map_err(|e| format!("report_applied: {e}"))?;
                ticket.profile
            } else {
                let j_status = stage_joint_profile(local_decision.profile, current_gen)?;
                current_gen = j_status.generation;
                let _ = j_status;
                local_decision.profile
            };
            if active_before.profile_id != chosen_profile as u8 {
                last_non_renewal_us = Some(now_us());
            }
            let j_status = read_joint()?;
            arenas.set_cache_target(j_status.cache_soft_target_bytes);
            admission.set_limits(InflightLimits {
                latency: j_status.inflight_latency,
                batch: j_status.inflight_batch,
            });
            println!(
                "FERRUM_JOINT_ABI phase={} profile={} regime={} rule={} gen={} weights={}:{}:{} cache_target_mib={} inflight={}/{} override={}",
                phase.name,
                chosen_profile.as_str(),
                local_decision.regime.as_str(),
                local_decision.rule.as_str(),
                j_status.generation,
                j_status.weight_latency,
                j_status.weight_batch,
                j_status.weight_maintenance,
                j_status.cache_soft_target_bytes / MIB,
                j_status.inflight_latency,
                j_status.inflight_batch,
                j_status.override_active
            );
        }
    }

    for _ in 0..4 {
        if admission
            .submit(WorkClass::Latency, 64 * 1024)
            .map_err(|e| format!("submit: {e:?}"))?
            != Submit::Admitted
        {
            return Err("joint admission rejected in-limit job".to_string());
        }
        admission
            .complete(WorkClass::Latency)
            .map_err(|e| format!("complete: {e:?}"))?;
    }
    entries
        .release(&mut arenas, live_lat)
        .map_err(|e| format!("release latency: {e:?}"))?;
    if let Some(&last_cache) = cache_entries.last() {
        let _ = entries.release(&mut arenas, last_cache);
    }

    Ok(())
}
