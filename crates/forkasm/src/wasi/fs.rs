//! File-descriptor and path calls on top of the [`Vfs`](crate::vfs::Vfs).
//!
//! fd 0-2 are stdio (stdin reads EOF, stdout/stderr are captured), fd 3 is
//! the preopened root `/`. Every other fd is a file or directory opened by the
//! guest. The table is plain data, cloned on fork with the rest of
//! [`WasiState`].

use std::collections::BTreeMap;

use wasmtime::{Caller, Linker};

use super::{Errno, GuestMem, WasiState, errno, with_mem};
use crate::vfs::{FsError, Ino, Kind, ROOT};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    Stdin,
    Stdout,
    Stderr,
    Dir { ino: Ino, preopen: Option<String> },
    File { ino: Ino, offset: u64, append: bool, read: bool, write: bool },
}

#[derive(Clone, Debug)]
pub struct FdTable {
    entries: BTreeMap<u32, Entry>,
}

impl Default for FdTable {
    fn default() -> Self {
        let entries = BTreeMap::from([
            (0, Entry::Stdin),
            (1, Entry::Stdout),
            (2, Entry::Stderr),
            (3, Entry::Dir { ino: ROOT, preopen: Some("/".into()) }),
        ]);
        FdTable { entries }
    }
}

impl FdTable {
    fn get(&self, fd: i32) -> Result<&Entry, Errno> {
        self.entries.get(&(fd as u32)).ok_or(errno::BADF)
    }

    fn get_mut(&mut self, fd: i32) -> Result<&mut Entry, Errno> {
        self.entries.get_mut(&(fd as u32)).ok_or(errno::BADF)
    }

    fn insert(&mut self, entry: Entry) -> u32 {
        let fd = (0..).find(|fd| !self.entries.contains_key(fd)).expect("free fd");
        self.entries.insert(fd, entry);
        fd
    }

    fn dir(&self, fd: i32) -> Result<Ino, Errno> {
        match self.get(fd)? {
            Entry::Dir { ino, .. } => Ok(*ino),
            _ => Err(errno::NOTDIR),
        }
    }

    /// Whether any open fd refers to inode `ino`.
    pub(crate) fn references(&self, ino: Ino) -> bool {
        self.entries
            .values()
            .any(|e| matches!(e, Entry::Dir { ino: i, .. } | Entry::File { ino: i, .. } if *i == ino))
    }

    /// Number of open fds (including stdio and the preopen).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl From<FsError> for Errno {
    fn from(e: FsError) -> Errno {
        match e {
            FsError::NoEnt => errno::NOENT,
            FsError::Exist => errno::EXIST,
            FsError::NotDir => errno::NOTDIR,
            FsError::IsDir => errno::ISDIR,
            FsError::NotEmpty => errno::NOTEMPTY,
            FsError::ReadOnly => errno::ROFS,
            FsError::Inval => errno::INVAL,
            FsError::NotCapable => errno::NOTCAPABLE,
        }
    }
}

// From the preview1 spec.
const FILETYPE_CHARACTER_DEVICE: u8 = 2;
const FILETYPE_DIRECTORY: u8 = 3;
const FILETYPE_REGULAR_FILE: u8 = 4;
const OFLAGS_CREAT: i32 = 1;
const OFLAGS_DIRECTORY: i32 = 2;
const OFLAGS_EXCL: i32 = 4;
const OFLAGS_TRUNC: i32 = 8;
const FDFLAGS_APPEND: i32 = 1;
const RIGHT_FD_READ: i64 = 1 << 1;
const RIGHT_FD_WRITE: i64 = 1 << 6;
const ALL_RIGHTS: u64 = (1 << 29) - 1;

/// Calls this module implements (everything else gets the `ENOSYS` stub).
pub(super) const IMPLEMENTED: &[&str] = &[
    "fd_write",
    "fd_read",
    "fd_pread",
    "fd_pwrite",
    "fd_seek",
    "fd_tell",
    "fd_close",
    "fd_renumber",
    "fd_fdstat_get",
    "fd_fdstat_set_flags",
    "fd_filestat_get",
    "fd_filestat_set_size",
    "fd_filestat_set_times",
    "fd_sync",
    "fd_datasync",
    "fd_advise",
    "fd_allocate",
    "fd_prestat_get",
    "fd_prestat_dir_name",
    "fd_readdir",
    "path_open",
    "path_filestat_get",
    "path_filestat_set_times",
    "path_create_directory",
    "path_unlink_file",
    "path_remove_directory",
    "path_rename",
    "path_readlink",
];

