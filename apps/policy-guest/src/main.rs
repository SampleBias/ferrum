//! Hermit guest that asks the host mock controller for one CPU profile.

#[cfg(target_os = "hermit")]
use hermit as _;

use std::env;
use std::net::TcpStream;
use std::process::ExitCode;

#[cfg(not(target_os = "hermit"))]
use policy_guest::exchange;
use policy_types::BootId;

fn main() -> ExitCode {
    println!("FERRUM_START policy-guest");
    let emergency = env::args().skip(1).any(|arg| arg == "--emergency");
    let stage_race = env::args().skip(1).any(|arg| arg == "--stage-race");
    let stale = env::args().skip(1).any(|arg| arg == "--stale");
    let late = env::args().skip(1).any(|arg| arg == "--late");
    if env::args().skip(1).any(|arg| arg == "--lose-ack") {
        return lose_ack_exit();
    }
    if env::args().skip(1).any(|arg| arg == "--fair-progress") {
        return fair_progress_exit();
    }
    if env::args().skip(1).any(|arg| arg == "--fair-demo") {
        return fair_demo_exit();
    }
    if env::args().skip(1).any(|arg| arg == "--fair-credit") {
        return fair_credit_exit();
    }
    if env::args().skip(1).any(|arg| arg == "--fair-queue") {
        return fair_queue_exit();
    }
    if env::args().skip(1).any(|arg| arg == "--reject-frames") {
        return reject_frames_exit();
    }
    if env::args().skip(1).any(|arg| arg == "--fair-dwell") {
        return fair_dwell_exit();
    }
    match run(emergency, stage_race, stale, late) {
        Ok(()) => {
            println!("FERRUM_COMPLETE");
            ExitCode::SUCCESS
        }
        Err(err) => {
            println!("FERRUM_FAIL {err}");
            ExitCode::from(1)
        }
    }
}

fn guest_endpoint() -> Result<(String, BootId), String> {
    let mut controller = default_controller();
    let mut boot = BootId::from_hex("00112233445566778899aabbccddeeff").expect("boot id");
    for arg in env::args().skip(1) {
        if let Some(value) = arg.strip_prefix("--controller=") {
            controller = value.to_string();
        } else if let Some(value) = arg.strip_prefix("--boot-id=") {
            boot = BootId::from_hex(value).map_err(|_| "bad boot id".to_string())?;
        } else if arg == "--emergency" || arg == "--stage-race" || arg == "--stale" || arg == "--late" {
        } else {
            return Err(format!("unknown argument {arg}"));
        }
    }
    Ok((controller, boot))
}

/// Host build. There is no kernel scheduler, so the userspace engine's
/// activation is the report. The Hermit build waits for the scheduler.
#[cfg(not(target_os = "hermit"))]
fn run(emergency: bool, stage_race: bool, stale: bool, late: bool) -> Result<(), String> {
    let (controller, boot) = guest_endpoint()?;
    let mut stream = TcpStream::connect(&controller).map_err(|err| format!("connect {controller}: {err}"))?;
    let applied = exchange(&mut stream, boot, 3_000_000).map_err(|err| err.to_string())?;
    std::mem::forget(stream);
    println!(
        "FERRUM_APPLIED profile={} generation={}",
        applied.profile, applied.generation
    );
    actuate(&applied.profile, emergency, stage_race, stale, late)
}

/// Hermit build. The controller stays connected until the scheduler writes
/// the acknowledgment for this proposal. Closing the socket hangs in Hermit
/// after the peer has exited, so the stream is leaked for the rest of the run.
///
/// A missing controller is not a guest failure. The bridge gives the attempt
/// two seconds, then the boot profile, which is balanced, runs the workload.
#[cfg(target_os = "hermit")]
fn run(emergency: bool, stage_race: bool, stale: bool, late: bool) -> Result<(), String> {
    let (controller, boot) = guest_endpoint()?;
    let mut stream = match connect_controller(&controller) {
        Ok(stream) => stream,
        Err(err) => {
            println!("FERRUM_NO_CONTROLLER {err}");
            return fallback_balanced();
        }
    };
    let ticket = policy_guest::open_proposal(&mut stream, boot, 3_000_000).map_err(|err| err.to_string())?;
    let result = actuate(&mut stream, &ticket, emergency, stage_race, stale, late);
    std::mem::forget(stream);
    result
}

/// Try the controller without letting a silent peer hold the boot path.
/// The attempt runs on another thread because Hermit's connect waits inside
/// the network executor until the handshake finishes or the peer refuses it.
#[cfg(target_os = "hermit")]
fn connect_controller(addr: &str) -> Result<TcpStream, String> {
    use std::sync::mpsc;
    use std::thread;

    let addr = addr.to_string();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(TcpStream::connect(addr));
    });

    unsafe extern "C" {
        fn sys_usleep(usecs: u64);
    }

    let mut waited_us = 0u64;
    loop {
        match rx.try_recv() {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(err)) => return Err(err.to_string()),
            Err(mpsc::TryRecvError::Empty) => {
                if waited_us >= 2_000_000 {
                    return Err("timed out".to_string());
                }
                unsafe { sys_usleep(50_000) };
                waited_us = waited_us.saturating_add(50_000);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err("connect thread exited".to_string());
            }
        }
    }
}

/// Boot weights are balanced. Nothing is staged, so generation stays at 1.
#[cfg(target_os = "hermit")]
fn fallback_balanced() -> Result<(), String> {
    use std::hint::spin_loop;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::thread;

    const LATENCY: u8 = 1;
    const BATCH: u8 = 2;
    const MAINTENANCE: u8 = 3;

    unsafe extern "C" {
        fn sys_policy_register(class: u8) -> i32;
        fn sys_policy_read(class: u8, service_us: *mut u64) -> i32;
        fn sys_policy_generation(generation: *mut u64) -> i32;
        fn sys_policy_read_ack(ack: *mut PolicyAck) -> i32;
        fn sys_policy_staged() -> i32;
        fn sys_usleep(usecs: u64);
    }

    fn generation() -> Result<u64, String> {
        let mut generation = 0u64;
        let rc = unsafe { sys_policy_generation(&mut generation) };
        if rc == 0 {
            Ok(generation)
        } else {
            Err(format!("generation returned {rc}"))
        }
    }

    fn read_service(class: u8) -> Result<u64, String> {
        let mut service_us = 0u64;
        let rc = unsafe { sys_policy_read(class, &mut service_us) };
        if rc == 0 {
            Ok(service_us)
        } else {
            Err(format!("read class {class} returned {rc}"))
        }
    }

    static READY: AtomicU32 = AtomicU32::new(0);
    static FAILED: AtomicBool = AtomicBool::new(false);

    for class in [LATENCY, BATCH, MAINTENANCE] {
        thread::spawn(move || {
            if unsafe { sys_policy_register(class) } == 0 {
                READY.fetch_add(1, Ordering::Release);
            } else {
                FAILED.store(true, Ordering::Release);
            }
            loop {
                spin_loop();
            }
        });
    }

    for _ in 0..40 {
        if READY.load(Ordering::Acquire) == 3 || FAILED.load(Ordering::Acquire) {
            break;
        }
        unsafe { sys_usleep(50_000) };
    }
    if FAILED.load(Ordering::Acquire) {
        return Err("a worker could not register".to_string());
    }
    if READY.load(Ordering::Acquire) != 3 {
        return Err("workers did not register".to_string());
    }

    let generation_at_boot = generation()?;
    if generation_at_boot != 1 {
        return Err(format!("fallback generation was {generation_at_boot}"));
    }
    if unsafe { sys_policy_staged() } != 0 {
        return Err("fallback left a profile staged".to_string());
    }
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
    let ack_rc = unsafe { sys_policy_read_ack(&mut ack) };
    if ack_rc != 1 {
        return Err(format!("fallback found a scheduler acknowledgment ({ack_rc})"));
    }
    println!("FERRUM_FALLBACK profile=balanced generation={generation_at_boot}");

    let before = (
        read_service(LATENCY)?,
        read_service(BATCH)?,
        read_service(MAINTENANCE)?,
    );
    unsafe { sys_usleep(1_000_000) };
    let after = (
        read_service(LATENCY)?,
        read_service(BATCH)?,
        read_service(MAINTENANCE)?,
    );
    let sample = (
        after.0.saturating_sub(before.0),
        after.1.saturating_sub(before.1),
        after.2.saturating_sub(before.2),
    );
    println!(
        "FERRUM_SCHED window=balanced latency={} batch={} maintenance={}",
        sample.0, sample.1, sample.2
    );
    if generation()? != generation_at_boot {
        return Err("fallback workload advanced the scheduler generation".to_string());
    }
    let sum = sample.0 + sample.1 + sample.2;
    let near_even = sum > 800_000
        && sample.0 > 0
        && sample.1 > 0
        && sample.2 > 0
        && sample.0 * 20 < sum * 9
        && sample.1 * 20 < sum * 9
        && sample.2 * 20 < sum * 9;
    if !near_even {
        return Err("absent controller did not keep balanced service".to_string());
    }
    println!("FERRUM_FALLBACK_OK");
    Ok(())
}

