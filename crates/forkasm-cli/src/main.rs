//! forkasm demo and benchmark harness.

use anyhow::{Result, bail};

fn main() -> Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    match cmd.as_str() {
        "envcheck" => envcheck(),
        "rewrite" => inspect(&std::env::args().nth(2).unwrap_or_default()),
        _ => bail!("usage: forkasm-cli envcheck | rewrite <module.wasm>"),
    }
}

fn envcheck() -> Result<()> {
    let probes = forkasm::mem::probe::run_all();
    for p in &probes {
        println!("{p}");
    }
    Ok(())
}

/// Prints what the rewrite pass does to a module.
fn inspect(path: &str) -> Result<()> {
    let r = forkasm::rewrite::rewrite(&std::fs::read(path)?)?;
    let data: usize = r.image.segments.iter().map(|(_, b)| b.len()).sum();
    println!("memory:   {} bytes initial", r.image.min_bytes);
    println!("data:     {} segments, {data} bytes", r.image.segments.len());
    println!("globals:  {:?}", r.globals);
    println!("start:    {}", r.has_start);
    Ok(())
}
