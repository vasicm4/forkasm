//! Baseline B (SPEC §5.3): forking the whole host process at a quiescent
//! point, for comparison with sandbox forks. Benchmark-only.

use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

/// Timings of one process fork, all measured in the parent from just before
/// `fork()`.
#[derive(Clone, Copy, Debug)]
pub struct ProcessFork {
    /// Until `fork()` returned in the parent (page-table copy).
    pub fork: Duration,
    /// Until the child reported it was running.
    pub ready: Duration,
    /// Until the child reported the result of its work.
    pub result: Duration,
    /// What the child's work returned.
    pub value: u64,
}

/// Forks the process; the child runs `work`, reports its result through a
/// pipe and exits with `_exit` (no destructors, no atexit handlers). The
/// parent waits for the child to exit.
///
/// Only the calling thread exists in the child, so `work` must not take a
/// lock another thread might hold at the time of the fork. forkasm calls this
/// only with work that runs guest code in a sandbox owned by the calling
/// thread; glibc's malloc is reset by its own fork handlers.
pub(crate) fn fork_run(work: impl FnOnce() -> u64) -> io::Result<ProcessFork> {
    let (rd, wr) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
    let t0 = Instant::now();
    // SAFETY: the child only runs `work` (see the contract above), writes to
    // the pipe and calls `_exit`; it never returns into the caller's frames.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        drop(rd);
        let mut wr = std::fs::File::from(wr);
        let _ = wr.write_all(&[1]);
        let value = work();
        let ok = wr.write_all(&value.to_le_bytes()).is_ok();
        // SAFETY: `_exit` ends the child without running destructors or
        // atexit handlers that could touch state shared with the parent.
        unsafe { libc::_exit(if ok { 0 } else { 1 }) };
    }
    let fork = t0.elapsed();
    drop(wr);
    let result = read_child(rd, t0);
    let mut status = 0;
    // SAFETY: `pid` is our child; `status` is a valid out-pointer.
    let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
    let (ready, result, value) = result?;
    if waited != pid || !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
        return Err(io::Error::other(format!("forked child failed (status {status:#x})")));
    }
    Ok(ProcessFork { fork, ready, result, value })
}

fn read_child(rd: OwnedFd, t0: Instant) -> io::Result<(Duration, Duration, u64)> {
    let mut rd = std::fs::File::from(rd);
    let mut byte = [0u8; 1];
    rd.read_exact(&mut byte)?;
    let ready = t0.elapsed();
    let mut value = [0u8; 8];
    rd.read_exact(&mut value)?;
    Ok((ready, t0.elapsed(), u64::from_le_bytes(value)))
}