fn lose_ack_exit() -> ExitCode {
    match lose_ack() {
        Ok(()) => {
            println!("FERRUM_COMPLETE");
            ExitCode::SUCCESS
        }
        Err(err) => {
            println!("FERRUM_FAIL {err}");
            ExitCode::from(1)
        }
    }
}

#[cfg(not(target_os = "hermit"))]
fn lose_ack() -> Result<(), String> {
    Err("lost-ack recovery runs inside the hermit guest".to_string())
}

/// Apply one profile, drop the connection before `applied` is sent, then
/// reconnect. The new snapshot carries the kernel generation. Replaying the
/// old proposal must not stage it again.
#[cfg(target_os = "hermit")]
fn lose_ack() -> Result<(), String> {
    use std::hint::spin_loop;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::thread;

    use policy_core::catalog_spec;
    use policy_types::{CatalogId, ProfileId, ACCEPTANCE_DEADLINE_US, PROFILE_LEASE_US};

    const LATENCY: u8 = 1;
    const BATCH: u8 = 2;
    const MAINTENANCE: u8 = 3;

    unsafe extern "C" {
        fn sys_policy_register(class: u8) -> i32;
        fn sys_policy_stage(
            latency: u32,
            batch: u32,
            maintenance: u32,
            accept_us: u64,
            lease_us: u64,
            base_generation: u64,
        ) -> i32;
        fn sys_policy_staged() -> i32;
        fn sys_policy_generation(generation: *mut u64) -> i32;
        fn sys_policy_read_ack(ack: *mut PolicyAck) -> i32;
        fn sys_policy_read(class: u8, service_us: *mut u64) -> i32;
        fn sys_usleep(usecs: u64);
    }

    let (controller, boot) = {
        let mut controller = default_controller();
        let mut boot = BootId::from_hex("00112233445566778899aabbccddeeff").expect("boot id");
        for arg in env::args().skip(1) {
            if let Some(value) = arg.strip_prefix("--controller=") {
                controller = value.to_string();
            } else if let Some(value) = arg.strip_prefix("--boot-id=") {
                boot = BootId::from_hex(value).map_err(|_| "bad boot id".to_string())?;
            } else if arg == "--lose-ack" {
            } else {
                return Err(format!("unknown argument {arg}"));
            }
        }
        (controller, boot)
    };

    static READY: AtomicU32 = AtomicU32::new(0);
    static FAILED: AtomicBool = AtomicBool::new(false);
    for class in [LATENCY, BATCH, MAINTENANCE] {
        thread::spawn(move || {
            if unsafe { sys_policy_register(class) } == 0 {
                READY.fetch_add(1, Ordering::Release);
            } else {
                FAILED.store(true, Ordering::Release);
            }
            loop {
                spin_loop();
            }
        });
    }
    for _ in 0..40 {
        if READY.load(Ordering::Acquire) == 3 || FAILED.load(Ordering::Acquire) {
            break;
        }
        unsafe { sys_usleep(50_000) };
    }
    if FAILED.load(Ordering::Acquire) || READY.load(Ordering::Acquire) != 3 {
        return Err("workers did not register".to_string());
    }

    let mut first = connect_controller(&controller)?;
    let ticket = policy_guest::open_proposal(&mut first, boot, 3_000_000).map_err(|err| err.to_string())?;
    let weights = catalog_spec(CatalogId::CpuV1).profile(ticket.profile).weights();
    let stage_rc = unsafe {
        sys_policy_stage(
            weights[0],
            weights[1],
            weights[2],
            ACCEPTANCE_DEADLINE_US,
            PROFILE_LEASE_US,
            ticket.base_generation,
        )
    };
    if stage_rc != 0 {
        return Err(format!("stage returned {stage_rc}"));
    }
    unsafe { sys_usleep(20_000) };
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
    let ack_rc = unsafe { sys_policy_read_ack(&mut ack) };
    if ack_rc != 0 {
        return Err(format!("scheduler wrote no applied acknowledgment ({ack_rc})"));
    }
    if ack.kind != 1 || ack_profile_name(ack.profile) != ticket.profile.as_str() || ack.generation != ticket.base_generation + 1 {
        return Err("scheduler acknowledgment was not the applied profile".to_string());
    }
    println!(
        "FERRUM_ACK kind=applied reason=none profile={} previous={} generation={} guest_us={} lease_until={}",
        ticket.profile.as_str(),
        ack.previous_generation,
        ack.generation,
        ack.guest_us,
        ack.lease_until_guest_us
    );
    println!("FERRUM_ACK_LOST generation={}", ack.generation);
    std::mem::forget(first);

    let mut second = None;
    let mut last = String::from("not attempted");
    for _ in 0..20 {
        match connect_controller(&controller) {
            Ok(stream) => {
                second = Some(stream);
                break;
            }
            Err(err) => last = err,
        }
        unsafe { sys_usleep(50_000) };
    }
    let mut second = second.ok_or(last)?;
    let recovered = policy_guest::recover_lost_ack(
        &mut second,
        boot,
        4_000_000,
        ack.generation,
        ticket.profile,
    )
    .map_err(|err| err.to_string())?;
    std::mem::forget(second);
    if recovered.generation != ack.generation || recovered.profile != ticket.profile {
        return Err("reconnect snapshot did not report the scheduler".to_string());
    }
    if recovered.reason != policy_types::RejectReason::IdentityMismatch {
        return Err(format!("replay reason was {}", recovered.reason.as_str()));
    }
    println!(
        "FERRUM_RECOVERED generation={} profile={}",
        recovered.generation,
        recovered.profile.as_str()
    );
    println!(
        "FERRUM_REPLAY_REJECTED reason={}",
        recovered.reason.as_str()
    );

    let mut generation = 0u64;
    if unsafe { sys_policy_generation(&mut generation) } != 0 || generation != ack.generation {
        return Err(format!("replay moved the scheduler generation to {generation}"));
    }
    if unsafe { sys_policy_staged() } != 0 {
        return Err("replay left a profile staged".to_string());
    }
    if unsafe { sys_policy_read_ack(&mut ack) } != 1 {
        return Err("replay wrote another scheduler acknowledgment".to_string());
    }

    let read = |class: u8| -> Result<u64, String> {
        let mut service_us = 0u64;
        let rc = unsafe { sys_policy_read(class, &mut service_us) };
        if rc == 0 {
            Ok(service_us)
        } else {
            Err(format!("read class {class} returned {rc}"))
        }
    };
    let before = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
    unsafe { sys_usleep(1_000_000) };
    let after = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
    let sample = (
        after.0.saturating_sub(before.0),
        after.1.saturating_sub(before.1),
        after.2.saturating_sub(before.2),
    );
    println!(
        "FERRUM_SCHED window={} latency={} batch={} maintenance={}",
        ticket.profile.as_str(),
        sample.0, sample.1, sample.2
    );
    let directed = match ticket.profile {
        ProfileId::Balanced => true,
        ProfileId::Latency => sample.0 > sample.1 && sample.0 > sample.2,
        ProfileId::Throughput => sample.1 > sample.0 && sample.1 > sample.2,
        ProfileId::Reclaim => sample.2 > sample.0 && sample.2 > sample.1,
    };
    if !directed || sample.0 == 0 || sample.1 == 0 || sample.2 == 0 {
        return Err("the first application did not keep its service".to_string());
    }
    println!("FERRUM_DUPLICATE_DROPPED");
    println!("FERRUM_SCHED_OK");
    Ok(())
}

fn fair_credit_exit() -> ExitCode {
    match fair_credit() {
        Ok(()) => {
            println!("FERRUM_COMPLETE");
            ExitCode::SUCCESS
        }
        Err(err) => {
            println!("FERRUM_CREDIT_FAIL {err}");
            ExitCode::from(1)
        }
    }
}

#[cfg(not(target_os = "hermit"))]
fn fair_credit() -> Result<(), String> {
    Err("credit check runs inside the hermit guest".to_string())
}

