//! forkasm demo and benchmark harness.

use anyhow::{Result, bail};

fn main() -> Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    match cmd.as_str() {
        "envcheck" => envcheck(),
        _ => bail!("usage: forkasm-cli envcheck"),
    }
}

fn envcheck() -> Result<()> {
    let probes = forkasm::mem::probe::run_all();
    for p in &probes {
        println!("{p}");
    }
    Ok(())
}