/// Registers a call whose body is `$body`, run with guest memory `$m` and
/// state `$s`, returning `Result<Errno, Errno>`.
macro_rules! call {
    ($linker:ident, $name:literal, |$m:ident, $s:ident $(, $arg:ident : $ty:ty)*| $body:expr) => {
        $linker.func_wrap(super::MODULE, $name, |mut c: Caller<'_, WasiState>, $($arg: $ty),*| {
            with_mem(&mut c, |$m, $s| $body)
        })?;
    };
}

pub(super) fn add_to_linker(linker: &mut Linker<WasiState>) -> anyhow::Result<()> {
    call!(linker, "fd_write", |m, s, fd: i32, iovs: i32, n: i32, out: i32| {
        let data = m.gather(iovs, n)?;
        let written = write(s, fd, None, &data)?;
        m.write_u32(out, written)
    });
    call!(linker, "fd_pwrite", |m, s, fd: i32, iovs: i32, n: i32, offset: i64, out: i32| {
        let data = m.gather(iovs, n)?;
        let written = write(s, fd, Some(offset as u64), &data)?;
        m.write_u32(out, written)
    });
    call!(linker, "fd_read", |m, s, fd: i32, iovs: i32, n: i32, out: i32| {
        let read = read(m, s, fd, None, iovs, n)?;
        m.write_u32(out, read)
    });
    call!(linker, "fd_pread", |m, s, fd: i32, iovs: i32, n: i32, offset: i64, out: i32| {
        let read = read(m, s, fd, Some(offset as u64), iovs, n)?;
        m.write_u32(out, read)
    });
    call!(linker, "fd_seek", |m, s, fd: i32, delta: i64, whence: i32, out: i32| {
        let size = match s.fds.get(fd)? {
            Entry::File { ino, .. } => s.vfs.inode(*ino)?.size(),
            Entry::Stdin | Entry::Stdout | Entry::Stderr => return Err(errno::SPIPE),
            Entry::Dir { .. } => return Err(errno::BADF),
        };
        let Entry::File { offset, .. } = s.fds.get_mut(fd)? else { unreachable!() };
        let base = match whence {
            0 => 0,
            1 => *offset as i64,
            2 => size as i64,
            _ => return Err(errno::INVAL),
        };
        let new = base.checked_add(delta).filter(|&o| o >= 0).ok_or(errno::INVAL)?;
        *offset = new as u64;
        m.write_u64(out, new as u64)
    });
    call!(linker, "fd_tell", |m, s, fd: i32, out: i32| {
        match s.fds.get(fd)? {
            Entry::File { offset, .. } => m.write_u64(out, *offset),
            Entry::Stdin | Entry::Stdout | Entry::Stderr => Err(errno::SPIPE),
            Entry::Dir { .. } => Err(errno::BADF),
        }
    });
    call!(linker, "fd_close", |_m, s, fd: i32| {
        let entry = s.fds.entries.remove(&(fd as u32)).ok_or(errno::BADF)?;
        if let Entry::File { ino, .. } | Entry::Dir { ino, .. } = entry {
            release(s, ino);
        }
        Ok(errno::SUCCESS)
    });
    call!(linker, "fd_renumber", |_m, s, from: i32, to: i32| {
        s.fds.get(to)?;
        let entry = s.fds.entries.remove(&(from as u32)).ok_or(errno::BADF)?;
        if let Some(Entry::File { ino, .. } | Entry::Dir { ino, .. }) = s.fds.entries.insert(to as u32, entry)
        {
            release(s, ino);
        }
        Ok(errno::SUCCESS)
    });
    call!(linker, "fd_fdstat_get", |m, s, fd: i32, out: i32| {
        let (filetype, flags) = match s.fds.get(fd)? {
            Entry::Stdin | Entry::Stdout | Entry::Stderr => (FILETYPE_CHARACTER_DEVICE, 0),
            Entry::Dir { .. } => (FILETYPE_DIRECTORY, 0),
            Entry::File { append, .. } => {
                (FILETYPE_REGULAR_FILE, if *append { FDFLAGS_APPEND as u16 } else { 0 })
            }
        };
        let stat = m.slice_mut(out, 24)?;
        stat.fill(0);
        stat[0] = filetype;
        stat[2..4].copy_from_slice(&flags.to_le_bytes());
        stat[8..16].copy_from_slice(&ALL_RIGHTS.to_le_bytes());
        stat[16..24].copy_from_slice(&ALL_RIGHTS.to_le_bytes());
        Ok(errno::SUCCESS)
    });
    call!(linker, "fd_fdstat_set_flags", |_m, s, fd: i32, flags: i32| {
        if let Entry::File { append, .. } = s.fds.get_mut(fd)? {
            *append = flags & FDFLAGS_APPEND != 0;
        }
        Ok(errno::SUCCESS)
    });
    call!(linker, "fd_filestat_get", |m, s, fd: i32, out: i32| {
        match s.fds.get(fd)? {
            Entry::File { ino, .. } | Entry::Dir { ino, .. } => {
                let ino = *ino;
                filestat(m, s, ino, out)
            }
            _ => {
                let stat = m.slice_mut(out, 64)?;
                stat.fill(0);
                stat[16] = FILETYPE_CHARACTER_DEVICE;
                Ok(errno::SUCCESS)
            }
        }
    });
    call!(linker, "fd_filestat_set_size", |_m, s, fd: i32, size: i64| {
        let ino = writable_file(s, fd)?;
        let now = s.clock.peek_realtime();
        s.vfs.truncate(ino, size as u64, now)?;
        Ok(errno::SUCCESS)
    });
    call!(linker, "fd_allocate", |_m, s, fd: i32, offset: i64, len: i64| {
        let ino = writable_file(s, fd)?;
        let end = (offset as u64).checked_add(len as u64).ok_or(errno::INVAL)?;
        if end > s.vfs.inode(ino)?.size() {
            let now = s.clock.peek_realtime();
            s.vfs.truncate(ino, end, now)?;
        }
        Ok(errno::SUCCESS)
    });
    // Timestamps are not settable; syncing and advice are no-ops.
    call!(linker, "fd_filestat_set_times", |_m, s, fd: i32, _a: i64, _b: i64, _f: i32| {
        s.fds.get(fd).map(|_| errno::SUCCESS)
    });
    call!(linker, "fd_sync", |_m, s, fd: i32| s.fds.get(fd).map(|_| errno::SUCCESS));
    call!(linker, "fd_datasync", |_m, s, fd: i32| s.fds.get(fd).map(|_| errno::SUCCESS));
    call!(linker, "fd_advise", |_m, s, fd: i32, _o: i64, _l: i64, _a: i32| {
        s.fds.get(fd).map(|_| errno::SUCCESS)
    });
    call!(linker, "fd_prestat_get", |m, s, fd: i32, out: i32| {
        let Entry::Dir { preopen: Some(name), .. } = s.fds.get(fd)? else { return Err(errno::BADF) };
        let stat = m.slice_mut(out, 8)?;
        stat.fill(0); // tag 0: directory
        stat[4..8].copy_from_slice(&(name.len() as u32).to_le_bytes());
        Ok(errno::SUCCESS)
    });
    call!(linker, "fd_prestat_dir_name", |m, s, fd: i32, path: i32, len: i32| {
        let Entry::Dir { preopen: Some(name), .. } = s.fds.get(fd)? else { return Err(errno::BADF) };
        let n = name.len().min(len as usize);
        m.slice_mut(path, n as u32)?.copy_from_slice(&name.as_bytes()[..n]);
        Ok(errno::SUCCESS)
    });
    call!(linker, "fd_readdir", |m, s, fd: i32, buf: i32, len: i32, cookie: i64, out: i32| {
        let dir = s.fds.dir(fd)?;
        let mut entries = vec![(".".to_string(), dir), ("..".to_string(), dir)];
        entries.extend(s.vfs.list(dir)?);
        let mut bytes = Vec::new();
        for (i, (name, ino)) in entries.iter().enumerate().skip(cookie as usize) {
            let filetype =
                if s.vfs.inode(*ino)?.is_dir() { FILETYPE_DIRECTORY } else { FILETYPE_REGULAR_FILE };
            bytes.extend_from_slice(&(i as u64 + 1).to_le_bytes()); // d_next
            bytes.extend_from_slice(&ino.to_le_bytes());
            bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&[filetype, 0, 0, 0]);
            bytes.extend_from_slice(name.as_bytes());
            if bytes.len() >= len as usize {
                break;
            }
        }
        // A full buffer (even with a truncated last entry) tells the guest
        // to call again with a larger buffer or a later cookie.
        let n = bytes.len().min(len as usize);
        m.slice_mut(buf, n as u32)?.copy_from_slice(&bytes[..n]);
        m.write_u32(out, n as u32)
    });
    call!(linker, "path_open", |m,
                                s,
                                fd: i32,
                                _lookup: i32,
                                path: i32,
                                path_len: i32,
                                oflags: i32,
                                rights: i64,
                                _inheriting: i64,
                                fdflags: i32,
                                out: i32| {
        let path = m.read_str(path, path_len)?;
        let entry = open(s, fd, &path, oflags, rights, fdflags)?;
        let new_fd = s.fds.insert(entry);
        m.write_u32(out, new_fd)
    });
    call!(linker, "path_filestat_get", |m, s, fd: i32, _flags: i32, path: i32, len: i32, out: i32| {
        let path = m.read_str(path, len)?;
        let ino = s.vfs.resolve(s.fds.dir(fd)?, &path)?;
        filestat(m, s, ino, out)
    });
    call!(linker, "path_filestat_set_times", |m,
                                              s,
                                              fd: i32,
                                              _flags: i32,
                                              path: i32,
                                              len: i32,
                                              _a: i64,
                                              _b: i64,
                                              _f: i32| {
        let path = m.read_str(path, len)?;
        s.vfs.resolve(s.fds.dir(fd)?, &path)?;
        Ok(errno::SUCCESS)
    });
    call!(linker, "path_create_directory", |m, s, fd: i32, path: i32, len: i32| {
        let path = m.read_str(path, len)?;
        let (parent, name) = s.vfs.resolve_parent(s.fds.dir(fd)?, &path)?;
        let now = s.clock.peek_realtime();
        s.vfs.create_dir(parent, name, now)?;
        Ok(errno::SUCCESS)
    });
    call!(linker, "path_unlink_file", |m, s, fd: i32, path: i32, len: i32| {
        let path = m.read_str(path, len)?;
        remove(s, fd, &path, false)
    });
    call!(linker, "path_remove_directory", |m, s, fd: i32, path: i32, len: i32| {
        let path = m.read_str(path, len)?;
        remove(s, fd, &path, true)
    });
    call!(linker, "path_rename", |m,
                                  s,
                                  fd: i32,
                                  old: i32,
                                  old_len: i32,
                                  new_fd: i32,
                                  new: i32,
                                  new_len: i32| {
        let (old, new) = (m.read_str(old, old_len)?, m.read_str(new, new_len)?);
        let (from_dir, from) = s.vfs.resolve_parent(s.fds.dir(fd)?, &old)?;
        let (to_dir, to) = s.vfs.resolve_parent(s.fds.dir(new_fd)?, &new)?;
        let now = s.clock.peek_realtime();
        if let Some(replaced) = s.vfs.rename(from_dir, from, to_dir, to, now)? {
            release(s, replaced);
        }
        Ok(errno::SUCCESS)
    });
    // There are no symlinks: an existing path is "not a link".
    call!(linker, "path_readlink", |m, s, fd: i32, path: i32, len: i32, _buf: i32, _cap: i32, _out: i32| {
        let path = m.read_str(path, len)?;
        s.vfs.resolve(s.fds.dir(fd)?, &path)?;
        Err(errno::INVAL)
    });
    Ok(())
}

