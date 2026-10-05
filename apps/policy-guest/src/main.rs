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

fn default_controller() -> String {
    if cfg!(target_os = "hermit") {
        "10.0.2.2:7777".to_string()
    } else {
        "127.0.0.1:7777".to_string()
    }
}
