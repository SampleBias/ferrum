//! Hermit guest that asks the host mock controller for one CPU profile.

#[cfg(target_os = "hermit")]
use hermit as _;

use std::env;
use std::net::TcpStream;
use std::process::ExitCode;

use policy_guest::exchange;
use policy_types::BootId;

fn main() -> ExitCode {
    println!("FERRUM_START policy-guest");
    if env::args().skip(1).any(|arg| arg == "--fair-progress") {
        return fair_progress_exit();
    }
    if env::args().skip(1).any(|arg| arg == "--fair-demo") {
        return fair_demo_exit();
    }
    match run() {
        Ok(applied) => {
            println!(
                "FERRUM_APPLIED profile={} generation={}",
                applied.profile, applied.generation
            );
            if let Err(err) = actuate(&applied.profile) {
                println!("FERRUM_FAIL {err}");
                return ExitCode::from(1);
            }
            println!("FERRUM_COMPLETE");
            ExitCode::SUCCESS
        }
        Err(err) => {
            println!("FERRUM_FAIL {err}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<policy_guest::Applied, String> {
    let mut controller = default_controller();
    let mut boot = BootId::from_hex("00112233445566778899aabbccddeeff").expect("boot id");
    for arg in env::args().skip(1) {
        if let Some(value) = arg.strip_prefix("--controller=") {
            controller = value.to_string();
        } else if let Some(value) = arg.strip_prefix("--boot-id=") {
            boot = BootId::from_hex(value).map_err(|_| format!("bad boot id"))?;
        } else {
            return Err(format!("unknown argument {arg}"));
        }
    }
    let mut stream = TcpStream::connect(&controller).map_err(|err| format!("connect {controller}: {err}"))?;
    let applied = exchange(&mut stream, boot, 3_000_000).map_err(|err| err.to_string())?;
    // Hermit's TCP close waits until the socket is inactive. The mock
    // controller has already exited, so that wait does not finish.
    std::mem::forget(stream);
    Ok(applied)
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
fn actuate(profile: &str) -> Result<(), String> {
    use std::hint::spin_loop;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::thread;

    use policy_core::catalog_spec;
    use policy_types::{CatalogId, ProfileId};

    const LATENCY: u8 = 1;
    const BATCH: u8 = 2;
    const MAINTENANCE: u8 = 3;

    unsafe extern "C" {
        fn sys_policy_register(class: u8) -> i32;
        fn sys_policy_set_weights(latency: u32, batch: u32, maintenance: u32) -> i32;
        fn sys_policy_read(class: u8, service_us: *mut u64) -> i32;
        fn sys_usleep(usecs: u64);
    }

    let id = ProfileId::parse(profile).ok_or_else(|| format!("unknown profile {profile}"))?;
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

    let rc = unsafe { sys_policy_set_weights(weights[0], weights[1], weights[2]) };
    if rc != 0 {
        return Err(format!("weights returned {rc}"));
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
        "FERRUM_SCHED window={profile} latency={} batch={} maintenance={}",
        sample.0, sample.1, sample.2
    );

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
    println!("FERRUM_SCHED_OK");
    Ok(())
}

#[cfg(not(target_os = "hermit"))]
fn actuate(_profile: &str) -> Result<(), String> {
    Ok(())
}

fn default_controller() -> String {
    if cfg!(target_os = "hermit") {
        "10.0.2.2:7777".to_string()
    } else {
        "127.0.0.1:7777".to_string()
    }
}