/// A sleeping class must not wake with a private reserve of CPU, and a thread
/// that yields must not receive more service than a sibling that keeps running.
#[cfg(target_os = "hermit")]
fn fair_credit() -> Result<(), String> {
    use std::hint::spin_loop;
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::thread;

    const LATENCY: u8 = 1;
    const BATCH: u8 = 2;
    const PHASE_RUN: u8 = 0;
    const PHASE_SLEEP: u8 = 1;
    const PHASE_YIELD: u8 = 3;

    unsafe extern "C" {
        fn sys_policy_register(class: u8) -> i32;
        fn sys_policy_read(class: u8, service_us: *mut u64) -> i32;
        fn sys_usleep(usecs: u64);
        fn sys_yield();
    }

    static PHASE: AtomicU8 = AtomicU8::new(PHASE_RUN);
    static READY: AtomicU8 = AtomicU8::new(0);
    static WOKE: AtomicBool = AtomicBool::new(false);
    static FAILED: AtomicBool = AtomicBool::new(false);

    thread::spawn(|| {
        if unsafe { sys_policy_register(LATENCY) } != 0 {
            FAILED.store(true, Ordering::Release);
            return;
        }
        READY.fetch_add(1, Ordering::Release);
        let mut slept = false;
        loop {
            match PHASE.load(Ordering::Acquire) {
                PHASE_SLEEP if !slept => {
                    slept = true;
                    unsafe { sys_usleep(400_000) };
                    WOKE.store(true, Ordering::Release);
                }
                PHASE_YIELD => unsafe { sys_yield() },
                _ => spin_loop(),
            }
        }
    });
    thread::spawn(|| {
        if unsafe { sys_policy_register(BATCH) } != 0 {
            FAILED.store(true, Ordering::Release);
            return;
        }
        READY.fetch_add(1, Ordering::Release);
        loop {
            spin_loop();
        }
    });

    for _ in 0..40 {
        if READY.load(Ordering::Acquire) == 2 || FAILED.load(Ordering::Acquire) {
            break;
        }
        unsafe { sys_usleep(50_000) };
    }
    if FAILED.load(Ordering::Acquire) || READY.load(Ordering::Acquire) != 2 {
        return Err("workers did not register".to_string());
    }

    let read = |class: u8| -> Result<u64, String> {
        let mut service_us = 0u64;
        let rc = unsafe { sys_policy_read(class, &mut service_us) };
        if rc == 0 {
            Ok(service_us)
        } else {
            Err(format!("read class {class} returned {rc}"))
        }
    };
    let delta = |before: (u64, u64), after: (u64, u64)| {
        (
            after.0.saturating_sub(before.0),
            after.1.saturating_sub(before.1),
        )
    };

    let before = (read(LATENCY)?, read(BATCH)?);
    PHASE.store(PHASE_SLEEP, Ordering::Release);
    unsafe { sys_usleep(400_000) };
    let slept = delta(before, (read(LATENCY)?, read(BATCH)?));
    println!(
        "FERRUM_CREDIT sleep latency={} batch={}",
        slept.0, slept.1
    );
    if slept.1 < 250_000 || slept.0.saturating_mul(5) > slept.1 {
        return Err("sleeping thread kept running or the sibling did not".to_string());
    }
    for _ in 0..40 {
        if WOKE.load(Ordering::Acquire) {
            break;
        }
        unsafe { sys_usleep(10_000) };
    }
    if !WOKE.load(Ordering::Acquire) {
        return Err("sleeping thread did not wake".to_string());
    }

    let before = (read(LATENCY)?, read(BATCH)?);
    unsafe { sys_usleep(200_000) };
    let woke = delta(before, (read(LATENCY)?, read(BATCH)?));
    println!("FERRUM_CREDIT wake latency={} batch={}", woke.0, woke.1);
    let wake_sum = woke.0 + woke.1;
    // A banked sleeper would take this whole window. Clamping keeps both classes in it.
    if wake_sum < 100_000
        || woke.0 == 0
        || woke.1 == 0
        || woke.0.saturating_mul(3) > wake_sum.saturating_mul(2)
        || woke.1.saturating_mul(3) > wake_sum.saturating_mul(2)
    {
        return Err("woken thread banked the sleep".to_string());
    }

    let before = (read(LATENCY)?, read(BATCH)?);
    PHASE.store(PHASE_YIELD, Ordering::Release);
    unsafe { sys_usleep(400_000) };
    let yielded = delta(before, (read(LATENCY)?, read(BATCH)?));
    println!(
        "FERRUM_CREDIT yield latency={} batch={}",
        yielded.0, yielded.1
    );
    // Yielding does not pay the thread more than the sibling that kept running.
    if yielded.0 == 0 || yielded.1 == 0 || yielded.0 > yielded.1.saturating_mul(5) / 4 {
        return Err("yielding thread received more service than the runner".to_string());
    }
    println!("FERRUM_CREDIT_OK");
    Ok(())
}

fn fair_queue_exit() -> ExitCode {
    match fair_queue() {
        Ok(()) => {
            println!("FERRUM_COMPLETE");
            ExitCode::SUCCESS
        }
        Err(err) => {
            println!("FERRUM_QUEUE_FAIL {err}");
            ExitCode::from(1)
        }
    }
}

#[cfg(not(target_os = "hermit"))]
fn fair_queue() -> Result<(), String> {
    Err("queue check runs inside the hermit guest".to_string())
}

