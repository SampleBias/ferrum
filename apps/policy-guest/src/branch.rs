//! `--branch`: one branched trial of a `jobs-v1` unit.
//!
//! The guest runs the unit's prefix under balanced, prints the state it had
//! reached at the decision point, forces one `cpu-v1` profile for the horizon,
//! and prints the outcome. Each branch is a fresh boot on the same schedule, so
//! branches of one unit share a prefix up to scheduling noise, which the state
//! line exposes for the comparability check.
//!
//! Job and unit sizes are fixed in guest CPU time. Host speed differs by about
//! 5% from one boot to the next, so each boot sizes its work from a warmed
//! calibration of its own. With a fixed spin count instead, batch counts of two
//! equal-weight branches differed by more than the 95% retention margin. The
//! outcome line reports the in-trial speed so a boot whose host speed moved
//! during the trial can be refused. `--spins-per-ms` overrides the calibration.

use std::env;
use std::process::ExitCode;

use policy_types::ProfileId;
use workloads::jobs::{self, JobFamily, JobScenario};

pub fn exit() -> ExitCode {
    match run() {
        Ok(()) => {
            println!("FERRUM_BRANCH_OK");
            ExitCode::SUCCESS
        }
        Err(err) => {
            println!("FERRUM_BRANCH_FAIL {err}");
            ExitCode::from(1)
        }
    }
}

struct Request {
    family: JobFamily,
    scenario: JobScenario,
    seed: u64,
    profile: ProfileId,
    spins_per_ms: Option<u64>,
}

fn calibrate_only() -> bool {
    env::args().skip(1).any(|arg| arg == "--calibrate")
}

fn request() -> Result<Request, String> {
    let mut family = None;
    let mut scenario = None;
    let mut seed = None;
    let mut profile = None;
    let mut spins_per_ms = None;
    for arg in env::args().skip(1) {
        if let Some(value) = arg.strip_prefix("--family=") {
            family = Some(jobs::family(value).ok_or(format!("unknown family {value}"))?);
        } else if let Some(value) = arg.strip_prefix("--scenario=") {
            scenario = Some(value.to_string());
        } else if let Some(value) = arg.strip_prefix("--seed=") {
            seed = Some(value.parse::<u64>().map_err(|_| format!("bad seed {value}"))?);
        } else if let Some(value) = arg.strip_prefix("--profile=") {
            profile = Some(ProfileId::parse(value).ok_or(format!("unknown profile {value}"))?);
        } else if let Some(value) = arg.strip_prefix("--spins-per-ms=") {
            spins_per_ms = Some(
                value
                    .parse::<u64>()
                    .ok()
                    .filter(|rate| *rate > 0)
                    .ok_or(format!("bad spin rate {value}"))?,
            );
        }
    }
    let family = family.ok_or("--family is required")?;
    let name = scenario.ok_or("--scenario is required")?;
    Ok(Request {
        family,
        scenario: family
            .scenario(&name)
            .ok_or(format!("{} has no scenario {name}", family.id))?,
        seed: seed.ok_or("--seed is required")?,
        profile: profile.ok_or("--profile is required")?,
        spins_per_ms,
    })
}

#[cfg(target_os = "hermit")]
fn percentile(value: Option<u64>) -> String {
    value.map_or_else(|| "censored".to_string(), |us| us.to_string())
}

#[cfg(not(target_os = "hermit"))]
fn run() -> Result<(), String> {
    if calibrate_only() {
        return Err("calibration runs inside the hermit guest".to_string());
    }
    let request = request()?;
    Err(format!(
        "branch {} {}/{} under {} ({:?} spins/ms) runs inside the hermit guest",
        request.family.id,
        request.scenario.name,
        request.seed,
        request.profile.as_str(),
        request.spins_per_ms
    ))
}

