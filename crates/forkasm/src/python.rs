//! Helpers for G2, the CPython guest (`guests/py`, SPEC §6).
//!
//! G2 is a WASI reactor exporting `py_init`, `py_exec(ptr, len)`,
//! `py_get_int(ptr, len)` and `py_alloc`/`py_free`. Its stdlib is mounted
//! read-only at [`PY_LIB`].

use std::path::PathBuf;

use anyhow::{Result, bail};
use wasmtime::Val;

use crate::{GuestModule, Runtime, Sandbox, SpawnOpts};

/// Where G2 expects its stdlib (`python312.zip` plus the `os.py` landmark).
pub const PY_LIB: &str = "/usr/local/lib";

/// Spawn options mounting the stdlib directory `lib` (`guests/py/out/lib`).
pub fn spawn_opts(lib: impl Into<PathBuf>) -> SpawnOpts {
    SpawnOpts { mounts: vec![(PY_LIB.into(), lib.into())], ..SpawnOpts::default() }
}

/// Spawns a root with the stdlib from `lib` mounted and the interpreter
/// initialised.
pub fn spawn(rt: &Runtime, module: &GuestModule, lib: impl Into<PathBuf>) -> Result<Sandbox> {
    let mut sb = rt.spawn(module, spawn_opts(lib))?;
    if sb.call_u64("py_init", &[])? != 0 {
        bail!("py_init failed: {}", String::from_utf8_lossy(&sb.stderr()));
    }
    Ok(sb)
}

/// Runs Python source in `__main__`. If it raises, the error carries the
/// stderr written by that call (the traceback).
pub fn exec(sb: &mut Sandbox, src: &str) -> Result<()> {
    let stderr_before = sb.wasi().stderr.len();
    if with_bytes(sb, "py_exec", src.as_bytes())? != 0 {
        bail!("{}", String::from_utf8_lossy(&sb.wasi().stderr.bytes_from(stderr_before)).trim_end());
    }
    Ok(())
}

/// Reads the int variable `name` from `__main__`; `None` if it is missing or
/// not an int that fits in an `i64`.
pub fn get_int(sb: &mut Sandbox, name: &str) -> Result<Option<i64>> {
    let v = with_bytes(sb, "py_get_int", name.as_bytes())? as i64;
    Ok((v != i64::MIN).then_some(v))
}

/// Copies `bytes` into a fresh guest allocation, calls `func(ptr, len)`, and
/// frees the allocation.
fn with_bytes(sb: &mut Sandbox, func: &str, bytes: &[u8]) -> Result<u64> {
    let len = Val::I32(bytes.len() as i32);
    let ptr = sb.call_u64("py_alloc", std::slice::from_ref(&len))?;
    sb.write_memory(ptr as usize, bytes)?;
    let ptr = Val::I32(ptr as i32);
    let r = sb.call_u64(func, &[ptr, len])?;
    sb.call("py_free", &[ptr])?;
    Ok(r)
}