/// Stage a profile and, before the scheduler installs it, exit one thread and
/// block another. The ready queue must still apply that profile once.
#[cfg(target_os = "hermit")]
fn fair_queue() -> Result<(), String> {
    use std::hint::spin_loop;
    use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
    use std::thread;

    const LATENCY: u8 = 1;
    const BATCH: u8 = 2;
    const MAINTENANCE: u8 = 3;
    const PHASE_WAIT: u8 = 0;
    const PHASE_EXIT: u8 = 1;
    const PHASE_BLOCK: u8 = 2;

    unsafe extern "C" {
        fn sys_policy_register(class: u8) -> i32;
        fn sys_policy_stage(
            latency: u32,
            batch: u32,
            maintenance: u32,
            accept_us: u64,
            lease_us: u64,
            base_generation: u64,
        ) -> i32;
        fn sys_policy_staged() -> i32;
        fn sys_policy_generation(generation: *mut u64) -> i32;
        fn sys_policy_read_ack(ack: *mut PolicyAck) -> i32;
        fn sys_policy_read(class: u8, service_us: *mut u64) -> i32;
        fn sys_usleep(usecs: u64);
    }

    static PHASE: AtomicU8 = AtomicU8::new(PHASE_WAIT);
    static READY: AtomicU8 = AtomicU8::new(0);
    static WOKE: AtomicBool = AtomicBool::new(false);
    static FAILED: AtomicBool = AtomicBool::new(false);
    static BASE: AtomicU64 = AtomicU64::new(1);

    thread::spawn(|| {
        if unsafe { sys_policy_register(LATENCY) } != 0 {
            FAILED.store(true, Ordering::Release);
            return;
        }
        READY.fetch_add(1, Ordering::Release);
        loop {
            if PHASE.load(Ordering::Acquire) == PHASE_EXIT {
                break;
            }
            spin_loop();
        }
        let rc = unsafe { sys_policy_stage(2, 6, 2, 5_000_000, 3_000_000, 1) };
        if rc != 0 {
            FAILED.store(true, Ordering::Release);
        }
    });
    thread::spawn(|| {
        if unsafe { sys_policy_register(BATCH) } != 0 {
            FAILED.store(true, Ordering::Release);
            return;
        }
        READY.fetch_add(1, Ordering::Release);
        loop {
            if PHASE.load(Ordering::Acquire) == PHASE_BLOCK {
                break;
            }
            spin_loop();
        }
        let base = BASE.load(Ordering::Acquire);
        let rc = unsafe { sys_policy_stage(2, 2, 6, 5_000_000, 3_000_000, base) };
        if rc != 0 {
            FAILED.store(true, Ordering::Release);
            return;
        }
        unsafe { sys_usleep(250_000) };
        WOKE.store(true, Ordering::Release);
        loop {
            spin_loop();
        }
    });
    thread::spawn(|| {
        if unsafe { sys_policy_register(MAINTENANCE) } != 0 {
            FAILED.store(true, Ordering::Release);
            return;
        }
        READY.fetch_add(1, Ordering::Release);
        loop {
            spin_loop();
        }
    });

    for _ in 0..40 {
        if READY.load(Ordering::Acquire) == 3 || FAILED.load(Ordering::Acquire) {
            break;
        }
        unsafe { sys_usleep(50_000) };
    }
    if FAILED.load(Ordering::Acquire) || READY.load(Ordering::Acquire) != 3 {
        return Err("workers did not register".to_string());
    }

    let generation = || -> Result<u64, String> {
        let mut generation = 0u64;
        let rc = unsafe { sys_policy_generation(&mut generation) };
        if rc == 0 {
            Ok(generation)
        } else {
            Err(format!("generation returned {rc}"))
        }
    };
    let read = |class: u8| -> Result<u64, String> {
        let mut service_us = 0u64;
        let rc = unsafe { sys_policy_read(class, &mut service_us) };
        if rc == 0 {
            Ok(service_us)
        } else {
            Err(format!("read class {class} returned {rc}"))
        }
    };
    let pull_ack = || -> Result<Option<PolicyAck>, String> {
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
        let rc = unsafe { sys_policy_read_ack(&mut ack) };
        if rc == 0 {
            Ok(Some(ack))
        } else if rc == 1 {
            Ok(None)
        } else {
            Err(format!("read ack returned {rc}"))
        }
    };
    let wait_applied = |previous: u64, profile: u8| -> Result<PolicyAck, String> {
        for _ in 0..50 {
            if FAILED.load(Ordering::Acquire) {
                return Err("worker failed while a profile was staged".to_string());
            }
            if unsafe { sys_policy_staged() } == 0 && generation()? == previous + 1 {
                break;
            }
            unsafe { sys_usleep(10_000) };
        }
        if unsafe { sys_policy_staged() } != 0 {
            return Err("staged profile was still pending".to_string());
        }
        let Some(ack) = pull_ack()? else {
            return Err("scheduler wrote no acknowledgment".to_string());
        };
        if ack.kind != 1 || ack.profile != profile || ack.previous_generation != previous || ack.generation != previous + 1 {
            return Err(format!(
                "acknowledgment kind={} profile={} previous={} generation={}",
                ack.kind, ack.profile, ack.previous_generation, ack.generation
            ));
        }
        if pull_ack()?.is_some() {
            return Err("scheduler wrote an extra acknowledgment".to_string());
        }
        println!(
            "FERRUM_ACK kind=applied reason=none profile={} previous={} generation={}",
            ack_profile_name(ack.profile),
            ack.previous_generation,
            ack.generation
        );
        Ok(ack)
    };

    if generation()? != 1 {
        return Err("boot generation was not 1".to_string());
    }
    PHASE.store(PHASE_EXIT, Ordering::Release);
    let exit_ack = wait_applied(1, 2)?;
    let before = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
    unsafe { sys_usleep(150_000) };
    let after = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
    let exit_window = (
        after.0.saturating_sub(before.0),
        after.1.saturating_sub(before.1),
        after.2.saturating_sub(before.2),
    );
    println!(
        "FERRUM_QUEUE exit generation={} latency={} batch={} maintenance={}",
        exit_ack.generation, exit_window.0, exit_window.1, exit_window.2
    );
    if exit_window.0 > 2_000 || exit_window.1 <= exit_window.2 || exit_window.1 < 50_000 {
        return Err("exited thread stayed runnable or throughput was not applied".to_string());
    }
    // Reclaim is a different profile, so it has to wait out the throughput dwell.
    unsafe { sys_usleep(2_000_000) };

    let base = generation()?;
    BASE.store(base, Ordering::Release);
    PHASE.store(PHASE_BLOCK, Ordering::Release);
    let block_ack = wait_applied(base, 3)?;
    let before = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
    unsafe { sys_usleep(150_000) };
    let after = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
    let blocked = (
        after.0.saturating_sub(before.0),
        after.1.saturating_sub(before.1),
        after.2.saturating_sub(before.2),
    );
    println!(
        "FERRUM_QUEUE block generation={} latency={} batch={} maintenance={}",
        block_ack.generation, blocked.0, blocked.1, blocked.2
    );
    if blocked.0 > 2_000 || blocked.1 > 2_000 || blocked.2 < 80_000 {
        return Err("blocked or exited thread was scheduled".to_string());
    }
    for _ in 0..40 {
        if WOKE.load(Ordering::Acquire) {
            break;
        }
        unsafe { sys_usleep(10_000) };
    }
    if !WOKE.load(Ordering::Acquire) {
        return Err("blocked thread did not return to the run queue".to_string());
    }
    let before = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
    unsafe { sys_usleep(200_000) };
    let after = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
    let woke = (
        after.0.saturating_sub(before.0),
        after.1.saturating_sub(before.1),
        after.2.saturating_sub(before.2),
    );
    println!(
        "FERRUM_QUEUE wake latency={} batch={} maintenance={}",
        woke.0, woke.1, woke.2
    );
    if woke.0 > 2_000 || woke.1 == 0 || woke.2 <= woke.1 {
        return Err("woken thread missed the reclaim profile or the exited thread ran".to_string());
    }
    println!("FERRUM_QUEUE_OK");
    Ok(())
}

fn reject_frames_exit() -> ExitCode {
    match reject_frames() {
        Ok(()) => {
            println!("FERRUM_COMPLETE");
            ExitCode::SUCCESS
        }
        Err(err) => {
            println!("FERRUM_FRAME_FAIL {err}");
            ExitCode::from(1)
        }
    }
}

#[cfg(not(target_os = "hermit"))]
fn reject_frames() -> Result<(), String> {
    Err("frame rejection runs inside the hermit guest".to_string())
}

/// Refuse bad controller frames and leave the boot profile in place.
#[cfg(target_os = "hermit")]
fn reject_frames() -> Result<(), String> {
    use std::hint::spin_loop;
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::thread;

    use policy_guest::FrameFault;

    const LATENCY: u8 = 1;
    const BATCH: u8 = 2;
    const MAINTENANCE: u8 = 3;

    unsafe extern "C" {
        fn sys_policy_register(class: u8) -> i32;
        fn sys_policy_staged() -> i32;
        fn sys_policy_generation(generation: *mut u64) -> i32;
        fn sys_policy_read_ack(ack: *mut PolicyAck) -> i32;
        fn sys_policy_read(class: u8, service_us: *mut u64) -> i32;
        fn sys_usleep(usecs: u64);
    }

    let (controller, boot) = {
        let mut controller = default_controller();
        let mut boot = BootId::from_hex("00112233445566778899aabbccddeeff").expect("boot id");
        for arg in env::args().skip(1) {
            if let Some(value) = arg.strip_prefix("--controller=") {
                controller = value.to_string();
            } else if let Some(value) = arg.strip_prefix("--boot-id=") {
                boot = BootId::from_hex(value).map_err(|_| "bad boot id".to_string())?;
            } else if arg == "--reject-frames" {
            } else {
                return Err(format!("unknown argument {arg}"));
            }
        }
        (controller, boot)
    };

    let mut stream = connect_controller(&controller)?;
    let faults = policy_guest::reject_bad_frames(&mut stream, boot, 3_000_000).map_err(|err| err.to_string())?;
    std::mem::forget(stream);
    let expected = [
        FrameFault::Unauthenticated,
        FrameFault::Duplicate,
        FrameFault::Invalid,
        FrameFault::Oversized,
    ];
    if faults != expected {
        return Err(format!(
            "frame faults were {} {} {} {}",
            faults[0].as_str(),
            faults[1].as_str(),
            faults[2].as_str(),
            faults[3].as_str()
        ));
    }
    for fault in faults {
        println!("FERRUM_FRAME_REJECTED {}", fault.as_str());
    }

    let mut generation = 0u64;
    if unsafe { sys_policy_generation(&mut generation) } != 0 || generation != 1 {
        return Err(format!("rejected frames moved generation to {generation}"));
    }
    if unsafe { sys_policy_staged() } != 0 {
        return Err("rejected frames left a profile staged".to_string());
    }
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
    if unsafe { sys_policy_read_ack(&mut ack) } != 1 {
        return Err("rejected frames wrote a scheduler acknowledgment".to_string());
    }

    static READY: AtomicU8 = AtomicU8::new(0);
    static FAILED: AtomicBool = AtomicBool::new(false);
    for class in [LATENCY, BATCH, MAINTENANCE] {
        thread::spawn(move || {
            if unsafe { sys_policy_register(class) } == 0 {
                READY.fetch_add(1, Ordering::Release);
            } else {
                FAILED.store(true, Ordering::Release);
            }
            loop {
                spin_loop();
            }
        });
    }
    for _ in 0..40 {
        if READY.load(Ordering::Acquire) == 3 || FAILED.load(Ordering::Acquire) {
            break;
        }
        unsafe { sys_usleep(50_000) };
    }
    if FAILED.load(Ordering::Acquire) || READY.load(Ordering::Acquire) != 3 {
        return Err("workers did not register".to_string());
    }
    let read = |class: u8| -> Result<u64, String> {
        let mut service_us = 0u64;
        let rc = unsafe { sys_policy_read(class, &mut service_us) };
        if rc == 0 {
            Ok(service_us)
        } else {
            Err(format!("read class {class} returned {rc}"))
        }
    };
    let before = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
    unsafe { sys_usleep(1_000_000) };
    let after = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
    let sample = (
        after.0.saturating_sub(before.0),
        after.1.saturating_sub(before.1),
        after.2.saturating_sub(before.2),
    );
    println!(
        "FERRUM_SCHED window=balanced latency={} batch={} maintenance={}",
        sample.0, sample.1, sample.2
    );
    let sum = sample.0 + sample.1 + sample.2;
    let near_even = sum > 800_000
        && sample.0 > 0
        && sample.1 > 0
        && sample.2 > 0
        && sample.0 * 20 < sum * 9
        && sample.1 * 20 < sum * 9
        && sample.2 * 20 < sum * 9;
    if !near_even {
        return Err("rejected frames changed class service".to_string());
    }
    println!("FERRUM_FRAMES_OK");
    Ok(())
}

