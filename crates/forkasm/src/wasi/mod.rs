//! Our own `wasi_snapshot_preview1` (SPEC §5.4).
//!
//! All sandbox-visible state lives in [`WasiState`], which is plain data and
//! cloned on fork: the filesystem ([`Vfs`]) and fd table included. Clock and
//! RNG are deterministic unless real time is asked for. Calls we don't
//! implement return `ENOSYS` and are logged once.

mod fs;
mod output;
mod rng;

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime};

use wasmtime::{Caller, Extern, Linker, Module, Val, ValType};

pub use fs::FdTable;
pub use output::Output;
pub use rng::Rng;

use crate::vfs::Vfs;

pub(crate) const MODULE: &str = "wasi_snapshot_preview1";

/// WASI errno values.
mod errno {
    pub const SUCCESS: i32 = 0;
    pub const BADF: i32 = 8;
    pub const EXIST: i32 = 20;
    pub const FAULT: i32 = 21;
    pub const INVAL: i32 = 28;
    pub const ISDIR: i32 = 31;
    pub const NOENT: i32 = 44;
    pub const NOSYS: i32 = 52;
    pub const NOTDIR: i32 = 54;
    pub const NOTEMPTY: i32 = 55;
    pub const ROFS: i32 = 69;
    pub const SPIPE: i32 = 70;
    pub const NOTCAPABLE: i32 = 76;
}

/// The guest called `proc_exit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("guest exited with code {0}")]
pub struct Exit(pub i32);

/// Virtual time: every clock read advances it by `tick_ns`.
#[derive(Clone, Debug)]
pub struct Clock {
    /// Virtual time elapsed so far, in ns. Realtime reads add it to
    /// 2026-01-01T00:00:00Z.
    pub now_ns: u64,
    /// How far each clock read advances `now_ns`.
    pub tick_ns: u64,
    /// Read the host clock instead (non-deterministic, opt-in).
    pub real: bool,
}

/// Start of virtual realtime: 2026-01-01T00:00:00Z.
const EPOCH_NS: u64 = 1_767_225_600 * 1_000_000_000;

impl Default for Clock {
    fn default() -> Self {
        Clock { now_ns: 0, tick_ns: 1_000, real: false }
    }
}

impl Clock {
    fn read(&mut self, realtime: bool) -> u64 {
        if self.real {
            static START: OnceLock<Instant> = OnceLock::new();
            return if realtime {
                let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH);
                now.unwrap_or_default().as_nanos() as u64
            } else {
                START.get_or_init(Instant::now).elapsed().as_nanos() as u64
            };
        }
        self.now_ns += self.tick_ns;
        if realtime { EPOCH_NS + self.now_ns } else { self.now_ns }
    }

    /// Current realtime without advancing virtual time (file timestamps).
    pub(crate) fn peek_realtime(&self) -> u64 {
        if self.real {
            let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH);
            return now.unwrap_or_default().as_nanos() as u64;
        }
        EPOCH_NS + self.now_ns
    }
}

/// Everything a guest can observe through WASI. Cloned on fork.
#[derive(Clone, Debug, Default)]
pub struct WasiState {
    /// Command-line arguments.
    pub args: Arc<[String]>,
    /// Environment variables.
    pub env: Arc<[(String, String)]>,
    /// The clock behind `clock_time_get`.
    pub clock: Clock,
    /// The random stream behind `random_get`.
    pub rng: Rng,
    /// Everything written to fd 1.
    pub stdout: Output,
    /// Everything written to fd 2.
    pub stderr: Output,
    /// The code passed to `proc_exit`, once the guest has called it.
    pub exit: Option<i32>,
    /// The filesystem.
    pub vfs: Vfs,
    /// Open file descriptors.
    pub fds: FdTable,
}

const IMPLEMENTED: &[&str] = &[
    "args_sizes_get",
    "args_get",
    "environ_sizes_get",
    "environ_get",
    "clock_time_get",
    "random_get",
    "proc_exit",
    "sched_yield",
];