#[cfg(target_os = "hermit")]
fn run() -> Result<(), String> {
    use std::hint::spin_loop;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
    use std::thread;

    use policy_core::catalog_spec;
    use policy_types::CatalogId;

    const LATENCY: u8 = 1;
    const BATCH: u8 = 2;
    const MAINTENANCE: u8 = 3;
    const SYSTEM: u8 = 4;

    unsafe extern "C" {
        fn sys_policy_register(class: u8) -> i32;
        fn sys_policy_set_weights(latency: u32, batch: u32, maintenance: u32) -> i32;
        fn sys_policy_read(class: u8, service_us: *mut u64) -> i32;
        fn sys_clock_gettime(clock_id: i32, tp: *mut Timespec) -> i32;
        fn sys_usleep(usecs: u64);
        fn sys_block_current_task_with_timeout(timeout_ms: u64);
        fn sys_yield();
    }

    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i32,
    }

    static READY: AtomicU8 = AtomicU8::new(0);
    static FAILED: AtomicBool = AtomicBool::new(false);
    static CLOCK_FAILED: AtomicBool = AtomicBool::new(false);

    struct Trial {
        arrivals: Vec<u64>,
        done: Vec<AtomicU64>,
        next: AtomicUsize,
        start_us: AtomicU64,
        units: AtomicU64,
        job_spins: u64,
        unit_spins: u64,
    }

    fn now_us() -> u64 {
        let mut time = Timespec { tv_sec: 0, tv_nsec: 0 };
        // CLOCK_MONOTONIC is the scheduler's guest timer, in microseconds.
        if unsafe { sys_clock_gettime(4, &mut time) } != 0 {
            CLOCK_FAILED.store(true, Ordering::Release);
            return 0;
        }
        (time.tv_sec as u64)
            .saturating_mul(1_000_000)
            .saturating_add((time.tv_nsec / 1000) as u64)
    }

    /// `sys_usleep` below 10 ms busy-waits inside the caller's class, which
    /// would charge an idle job worker as latency demand. Short waits block for
    /// whole milliseconds instead, so a job can start up to 1 ms late.
    fn sleep_until(target: u64) {
        loop {
            let now = now_us();
            if now >= target || CLOCK_FAILED.load(Ordering::Acquire) {
                return;
            }
            let rest = target - now;
            if rest >= 10_000 {
                unsafe { sys_usleep(rest) };
            } else {
                unsafe {
                    sys_block_current_task_with_timeout(rest.div_ceil(1_000));
                    sys_yield();
                }
            }
        }
    }

    fn spin(count: u64) {
        for _ in 0..count {
            spin_loop();
        }
    }

    /// Spins per millisecond on the guest before any worker exists. The host
    /// is still raising its clock when the VM starts, so a 300 ms warm-up runs
    /// first. Median of five timed runs of at least 100 ms each.
    fn calibrate() -> Result<u64, String> {
        let warm = now_us();
        while now_us().saturating_sub(warm) < 300_000 {
            spin(100_000);
        }
        let mut count = 50_000u64;
        loop {
            let begin = now_us();
            spin(count);
            if now_us().saturating_sub(begin) >= 100_000 {
                break;
            }
            count = count.checked_mul(2).ok_or("spin calibration overflowed")?;
        }
        let mut rates = [0u64; 5];
        for rate in &mut rates {
            let begin = now_us();
            spin(count);
            let took = now_us().saturating_sub(begin).max(1);
            *rate = count.saturating_mul(1_000) / took;
        }
        rates.sort_unstable();
        Ok(rates[2])
    }

    fn wait_start(trial: &Trial) -> u64 {
        loop {
            let start = trial.start_us.load(Ordering::Acquire);
            if start != 0 {
                sleep_until(start);
                return start;
            }
            unsafe { sys_usleep(10_000) };
        }
    }

    fn read(class: u8) -> Result<u64, String> {
        let mut service_us = 0u64;
        let rc = unsafe { sys_policy_read(class, &mut service_us) };
        if rc == 0 {
            Ok(service_us)
        } else {
            Err(format!("read class {class} returned {rc}"))
        }
    }

    fn sample(trial: &Trial) -> Result<[u64; 5], String> {
        Ok([
            read(LATENCY)?,
            read(BATCH)?,
            read(MAINTENANCE)?,
            read(SYSTEM)?,
            trial.units.load(Ordering::Acquire),
        ])
    }

    fn completions(trial: &Trial) -> Vec<u64> {
        trial.done.iter().map(|done| done.load(Ordering::Acquire)).collect()
    }

    if calibrate_only() {
        println!("FERRUM_BRANCH_CALIBRATION spins_per_ms={}", calibrate()?);
        return Ok(());
    }
    let request = request()?;
    let arrivals = request.scenario.schedule(request.seed);
    let digest = jobs::schedule_digest(&arrivals);
    let measured_spins_per_ms = calibrate()?;
    let spins_per_ms = request.spins_per_ms.unwrap_or(measured_spins_per_ms);
    let trial = Arc::new(Trial {
        done: arrivals.iter().map(|_| AtomicU64::new(0)).collect(),
        arrivals,
        next: AtomicUsize::new(0),
        start_us: AtomicU64::new(0),
        units: AtomicU64::new(0),
        job_spins: request.scenario.job_us * spins_per_ms / 1_000,
        unit_spins: jobs::BATCH_UNIT_US * spins_per_ms / 1_000,
    });
    println!(
        "FERRUM_BRANCH_BEGIN family={} scenario={} seed={} profile={} jobs={} schedule_fnv64={:#018x} spins_per_ms={} measured_spins_per_ms={}",
        request.family.id,
        request.scenario.name,
        request.seed,
        request.profile.as_str(),
        trial.arrivals.len(),
        digest,
        spins_per_ms,
        measured_spins_per_ms
    );

    for _ in 0..workloads::LATENCY_WORKERS {
        let trial = Arc::clone(&trial);
        thread::spawn(move || {
            if unsafe { sys_policy_register(LATENCY) } != 0 {
                FAILED.store(true, Ordering::Release);
                return;
            }
            READY.fetch_add(1, Ordering::Release);
            let start = wait_start(&trial);
            loop {
                let index = trial.next.fetch_add(1, Ordering::AcqRel);
                let Some(arrival) = trial.arrivals.get(index).copied() else {
                    loop {
                        unsafe { sys_usleep(1_000_000) };
                    }
                };
                sleep_until(start + arrival);
                spin(trial.job_spins);
                let finished = now_us().saturating_sub(start).max(1);
                trial.done[index].store(finished, Ordering::Release);
            }
        });
    }
    for _ in 0..workloads::BATCH_WORKERS {
        let trial = Arc::clone(&trial);
        thread::spawn(move || {
            if unsafe { sys_policy_register(BATCH) } != 0 {
                FAILED.store(true, Ordering::Release);
                return;
            }
            READY.fetch_add(1, Ordering::Release);
            wait_start(&trial);
            loop {
                spin(trial.unit_spins);
                trial.units.fetch_add(1, Ordering::AcqRel);
            }
        });
    }
    let expected = workloads::LATENCY_WORKERS + workloads::BATCH_WORKERS;
    for _ in 0..80 {
        if READY.load(Ordering::Acquire) == expected || FAILED.load(Ordering::Acquire) {
            break;
        }
        unsafe { sys_usleep(50_000) };
    }
    if FAILED.load(Ordering::Acquire) || READY.load(Ordering::Acquire) != expected {
        return Err(format!(
            "workers did not register ({}/{expected})",
            READY.load(Ordering::Acquire)
        ));
    }

    let catalog = catalog_spec(CatalogId::CpuV1);
    let balanced = catalog.profile(ProfileId::Balanced).weights();
    let rc = unsafe { sys_policy_set_weights(balanced[0], balanced[1], balanced[2]) };
    if rc != 0 {
        return Err(format!("balanced weights returned {rc}"));
    }
    let start = now_us() + 20_000;
    trial.start_us.store(start, Ordering::Release);

    sleep_until(start + jobs::PREFIX_US - jobs::STATE_WINDOW_US);
    let window_open = sample(&trial)?;
    sleep_until(start + jobs::PREFIX_US);
    let window_close = sample(&trial)?;
    let decision_us = now_us().saturating_sub(start);
    let seen = completions(&trial);
    let forced = catalog.profile(request.profile).weights();
    let rc = unsafe { sys_policy_set_weights(forced[0], forced[1], forced[2]) };
    if rc != 0 {
        return Err(format!("{} weights returned {rc}", request.profile.as_str()));
    }
    let state = jobs::prefix_jobs(&trial.arrivals, &seen, decision_us);
    let delta = |index: usize| window_close[index].saturating_sub(window_open[index]);
    let obs = jobs::pre_decision(state, [delta(0), delta(1), delta(2), delta(3)], delta(4));
    let [latency, batch, maintenance, system] = obs.groups;
    println!(
        "FERRUM_BRANCH_STATE window_us={} decision_late_us={} latency_queue={} latency_runnable={} latency_service_us={} latency_wait_samples={} latency_max_wait_us={} latency_completions={} batch_queue={} batch_runnable={} batch_service_us={} batch_completions={} maintenance_service_us={} system_service_us={}",
        obs.window_us,
        decision_us.saturating_sub(jobs::PREFIX_US),
        latency.queue_len,
        latency.runnable,
        latency.cpu_service_us,
        latency.wait_samples,
        latency.max_wait_us,
        latency.completions,
        batch.queue_len,
        batch.runnable,
        batch.cpu_service_us,
        batch.completions,
        maintenance.cpu_service_us,
        system.cpu_service_us
    );

    sleep_until(start + jobs::PREFIX_US + jobs::HORIZON_US);
    let horizon_close = sample(&trial)?;
    sleep_until(start + jobs::SCHEDULE_US);
    let finished = completions(&trial);
    if CLOCK_FAILED.load(Ordering::Acquire) {
        return Err("guest clock read failed".to_string());
    }
    let outcome = jobs::outcome(
        &trial.arrivals,
        &finished,
        jobs::PREFIX_US,
        jobs::PREFIX_US + jobs::HORIZON_US,
        jobs::SCHEDULE_US,
    );
    let span = |index: usize| horizon_close[index].saturating_sub(window_close[index]);
    let batch_spins_per_ms =
        span(4).saturating_mul(trial.unit_spins).saturating_mul(1_000) / span(1).max(1);
    println!(
        "FERRUM_BRANCH_OUTCOME scenario={} seed={} profile={} offered={} completed={} completion_bp={} p50_us={} p90_us={} p95_us={} p99_us={} batch_units={} latency_service_us={} batch_service_us={} batch_spins_per_ms={}",
        request.scenario.name,
        request.seed,
        request.profile.as_str(),
        outcome.offered,
        outcome.completed,
        outcome.completion_bp,
        percentile(outcome.p50_us),
        percentile(outcome.p90_us),
        percentile(outcome.p95_us),
        percentile(outcome.p99_us),
        span(4),
        span(0),
        span(1),
        batch_spins_per_ms
    );
    Ok(())
}