fn fair_dwell_exit() -> ExitCode {
    match fair_dwell() {
        Ok(()) => {
            println!("FERRUM_COMPLETE");
            ExitCode::SUCCESS
        }
        Err(err) => {
            println!("FERRUM_DWELL_FAIL {err}");
            ExitCode::from(1)
        }
    }
}

#[cfg(not(target_os = "hermit"))]
fn fair_dwell() -> Result<(), String> {
    Err("dwell check runs inside the hermit guest".to_string())
}

/// Renew latency inside the dwell window, then accept a different profile
/// once the original change time has aged out. The renewal must not push
/// that deadline forward.
#[cfg(target_os = "hermit")]
fn fair_dwell() -> Result<(), String> {
    use std::hint::spin_loop;
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::thread;

    const LATENCY: u8 = 1;
    const BATCH: u8 = 2;
    const MAINTENANCE: u8 = 3;
    const MIN_DWELL_US: u64 = 2_000_000;

    unsafe extern "C" {
        fn sys_policy_register(class: u8) -> i32;
        fn sys_policy_stage(
            latency: u32,
            batch: u32,
            maintenance: u32,
            accept_us: u64,
            lease_us: u64,
            base_generation: u64,
        ) -> i32;
        fn sys_policy_generation(generation: *mut u64) -> i32;
        fn sys_policy_read_ack(ack: *mut PolicyAck) -> i32;
        fn sys_policy_read(class: u8, service_us: *mut u64) -> i32;
        fn sys_usleep(usecs: u64);
    }

    static READY: AtomicU8 = AtomicU8::new(0);
    static FAILED: AtomicBool = AtomicBool::new(false);
    for class in [LATENCY, BATCH, MAINTENANCE] {
        thread::spawn(move || {
            if unsafe { sys_policy_register(class) } == 0 {
                READY.fetch_add(1, Ordering::Release);
            } else {
                FAILED.store(true, Ordering::Release);
            }
            loop {
                spin_loop();
            }
        });
    }
    for _ in 0..40 {
        if READY.load(Ordering::Acquire) == 3 || FAILED.load(Ordering::Acquire) {
            break;
        }
        unsafe { sys_usleep(50_000) };
    }
    if FAILED.load(Ordering::Acquire) || READY.load(Ordering::Acquire) != 3 {
        return Err("workers did not register".to_string());
    }

    let generation = || -> Result<u64, String> {
        let mut generation = 0u64;
        let rc = unsafe { sys_policy_generation(&mut generation) };
        if rc == 0 {
            Ok(generation)
        } else {
            Err(format!("generation returned {rc}"))
        }
    };
    let stage = |weights: (u32, u32, u32), base: u64| -> Result<(), String> {
        let rc = unsafe { sys_policy_stage(weights.0, weights.1, weights.2, 5_000_000, 3_000_000, base) };
        if rc == 0 {
            Ok(())
        } else {
            Err(format!("stage returned {rc}"))
        }
    };
    let pull_ack = || -> Result<PolicyAck, String> {
        for _ in 0..50 {
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
            let rc = unsafe { sys_policy_read_ack(&mut ack) };
            if rc == 0 {
                return Ok(ack);
            }
            if rc != 1 {
                return Err(format!("read ack returned {rc}"));
            }
            unsafe { sys_usleep(10_000) };
        }
        Err("scheduler wrote no acknowledgment".to_string())
    };

    stage((6, 2, 2), generation()?)?;
    let first = pull_ack()?;
    if first.kind != 1 || first.profile != 1 || first.generation != 2 {
        return Err("latency profile was not applied".to_string());
    }
    println!(
        "FERRUM_DWELL_APPLIED profile=latency generation={} guest_us={} lease_until={}",
        first.generation, first.guest_us, first.lease_until_guest_us
    );

    unsafe { sys_usleep(200_000) };
    let during = generation()?;
    stage((2, 6, 2), during)?;
    let rejected = pull_ack()?;
    if rejected.kind != 4 || rejected.reason != 4 || rejected.generation != during {
        return Err(format!(
            "early throughput was not held for dwell (kind={} reason={} generation={})",
            rejected.kind, rejected.reason, rejected.generation
        ));
    }
    println!(
        "FERRUM_DWELL_REJECTED reason=dwell generation={} guest_us={}",
        rejected.generation, rejected.guest_us
    );

    unsafe { sys_usleep(1_300_000) };
    let renew_base = generation()?;
    stage((6, 2, 2), renew_base)?;
    let renewed = pull_ack()?;
    if renewed.kind != 1 || renewed.profile != 1 || renewed.generation != renew_base + 1 {
        return Err("identical profile was not renewed".to_string());
    }
    if renewed.lease_until_guest_us <= first.lease_until_guest_us {
        return Err("renewal did not extend the lease".to_string());
    }
    println!(
        "FERRUM_DWELL_RENEWED profile=latency previous={} generation={} guest_us={} lease_until={}",
        renewed.previous_generation,
        renewed.generation,
        renewed.guest_us,
        renewed.lease_until_guest_us
    );

    unsafe { sys_usleep(700_000) };
    let change_base = generation()?;
    stage((2, 6, 2), change_base)?;
    let changed = pull_ack()?;
    if changed.kind != 1 || changed.profile != 2 || changed.generation != change_base + 1 {
        return Err(format!(
            "throughput was not accepted after the original dwell (kind={} reason={} generation={})",
            changed.kind, changed.reason, changed.generation
        ));
    }
    if changed.guest_us < first.guest_us.saturating_add(MIN_DWELL_US) {
        return Err("throughput was accepted before the original dwell elapsed".to_string());
    }
    if changed.guest_us >= renewed.guest_us.saturating_add(MIN_DWELL_US) {
        return Err("throughput waited for a dwell timer that the renewal restarted".to_string());
    }
    println!(
        "FERRUM_DWELL_CHANGED profile=throughput previous={} generation={} guest_us={}",
        changed.previous_generation, changed.generation, changed.guest_us
    );

    let read = |class: u8| -> Result<u64, String> {
        let mut service_us = 0u64;
        let rc = unsafe { sys_policy_read(class, &mut service_us) };
        if rc == 0 {
            Ok(service_us)
        } else {
            Err(format!("read class {class} returned {rc}"))
        }
    };
    let before = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
    unsafe { sys_usleep(200_000) };
    let after = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
    let sample = (
        after.0.saturating_sub(before.0),
        after.1.saturating_sub(before.1),
        after.2.saturating_sub(before.2),
    );
    println!(
        "FERRUM_SCHED window=throughput latency={} batch={} maintenance={}",
        sample.0, sample.1, sample.2
    );
    if sample.1 <= sample.0 || sample.1 <= sample.2 {
        return Err("throughput weights were not in force".to_string());
    }
    println!("FERRUM_DWELL_OK");
    Ok(())
}

fn fair_demo_exit() -> ExitCode {
    match fair_demo() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            println!("FERRUM_SCHED_FAIL {err}");
            ExitCode::from(1)
        }
    }
}

