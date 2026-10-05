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
    if env::args().skip(1).any(|arg| arg == "--fair-demo") {
        return fair_demo_exit();
    }
    match run() {
        Ok(applied) => {
            println!(
                "FERRUM_APPLIED profile={} generation={}",
                applied.profile, applied.generation
            );
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
    exchange(&mut stream, boot, 3_000_000).map_err(|err| err.to_string())
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

    let rc = unsafe { sys_policy_set_weights(1, 1, 1) };
    if rc != 0 {
        return Err(format!("balanced weights returned {rc}"));
    }
    let before = snapshot()?;
    unsafe { sys_usleep(1_000_000) };
    let after = snapshot()?;
    let balanced = (
        after.0.saturating_sub(before.0),
        after.1.saturating_sub(before.1),
        after.2.saturating_sub(before.2),
    );
    println!(
        "FERRUM_SCHED window=balanced latency={} batch={} maintenance={}",
        balanced.0, balanced.1, balanced.2
    );

    let rc = unsafe { sys_policy_set_weights(6, 2, 2) };
    if rc != 0 {
        return Err(format!("latency weights returned {rc}"));
    }
    let before = snapshot()?;
    unsafe { sys_usleep(1_000_000) };
    let after = snapshot()?;
    let latency = (
        after.0.saturating_sub(before.0),
        after.1.saturating_sub(before.1),
        after.2.saturating_sub(before.2),
    );
    println!(
        "FERRUM_SCHED window=latency latency={} batch={} maintenance={}",
        latency.0, latency.1, latency.2
    );

    let passed = balanced.0 > 0
        && balanced.1 > 0
        && balanced.2 > 0
        && latency.0 > 0
        && latency.1 > 0
        && latency.2 > 0
        && latency.0 > latency.1
        && latency.0 > latency.2;
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

fn default_controller() -> String {
    if cfg!(target_os = "hermit") {
        "10.0.2.2:7777".to_string()
    } else {
        "127.0.0.1:7777".to_string()
    }
}