/// Forgets an unlinked inode once no fd refers to it.
fn release(s: &mut WasiState, ino: Ino) {
    if !s.fds.references(ino) {
        s.vfs.forget(ino);
    }
}

fn open(
    s: &mut WasiState,
    fd: i32,
    path: &str,
    oflags: i32,
    rights: i64,
    fdflags: i32,
) -> Result<Entry, Errno> {
    let dir = s.fds.dir(fd)?;
    let now = s.clock.peek_realtime();
    let ino = if oflags & OFLAGS_CREAT != 0 {
        let (parent, name) = s.vfs.resolve_parent(dir, path)?;
        match s.vfs.lookup(parent, name)? {
            Some(_) if oflags & OFLAGS_EXCL != 0 => return Err(errno::EXIST),
            Some(ino) => ino,
            None => s.vfs.create_file(parent, name, now)?,
        }
    } else {
        s.vfs.resolve(dir, path)?
    };
    let inode = s.vfs.inode(ino)?;
    let write = rights & RIGHT_FD_WRITE != 0 || oflags & OFLAGS_TRUNC != 0;
    if inode.is_dir() {
        if write {
            return Err(errno::ISDIR);
        }
        return Ok(Entry::Dir { ino, preopen: None });
    }
    if oflags & OFLAGS_DIRECTORY != 0 {
        return Err(errno::NOTDIR);
    }
    if write && inode.readonly {
        return Err(errno::ROFS);
    }
    if oflags & OFLAGS_TRUNC != 0 {
        s.vfs.truncate(ino, 0, now)?;
    }
    let read = rights & RIGHT_FD_READ != 0 || !write;
    Ok(Entry::File { ino, offset: 0, append: fdflags & FDFLAGS_APPEND != 0, read, write })
}