#[cfg(target_os = "hermit")]
fn fair_demo() -> Result<(), String> {
    use std::hint::spin_loop;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::thread;

    const LATENCY: u8 = 1;
    const BATCH: u8 = 2;
    const MAINTENANCE: u8 = 3;

    unsafe extern "C" {
        fn sys_policy_register(class: u8) -> i32;
        fn sys_policy_set_weights(latency: u32, batch: u32, maintenance: u32) -> i32;
        fn sys_policy_read(class: u8, service_us: *mut u64) -> i32;
        fn sys_usleep(usecs: u64);
    }

    static READY: AtomicU32 = AtomicU32::new(0);
    static FAILED: AtomicBool = AtomicBool::new(false);

    fn read_service(class: u8) -> Result<u64, String> {
        let mut service_us = 0u64;
        let rc = unsafe { sys_policy_read(class, &mut service_us) };
        if rc == 0 {
            Ok(service_us)
        } else {
            Err(format!("read class {class} returned {rc}"))
        }
    }

    fn snapshot() -> Result<(u64, u64, u64), String> {
        Ok((
            read_service(LATENCY)?,
            read_service(BATCH)?,
            read_service(MAINTENANCE)?,
        ))
    }

    fn window(name: &str, weights: (u32, u32, u32)) -> Result<(u64, u64, u64), String> {
        let rc = unsafe { sys_policy_set_weights(weights.0, weights.1, weights.2) };
        if rc != 0 {
            return Err(format!("{name} weights returned {rc}"));
        }
        let before = snapshot()?;
        unsafe { sys_usleep(1_000_000) };
        let after = snapshot()?;
        let delta = (
            after.0.saturating_sub(before.0),
            after.1.saturating_sub(before.1),
            after.2.saturating_sub(before.2),
        );
        println!(
            "FERRUM_SCHED window={name} latency={} batch={} maintenance={}",
            delta.0, delta.1, delta.2
        );
        Ok(delta)
    }

    for class in [LATENCY, BATCH, MAINTENANCE] {
        thread::spawn(move || {
            if unsafe { sys_policy_register(class) } == 0 {
                READY.fetch_add(1, Ordering::Release);
            } else {
                FAILED.store(true, Ordering::Release);
            }
            loop {
                spin_loop();
            }
        });
    }

    for _ in 0..40 {
        if READY.load(Ordering::Acquire) == 3 || FAILED.load(Ordering::Acquire) {
            break;
        }
        unsafe { sys_usleep(50_000) };
    }
    if FAILED.load(Ordering::Acquire) {
        return Err("a worker could not register".to_string());
    }
    if READY.load(Ordering::Acquire) != 3 {
        return Err("workers did not register".to_string());
    }

    let balanced = window("balanced", (1, 1, 1))?;
    let latency = window("latency", (6, 2, 2))?;
    let throughput = window("throughput", (2, 6, 2))?;
    let reclaim = window("reclaim", (2, 2, 6))?;

    let served = |sample: (u64, u64, u64)| sample.0 > 0 && sample.1 > 0 && sample.2 > 0;
    let passed = served(balanced)
        && served(latency)
        && served(throughput)
        && served(reclaim)
        && latency.0 > latency.1
        && latency.0 > latency.2
        && throughput.1 > throughput.0
        && throughput.1 > throughput.2
        && reclaim.2 > reclaim.0
        && reclaim.2 > reclaim.1;
    if !passed {
        return Err("service did not follow the class weights".to_string());
    }
    println!("FERRUM_SCHED_OK");
    println!("FERRUM_COMPLETE");
    Ok(())
}

#[cfg(not(target_os = "hermit"))]
fn fair_demo() -> Result<(), String> {
    Err("fair demo runs inside the hermit guest".to_string())
}

fn fair_progress_exit() -> ExitCode {
    match fair_progress() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            println!("FERRUM_PROGRESS_FAIL {err}");
            ExitCode::from(1)
        }
    }
}

#[cfg(target_os = "hermit")]
fn fair_progress() -> Result<(), String> {
    use std::hint::spin_loop;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::thread;

    const LATENCY: u8 = 1;
    const BATCH: u8 = 2;
    const MAINTENANCE: u8 = 3;
    const THREADS: [(u8, u8); 8] = [
        (LATENCY, 0),
        (LATENCY, 1),
        (LATENCY, 2),
        (BATCH, 0),
        (BATCH, 1),
        (BATCH, 2),
        (MAINTENANCE, 0),
        (MAINTENANCE, 1),
    ];

    unsafe extern "C" {
        fn sys_policy_register(class: u8) -> i32;
        fn sys_policy_set_weights(latency: u32, batch: u32, maintenance: u32) -> i32;
        fn sys_policy_member(slot: u8, class: *mut u8, service_us: *mut u64) -> i32;
        fn sys_usleep(usecs: u64);
    }

    static READY: AtomicU32 = AtomicU32::new(0);
    static FAILED: AtomicBool = AtomicBool::new(false);

    fn workload() -> Result<[(u8, u8, u64); 8], String> {
        let mut found = [(0u8, 0u8, 0u64); 8];
        let mut count = 0usize;
        for slot in 0..12u8 {
            let mut class = 0u8;
            let mut service_us = 0u64;
            let rc = unsafe { sys_policy_member(slot, &mut class, &mut service_us) };
            if rc != 0 {
                continue;
            }
            if !matches!(class, LATENCY | BATCH | MAINTENANCE) {
                continue;
            }
            if count >= found.len() {
                return Err("more than 8 workload threads registered".to_string());
            }
            found[count] = (slot, class, service_us);
            count += 1;
        }
        if count != found.len() {
            return Err(format!("found {count} workload threads"));
        }
        Ok(found)
    }

    let rc = unsafe { sys_policy_set_weights(6, 2, 2) };
    if rc != 0 {
        return Err(format!("latency weights returned {rc}"));
    }

    for (class, _) in THREADS {
        thread::spawn(move || {
            if unsafe { sys_policy_register(class) } == 0 {
                READY.fetch_add(1, Ordering::Release);
            } else {
                FAILED.store(true, Ordering::Release);
            }
            loop {
                spin_loop();
            }
        });
    }

    for _ in 0..40 {
        if READY.load(Ordering::Acquire) == THREADS.len() as u32 || FAILED.load(Ordering::Acquire) {
            break;
        }
        unsafe { sys_usleep(50_000) };
    }
    if FAILED.load(Ordering::Acquire) {
        return Err("a worker could not register".to_string());
    }
    if READY.load(Ordering::Acquire) != THREADS.len() as u32 {
        return Err("workers did not register".to_string());
    }

    let before = workload()?;
    unsafe { sys_usleep(500_000) };
    let after = workload()?;

    let mut report = String::new();
    let mut all_moved = true;
    for (start, end) in before.iter().zip(after.iter()) {
        if start.0 != end.0 || start.1 != end.1 {
            return Err("registration slots changed during the window".to_string());
        }
        let delta = end.2.saturating_sub(start.2);
        if !report.is_empty() {
            report.push(',');
        }
        report.push_str(&format!("{}:{}", end.1, delta));
        if delta == 0 {
            all_moved = false;
        }
    }
    println!("FERRUM_PROGRESS profile=latency threads=8 window_us=500000 deltas={report}");
    if !all_moved {
        return Err("a runnable thread received no service within 500 ms".to_string());
    }
    println!("FERRUM_PROGRESS_OK");
    println!("FERRUM_COMPLETE");
    Ok(())
}

#[cfg(not(target_os = "hermit"))]
fn fair_progress() -> Result<(), String> {
    Err("progress check runs inside the hermit guest".to_string())
}

#[cfg(target_os = "hermit")]
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

#[cfg(target_os = "hermit")]
fn ack_kind_name(kind: u8) -> &'static str {
    match kind {
        1 => "applied",
        2 => "lease_expired",
        3 => "emergency",
        4 => "rejected",
        _ => "unknown",
    }
}

#[cfg(target_os = "hermit")]
fn ack_reason_name(reason: u8) -> &'static str {
    match reason {
        0 => "none",
        1 => "emergency",
        2 => "late",
        3 => "stale",
        4 => "dwell",
        _ => "unknown",
    }
}

#[cfg(target_os = "hermit")]
fn ack_profile_name(profile: u8) -> &'static str {
    match profile {
        0 => "balanced",
        1 => "latency",
        2 => "throughput",
        3 => "reclaim",
        _ => "unknown",
    }
}

#[cfg(target_os = "hermit")]
fn publish_scheduler(
    stream: &mut TcpStream,
    ticket: &policy_guest::ProposalTicket,
    ack: &PolicyAck,
) -> Result<(), String> {
    use policy_types::RejectReason;

    match ack.kind {
        1 => {
            if ack_profile_name(ack.profile) != ticket.profile.as_str() {
                return Err("scheduler applied a different profile than the proposal".to_string());
            }
            println!(
                "FERRUM_APPLIED profile={} previous={} generation={} guest_us={} lease_until={}",
                ticket.profile.as_str(),
                ack.previous_generation,
                ack.generation,
                ack.guest_us,
                ack.lease_until_guest_us
            );
            policy_guest::report_applied(
                stream,
                ticket,
                ack.previous_generation,
                ack.generation,
                ack.guest_us,
                ack.lease_until_guest_us,
            )
            .map_err(|err| err.to_string())
        }
        3 | 4 => {
            let reason = if ack.kind == 3 {
                RejectReason::Emergency
            } else {
                match ack.reason {
                    1 => RejectReason::Emergency,
                    2 => RejectReason::Late,
                    3 => RejectReason::StaleGeneration,
                    other => return Err(format!("scheduler reject reason {other} has no protocol name")),
                }
            };
            println!("FERRUM_REPORT kind=reject reason={}", reason.as_str());
            policy_guest::report_reject(stream, ticket, reason).map_err(|err| err.to_string())
        }
        other => Err(format!("scheduler acknowledgment kind {other} does not answer the proposal")),
    }
}