/// Adds our preview1 functions, plus an `ENOSYS` stub for every other
/// preview1 import of `module`, to `linker`.
pub(crate) fn add_to_linker(linker: &mut Linker<WasiState>, module: &Module) -> anyhow::Result<()> {
    linker.func_wrap(MODULE, "args_sizes_get", |mut c: Caller<'_, WasiState>, argc: i32, size: i32| {
        let args = c.data().args.clone();
        let total: usize = args.iter().map(|a| a.len() + 1).sum();
        with_mem(&mut c, |m, _| {
            m.write_u32(argc, args.len() as u32)?;
            m.write_u32(size, total as u32)
        })
    })?;
    linker.func_wrap(MODULE, "args_get", |mut c: Caller<'_, WasiState>, argv: i32, buf: i32| {
        let args = c.data().args.clone();
        with_mem(&mut c, |m, _| m.write_strings(argv, buf, args.iter().map(|a| a.as_bytes().to_vec())))
    })?;
    linker.func_wrap(MODULE, "environ_sizes_get", |mut c: Caller<'_, WasiState>, n: i32, size: i32| {
        let env = c.data().env.clone();
        let total: usize = env.iter().map(|(k, v)| k.len() + v.len() + 2).sum();
        with_mem(&mut c, |m, _| {
            m.write_u32(n, env.len() as u32)?;
            m.write_u32(size, total as u32)
        })
    })?;
    linker.func_wrap(MODULE, "environ_get", |mut c: Caller<'_, WasiState>, ptrs: i32, buf: i32| {
        let env = c.data().env.clone();
        let entries = env.iter().map(|(k, v)| format!("{k}={v}").into_bytes());
        with_mem(&mut c, |m, _| m.write_strings(ptrs, buf, entries))
    })?;
    linker.func_wrap(
        MODULE,
        "clock_time_get",
        |mut c: Caller<'_, WasiState>, id: i32, _precision: i64, out: i32| {
            // 0 = realtime, 1 = monotonic; process/thread CPU time read as monotonic.
            if !(0..=3).contains(&id) {
                return Ok(errno::INVAL);
            }
            with_mem(&mut c, |m, s| m.write_u64(out, s.clock.read(id == 0)))
        },
    )?;
    linker.func_wrap(MODULE, "random_get", |mut c: Caller<'_, WasiState>, buf: i32, len: i32| {
        with_mem(&mut c, |m, s| {
            let dst = m.slice_mut(buf, len as u32)?;
            s.rng.fill(dst);
            Ok(errno::SUCCESS)
        })
    })?;
    linker.func_wrap(MODULE, "sched_yield", |_: Caller<'_, WasiState>| Ok(errno::SUCCESS))?;
    fs::add_to_linker(linker)?;
    linker.func_wrap(
        MODULE,
        "proc_exit",
        |mut c: Caller<'_, WasiState>, code: i32| -> wasmtime::Result<()> {
            c.data_mut().exit = Some(code);
            Err(wasmtime::Error::new(Exit(code)))
        },
    )?;

    for import in module.imports() {
        if import.module() != MODULE
            || IMPLEMENTED.contains(&import.name())
            || fs::IMPLEMENTED.contains(&import.name())
        {
            continue;
        }
        let Some(ty) = import.ty().func().cloned() else { continue };
        let name = import.name().to_string();
        let returns_errno = ty.results().len() == 1 && matches!(ty.results().next(), Some(ValType::I32));
        linker.func_new(MODULE, &name.clone(), ty, move |_, _, results| {
            log_once(&name);
            if returns_errno {
                results[0] = Val::I32(errno::NOSYS);
                Ok(())
            } else {
                wasmtime::bail!("unimplemented WASI call {name}")
            }
        })?;
    }
    Ok(())
}

fn log_once(name: &str) {
    static SEEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.get_or_insert_default().insert(name.to_string()) {
        eprintln!("forkasm: unimplemented WASI call {MODULE}::{name} -> ENOSYS");
    }
}

/// Bounds-checked access to guest memory; out-of-bounds becomes `EFAULT`.
struct GuestMem<'a>(&'a mut [u8]);

/// Errors inside a WASI call: an errno for the guest.
type Errno = i32;

impl GuestMem<'_> {
    fn slice_mut(&mut self, ptr: i32, len: u32) -> Result<&mut [u8], Errno> {
        let start = ptr as u32 as usize;
        let end = start.checked_add(len as usize).ok_or(errno::FAULT)?;
        self.0.get_mut(start..end).ok_or(errno::FAULT)
    }

    fn read_u32(&mut self, ptr: i32) -> Result<u32, Errno> {
        Ok(u32::from_le_bytes(self.slice_mut(ptr, 4)?.try_into().unwrap()))
    }

    fn write_u32(&mut self, ptr: i32, v: u32) -> Result<Errno, Errno> {
        self.slice_mut(ptr, 4)?.copy_from_slice(&v.to_le_bytes());
        Ok(errno::SUCCESS)
    }

    /// The `(ptr, len)` pairs of an iovec array.
    fn iovecs(&mut self, iovs: i32, n: i32) -> Result<Vec<(u32, u32)>, Errno> {
        (0..n as u32)
            .map(|i| {
                let iov = iovs.wrapping_add((i * 8) as i32);
                Ok((self.read_u32(iov)?, self.read_u32(iov.wrapping_add(4))?))
            })
            .collect()
    }

    /// The bytes an iovec array points to, concatenated.
    fn gather(&mut self, iovs: i32, n: i32) -> Result<Vec<u8>, Errno> {
        let mut data = Vec::new();
        for (ptr, len) in self.iovecs(iovs, n)? {
            data.extend_from_slice(self.slice_mut(ptr as i32, len)?);
        }
        Ok(data)
    }

    fn read_str(&mut self, ptr: i32, len: i32) -> Result<String, Errno> {
        let bytes = self.slice_mut(ptr, len as u32)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| errno::INVAL)
    }

    fn write_u64(&mut self, ptr: i32, v: u64) -> Result<Errno, Errno> {
        self.slice_mut(ptr, 8)?.copy_from_slice(&v.to_le_bytes());
        Ok(errno::SUCCESS)
    }

    /// Writes NUL-terminated `strings` to `buf` and their addresses to `ptrs`.
    fn write_strings(
        &mut self,
        ptrs: i32,
        buf: i32,
        strings: impl Iterator<Item = Vec<u8>>,
    ) -> Result<Errno, Errno> {
        let mut at = buf as u32;
        for (i, mut s) in strings.enumerate() {
            s.push(0);
            self.write_u32(ptrs.wrapping_add(4 * i as i32), at)?;
            self.slice_mut(at as i32, s.len() as u32)?.copy_from_slice(&s);
            at += s.len() as u32;
        }
        Ok(errno::SUCCESS)
    }
}

/// Runs `f` with the guest's exported memory and the WASI state.
fn with_mem(
    caller: &mut Caller<'_, WasiState>,
    f: impl FnOnce(&mut GuestMem<'_>, &mut WasiState) -> Result<Errno, Errno>,
) -> wasmtime::Result<i32> {
    let Some(Extern::Memory(mem)) = caller.get_export("memory") else {
        wasmtime::bail!("guest exports no memory");
    };
    let (bytes, state) = mem.data_and_store_mut(caller);
    Ok(f(&mut GuestMem(bytes), state).unwrap_or_else(|e| e))
}
