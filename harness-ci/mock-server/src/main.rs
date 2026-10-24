//! `mock-server --scenario <file> [--addr 127.0.0.1:0] [--log <file>]`
//!
//! Prints `MOCK_ADDR=<host:port>` once it is listening, then serves until killed.

use mock_server::{MockState, router, scenario::Scenario};
use std::io::Write;
use std::path::PathBuf;

struct Args {
    scenario: PathBuf,
    addr: String,
    log: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut scenario = None;
    let mut addr = "127.0.0.1:0".to_string();
    let mut log = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--scenario" | "-s" => scenario = Some(PathBuf::from(args.next().ok_or("--scenario needs a path")?)),
            "--addr" => addr = args.next().ok_or("--addr needs host:port")?,
            "--log" => log = Some(PathBuf::from(args.next().ok_or("--log needs a path")?)),
            "--help" | "-h" => {
                println!("usage: mock-server --scenario <file> [--addr 127.0.0.1:0] [--log <file>]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Args { scenario: scenario.ok_or("--scenario is required")?, addr, log })
}

#[tokio::main]
async fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("mock-server: {error}");
            std::process::exit(2);
        }
    };
    let scenario = match Scenario::from_path(&args.scenario) {
        Ok(scenario) => scenario,
        Err(error) => {
            eprintln!("mock-server: {error}");
            std::process::exit(2);
        }
    };
    let mut state = MockState::new(scenario);
    if let Some(log) = args.log {
        state = state.with_log(log);
    }
    let listener = match tokio::net::TcpListener::bind(&args.addr).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("mock-server: bind {}: {error}", args.addr);
            std::process::exit(1);
        }
    };
    let local = listener.local_addr().expect("bound address");
    println!("MOCK_ADDR={local}");
    println!("mock-server listening on http://{local} (scenario: {})", state.scenario.name);
    let _ = std::io::stdout().flush();
    if let Err(error) = axum::serve(listener, router(state)).await {
        eprintln!("mock-server: {error}");
        std::process::exit(1);
    }
}