/// Watch the controller socket without blocking the workload. A later
/// `note_controller` records whether the peer disappeared during the lease.
#[cfg(target_os = "hermit")]
fn watch_controller(stream: &TcpStream) -> Option<std::sync::mpsc::Receiver<std::io::Result<usize>>> {
    use std::io::Read;
    use std::sync::mpsc;
    use std::thread;

    let mut probe = stream.try_clone().ok()?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; 1];
        let _ = tx.send(probe.read(&mut buf));
    });
    Some(rx)
}

#[cfg(target_os = "hermit")]
fn note_controller(peer: Option<std::sync::mpsc::Receiver<std::io::Result<usize>>>) {
    let Some(peer) = peer else {
        println!("FERRUM_CONTROLLER_UNWATCHED");
        return;
    };
    match peer.try_recv() {
        Ok(Ok(0)) => println!("FERRUM_CONTROLLER_LOST closed"),
        Ok(Err(err)) => println!("FERRUM_CONTROLLER_LOST {err}"),
        Ok(Ok(n)) => println!("FERRUM_CONTROLLER_LOST bytes={n}"),
        Err(_) => println!("FERRUM_CONTROLLER_HELD"),
    }
}

#[cfg(target_os = "hermit")]
fn actuate(
    stream: &mut TcpStream,
    ticket: &policy_guest::ProposalTicket,
    emergency: bool,
    stage_race: bool,
    stale: bool,
    late: bool,
) -> Result<(), String> {
    use std::hint::spin_loop;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::thread;

    use policy_core::catalog_spec;
    use policy_types::{CatalogId, ProfileId, ACCEPTANCE_DEADLINE_US, PROFILE_LEASE_US};

    const LATENCY: u8 = 1;
    const BATCH: u8 = 2;
    const MAINTENANCE: u8 = 3;

    unsafe extern "C" {
        fn sys_policy_register(class: u8) -> i32;
        fn sys_policy_set_weights(latency: u32, batch: u32, maintenance: u32) -> i32;
        fn sys_policy_lease_expired() -> i32;
        fn sys_policy_install_emergency() -> i32;
        fn sys_policy_stage(
            latency: u32,
            batch: u32,
            maintenance: u32,
            accept_us: u64,
            lease_us: u64,
            base_generation: u64,
        ) -> i32;
        fn sys_policy_staged() -> i32;
        fn sys_policy_generation(generation: *mut u64) -> i32;
        fn sys_policy_read_ack(ack: *mut PolicyAck) -> i32;
        fn sys_policy_read(class: u8, service_us: *mut u64) -> i32;
        fn sys_usleep(usecs: u64);
    }

    let profile = ticket.profile.as_str();
    let id = ticket.profile;
    let weights = catalog_spec(CatalogId::CpuV1).profile(id).weights();

    static READY: AtomicU32 = AtomicU32::new(0);
    static FAILED: AtomicBool = AtomicBool::new(false);

    for class in [LATENCY, BATCH, MAINTENANCE] {
        thread::spawn(move || {
            if unsafe { sys_policy_register(class) } == 0 {
                READY.fetch_add(1, Ordering::Release);
            } else {
                FAILED.store(true, Ordering::Release);
            }
            loop {
                spin_loop();
            }
        });
    }

    for _ in 0..40 {
        if READY.load(Ordering::Acquire) == 3 || FAILED.load(Ordering::Acquire) {
            break;
        }
        unsafe { sys_usleep(50_000) };
    }
    if FAILED.load(Ordering::Acquire) {
        return Err("a worker could not register".to_string());
    }
    if READY.load(Ordering::Acquire) != 3 {
        return Err("workers did not register".to_string());
    }

    let stage = |accept_us: u64, base_generation: u64| -> Result<(), String> {
        let rc = unsafe {
            sys_policy_stage(
                weights[0],
                weights[1],
                weights[2],
                accept_us,
                PROFILE_LEASE_US,
                base_generation,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(format!("stage returned {rc}"))
        }
    };
    let generation = || -> Result<u64, String> {
        let mut generation = 0u64;
        let rc = unsafe { sys_policy_generation(&mut generation) };
        if rc == 0 {
            Ok(generation)
        } else {
            Err(format!("generation returned {rc}"))
        }
    };
    let pull_ack = || -> Result<Option<PolicyAck>, String> {
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
        let rc = unsafe { sys_policy_read_ack(&mut ack) };
        if rc == 0 {
            Ok(Some(ack))
        } else if rc == 1 {
            Ok(None)
        } else {
            Err(format!("read ack returned {rc}"))
        }
    };
    let require_ack = |kind: u8, reason: u8, profile: u8, previous: u64, generation: u64| -> Result<PolicyAck, String> {
        let Some(ack) = pull_ack()? else {
            return Err("scheduler wrote no acknowledgment".to_string());
        };
        println!(
            "FERRUM_ACK kind={} reason={} profile={} previous={} generation={} guest_us={} lease_until={}",
            ack_kind_name(ack.kind),
            ack_reason_name(ack.reason),
            ack_profile_name(ack.profile),
            ack.previous_generation,
            ack.generation,
            ack.guest_us,
            ack.lease_until_guest_us
        );
        if ack.kind != kind
            || ack.reason != reason
            || ack.profile != profile
            || ack.previous_generation != previous
            || ack.generation != generation
            || ack.guest_us == 0
        {
            return Err("scheduler acknowledgment did not match".to_string());
        }
        if pull_ack()?.is_some() {
            return Err("scheduler wrote an extra acknowledgment".to_string());
        }
        Ok(ack)
    };

    if stale {
        let current = generation()?;
        if current != 1 {
            return Err(format!("boot generation was {current}"));
        }
        // Acceptance stays open. The only reason to drop this stage is that
        // its base generation is not the scheduler's current generation.
        stage(5_000_000, 0)?;
        let read = |class: u8| -> Result<u64, String> {
            let mut service_us = 0u64;
            let rc = unsafe { sys_policy_read(class, &mut service_us) };
            if rc == 0 {
                Ok(service_us)
            } else {
                Err(format!("read class {class} returned {rc}"))
            }
        };
        let before = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
        unsafe { sys_usleep(1_000_000) };
        let after = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
        let sample = (
            after.0.saturating_sub(before.0),
            after.1.saturating_sub(before.1),
            after.2.saturating_sub(before.2),
        );
        println!(
            "FERRUM_SCHED window=balanced latency={} batch={} maintenance={}",
            sample.0, sample.1, sample.2
        );
        let sum = sample.0 + sample.1 + sample.2;
        let near_even = sum > 0
            && sample.0 > 0
            && sample.1 > 0
            && sample.2 > 0
            && sample.0 * 20 < sum * 9
            && sample.1 * 20 < sum * 9
            && sample.2 * 20 < sum * 9;
        if !near_even {
            return Err("stale stage changed class service".to_string());
        }
        if unsafe { sys_policy_staged() } != 0 {
            return Err("scheduler left the stale profile staged".to_string());
        }
        if generation()? != 1 {
            return Err("stale stage advanced the generation".to_string());
        }
        let ack = require_ack(4, 3, 0, 1, 1)?;
        if ack.lease_until_guest_us != 0 {
            return Err("stale stage armed a lease".to_string());
        }
        publish_scheduler(stream, ticket, &ack)?;
        println!("FERRUM_STALE_DROPPED");
        println!("FERRUM_SCHED_OK");
        return Ok(());
    }

    if late {
        let current = generation()?;
        if current != 1 {
            return Err(format!("boot generation was {current}"));
        }
        // The base generation is current. The acceptance window is already closed.
        stage(0, current)?;
        let read = |class: u8| -> Result<u64, String> {
            let mut service_us = 0u64;
            let rc = unsafe { sys_policy_read(class, &mut service_us) };
            if rc == 0 {
                Ok(service_us)
            } else {
                Err(format!("read class {class} returned {rc}"))
            }
        };
        let before = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
        unsafe { sys_usleep(1_000_000) };
        let after = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
        let sample = (
            after.0.saturating_sub(before.0),
            after.1.saturating_sub(before.1),
            after.2.saturating_sub(before.2),
        );
        println!(
            "FERRUM_SCHED window=balanced latency={} batch={} maintenance={}",
            sample.0, sample.1, sample.2
        );
        let sum = sample.0 + sample.1 + sample.2;
        let near_even = sum > 0
            && sample.0 > 0
            && sample.1 > 0
            && sample.2 > 0
            && sample.0 * 20 < sum * 9
            && sample.1 * 20 < sum * 9
            && sample.2 * 20 < sum * 9;
        if !near_even {
            return Err("late stage changed class service".to_string());
        }
        if unsafe { sys_policy_staged() } != 0 {
            return Err("scheduler left the late profile staged".to_string());
        }
        if generation()? != 1 {
            return Err("late stage advanced the generation".to_string());
        }
        let ack = require_ack(4, 2, 0, 1, 1)?;
        if ack.lease_until_guest_us != 0 {
            return Err("late stage armed a lease".to_string());
        }
        publish_scheduler(stream, ticket, &ack)?;
        println!("FERRUM_LATE_DROPPED");
        println!("FERRUM_SCHED_OK");
        return Ok(());
    }

    if stage_race {
        stage(ACCEPTANCE_DEADLINE_US, generation()?)?;
        let rc = unsafe { sys_policy_install_emergency() };
        if rc != 0 {
            return Err(format!("install emergency returned {rc}"));
        }
        if unsafe { sys_policy_staged() } != 0 {
            return Err("emergency left the staged profile in place".to_string());
        }
        println!(
            "FERRUM_EMERGENCY generation={}",
            generation()?
        );
        let ack = require_ack(3, 0, 3, 1, 2)?;
        if ack.lease_until_guest_us != 0 {
            return Err("emergency acknowledgment kept a profile lease".to_string());
        }
        publish_scheduler(stream, ticket, &ack)?;
        // Acceptance stays open for the whole measurement. The scheduler has
        // to drop this stage because the override is in force.
        stage(5_000_000, generation()?)?;
        let before = |class: u8| -> Result<u64, String> {
            let mut service_us = 0u64;
            let rc = unsafe { sys_policy_read(class, &mut service_us) };
            if rc == 0 {
                Ok(service_us)
            } else {
                Err(format!("read class {class} returned {rc}"))
            }
        };
        let read = before;
        let before = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
        unsafe { sys_usleep(1_000_000) };
        let after = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
        let held = (
            after.0.saturating_sub(before.0),
            after.1.saturating_sub(before.1),
            after.2.saturating_sub(before.2),
        );
        println!(
            "FERRUM_SCHED window=reclaim latency={} batch={} maintenance={}",
            held.0, held.1, held.2
        );
        if unsafe { sys_policy_staged() } != 0 {
            return Err("scheduler left the staged profile in place".to_string());
        }
        let ack = require_ack(4, 1, 3, 2, 2)?;
        if ack.lease_until_guest_us != 0 {
            return Err("dropped stage armed a lease".to_string());
        }
        println!("FERRUM_STAGE_DROPPED");
        let sum = held.0 + held.1 + held.2;
        let reclaim = sum > 0
            && held.0 > 0
            && held.1 > 0
            && held.2 > 0
            && held.2 > held.0
            && held.2 > held.1
            && held.2 * 20 > sum * 9;
        if !reclaim {
            return Err("staged profile replaced the emergency override".to_string());
        }
        println!("FERRUM_SCHED_OK");
        return Ok(());
    }

    let current = generation()?;
    if ticket.base_generation != current {
        return Err(format!(
            "proposal base {} is not the scheduler generation {current}",
            ticket.base_generation
        ));
    }
    stage(ACCEPTANCE_DEADLINE_US, ticket.base_generation)?;
    unsafe { sys_usleep(20_000) };
    let generation = generation()?;
    if generation < 2 {
        return Err(format!("scheduler did not activate the staged profile (generation {generation})"));
    }
    println!("FERRUM_ACTIVATED generation={generation}");
    let ack = require_ack(1, 0, 1, 1, 2)?;
    if ack.lease_until_guest_us <= ack.guest_us {
        return Err("applied acknowledgment has no lease".to_string());
    }
    publish_scheduler(stream, ticket, &ack)?;
    let peer = watch_controller(stream);

    let reclaim_share = |sample: (u64, u64, u64)| {
        let sum = sample.0 + sample.1 + sample.2;
        sum > 0
            && sample.0 > 0
            && sample.1 > 0
            && sample.2 > 0
            && sample.2 > sample.0
            && sample.2 > sample.1
            && sample.2 * 20 > sum * 9
    };
    let read = |class: u8| -> Result<u64, String> {
        let mut service_us = 0u64;
        let rc = unsafe { sys_policy_read(class, &mut service_us) };
        if rc == 0 {
            Ok(service_us)
        } else {
            Err(format!("read class {class} returned {rc}"))
        }
    };
    let before = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
    unsafe { sys_usleep(1_000_000) };
    let after = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
    let sample = (
        after.0.saturating_sub(before.0),
        after.1.saturating_sub(before.1),
        after.2.saturating_sub(before.2),
    );
    println!(
        "FERRUM_SCHED window={profile} latency={} batch={} maintenance={}",
        sample.0, sample.1, sample.2
    );
    note_controller(peer);

    let served = sample.0 > 0 && sample.1 > 0 && sample.2 > 0;
    let directed = match id {
        ProfileId::Balanced => true,
        ProfileId::Latency => sample.0 > sample.1 && sample.0 > sample.2,
        ProfileId::Throughput => sample.1 > sample.0 && sample.1 > sample.2,
        ProfileId::Reclaim => sample.2 > sample.0 && sample.2 > sample.1,
    };
    if !served || !directed {
        return Err("applied profile did not change class service".to_string());
    }
    if emergency {
        let rc = unsafe { sys_policy_install_emergency() };
        if rc != 0 {
            return Err(format!("install emergency returned {rc}"));
        }
        println!("FERRUM_EMERGENCY");
        let before = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
        unsafe { sys_usleep(1_000_000) };
        let after = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
        let installed = (
            after.0.saturating_sub(before.0),
            after.1.saturating_sub(before.1),
            after.2.saturating_sub(before.2),
        );
        println!(
            "FERRUM_SCHED window=reclaim latency={} batch={} maintenance={}",
            installed.0, installed.1, installed.2
        );
        if !reclaim_share(installed) {
            return Err("emergency did not install reclaim weights".to_string());
        }
        // The applied lease must not replace the override, including a direct
        // request for the balanced weights that expiry would otherwise install.
        let rc = unsafe { sys_policy_set_weights(1, 1, 1) };
        if rc == 0 {
            return Err("emergency accepted a weight change".to_string());
        }
        unsafe { sys_usleep(PROFILE_LEASE_US) };
        if unsafe { sys_policy_lease_expired() } != 1 {
            return Err("lease did not expire in the kernel".to_string());
        }
        println!("FERRUM_LEASE_EXPIRED");
        let before = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
        unsafe { sys_usleep(1_000_000) };
        let after = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
        let held = (
            after.0.saturating_sub(before.0),
            after.1.saturating_sub(before.1),
            after.2.saturating_sub(before.2),
        );
        println!(
            "FERRUM_SCHED window=reclaim latency={} batch={} maintenance={}",
            held.0, held.1, held.2
        );
        println!("FERRUM_EMERGENCY_HELD");
        if !reclaim_share(held) {
            return Err("expired lease replaced the emergency override".to_string());
        }
        println!("FERRUM_SCHED_OK");
        return Ok(());
    }
    if id == ProfileId::Balanced {
        println!("FERRUM_SCHED_OK");
        return Ok(());
    }

    // The kernel, not this thread, installs balanced weights when the lease ends.
    unsafe { sys_usleep(PROFILE_LEASE_US) };
    if unsafe { sys_policy_lease_expired() } != 1 {
        return Err("lease did not expire in the kernel".to_string());
    }
    let ack = require_ack(2, 0, 0, 2, 3)?;
    if ack.lease_until_guest_us != 0 {
        return Err("lease expiry acknowledgment still has a lease".to_string());
    }
    let before = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
    unsafe { sys_usleep(1_000_000) };
    let after = (read(LATENCY)?, read(BATCH)?, read(MAINTENANCE)?);
    let fallback = (
        after.0.saturating_sub(before.0),
        after.1.saturating_sub(before.1),
        after.2.saturating_sub(before.2),
    );
    println!(
        "FERRUM_SCHED window=balanced latency={} batch={} maintenance={}",
        fallback.0, fallback.1, fallback.2
    );
    println!("FERRUM_LEASE_EXPIRED");
    let sum = fallback.0 + fallback.1 + fallback.2;
    let near_even = sum > 0
        && fallback.0 > 0
        && fallback.1 > 0
        && fallback.2 > 0
        && fallback.0 * 20 < sum * 9
        && fallback.1 * 20 < sum * 9
        && fallback.2 * 20 < sum * 9;
    if !near_even {
        return Err("expired lease did not return service to balanced".to_string());
    }
    println!("FERRUM_SCHED_OK");
    Ok(())
}

#[cfg(not(target_os = "hermit"))]
fn actuate(
    _profile: &str,
    _emergency: bool,
    _stage_race: bool,
    _stale: bool,
    _late: bool,
) -> Result<(), String> {
    Ok(())
}

fn default_controller() -> String {
    if cfg!(target_os = "hermit") {
        "10.0.2.2:7777".to_string()
    } else {
        "127.0.0.1:7777".to_string()
    }
}