fn remove(s: &mut WasiState, fd: i32, path: &str, dir: bool) -> Result<Errno, Errno> {
    let (parent, name) = s.vfs.resolve_parent(s.fds.dir(fd)?, path)?;
    let now = s.clock.peek_realtime();
    let ino = s.vfs.remove(parent, name, dir, now)?;
    release(s, ino);
    Ok(errno::SUCCESS)
}

fn writable_file(s: &WasiState, fd: i32) -> Result<Ino, Errno> {
    match s.fds.get(fd)? {
        Entry::File { ino, write: true, .. } => Ok(*ino),
        Entry::Dir { .. } => Err(errno::ISDIR),
        _ => Err(errno::BADF),
    }
}

/// Writes `data` to `fd`: at `at`, or at the fd's offset (advancing it).
fn write(s: &mut WasiState, fd: i32, at: Option<u64>, data: &[u8]) -> Result<u32, Errno> {
    let now = s.clock.peek_realtime();
    match s.fds.get_mut(fd)? {
        Entry::Stdout => s.stdout.extend_from_slice(data),
        Entry::Stderr => s.stderr.extend_from_slice(data),
        Entry::File { ino, offset, append, write: true, .. } => {
            let ino = *ino;
            let pos = match at {
                Some(pos) => pos,
                None if *append => s.vfs.inode(ino)?.size(),
                None => *offset,
            };
            if at.is_none() {
                *offset = pos + data.len() as u64;
            }
            s.vfs.write(ino, pos, data, now)?;
        }
        Entry::Dir { .. } => return Err(errno::ISDIR),
        _ => return Err(errno::BADF),
    }
    Ok(data.len() as u32)
}

/// Reads from `fd` into the guest's iovecs: at `at`, or at the fd's offset
/// (advancing it).
fn read(
    m: &mut GuestMem<'_>,
    s: &mut WasiState,
    fd: i32,
    at: Option<u64>,
    iovs: i32,
    n: i32,
) -> Result<u32, Errno> {
    let (ino, pos) = match s.fds.get(fd)? {
        Entry::Stdin => return Ok(0),
        Entry::File { ino, offset, read: true, .. } => (*ino, at.unwrap_or(*offset)),
        Entry::Dir { .. } => return Err(errno::ISDIR),
        _ => return Err(errno::BADF),
    };
    let mut total = 0u32;
    for (ptr, len) in m.iovecs(iovs, n)? {
        let buf = m.slice_mut(ptr as i32, len)?;
        let got = s.vfs.read(ino, pos + total as u64, buf)?;
        total += got as u32;
        if got < len as usize {
            break;
        }
    }
    if at.is_none()
        && let Entry::File { offset, .. } = s.fds.get_mut(fd)?
    {
        *offset = pos + total as u64;
    }
    Ok(total)
}

fn filestat(m: &mut GuestMem<'_>, s: &WasiState, ino: Ino, out: i32) -> Result<Errno, Errno> {
    let inode = s.vfs.inode(ino)?;
    let filetype = match inode.kind {
        Kind::Dir(_) => FILETYPE_DIRECTORY,
        Kind::File(_) => FILETYPE_REGULAR_FILE,
    };
    let stat = m.slice_mut(out, 64)?;
    stat.fill(0);
    stat[0..8].copy_from_slice(&1u64.to_le_bytes()); // dev
    stat[8..16].copy_from_slice(&ino.to_le_bytes());
    stat[16] = filetype;
    stat[24..32].copy_from_slice(&inode.nlink.max(1).to_le_bytes());
    stat[32..40].copy_from_slice(&inode.size().to_le_bytes());
    for at in [40, 48, 56] {
        stat[at..at + 8].copy_from_slice(&inode.mtime.to_le_bytes());
    }
    Ok(errno::SUCCESS)
}
