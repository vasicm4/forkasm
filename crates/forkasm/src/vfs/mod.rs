//! A persistent copy-on-write filesystem (SPEC §5.5).
//!
//! Inodes live in an `imbl::OrdMap` (a persistent B-tree), directories are
//! `OrdMap`s from name to inode number, and file contents are vectors of
//! shared 64 KiB chunks. Cloning a [`Vfs`] is O(1); afterwards each side
//! copies only what it changes: a write copies one chunk plus the B-tree path
//! to its inode.
//!
//! The inode indirection (the spec sketches directories holding nodes
//! directly) gives POSIX semantics for open files: an fd refers to an inode,
//! so it keeps working after the file is renamed or unlinked.

mod file;

use std::path::Path;
use std::sync::Arc;

use imbl::OrdMap;

pub use file::{CHUNK, FileData};

pub type Ino = u64;

/// The root directory's inode.
pub const ROOT: Ino = 1;

/// Filesystem errors, named after the WASI errnos they map to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FsError {
    #[error("no such file or directory")]
    NoEnt,
    #[error("file exists")]
    Exist,
    #[error("not a directory")]
    NotDir,
    #[error("is a directory")]
    IsDir,
    #[error("directory not empty")]
    NotEmpty,
    #[error("read-only file system")]
    ReadOnly,
    #[error("invalid argument")]
    Inval,
    #[error("path escapes its directory")]
    NotCapable,
}

pub type FsResult<T> = Result<T, FsError>;

#[derive(Clone, Debug)]
pub enum Kind {
    File(Arc<FileData>),
    Dir(OrdMap<String, Ino>),
}

#[derive(Clone, Debug)]
pub struct Inode {
    pub kind: Kind,
    /// Directory entries naming this inode (0 once unlinked).
    pub nlink: u64,
    /// Modification time, ns since the Unix epoch.
    pub mtime: u64,
    /// Part of a read-only host mount.
    pub readonly: bool,
}

impl Inode {
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, Kind::Dir(_))
    }

    pub fn size(&self) -> u64 {
        match &self.kind {
            Kind::File(f) => f.len(),
            Kind::Dir(entries) => entries.len() as u64,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Vfs {
    inodes: OrdMap<Ino, Inode>,
    next_ino: Ino,
}

impl Default for Vfs {
    fn default() -> Self {
        let root = Inode { kind: Kind::Dir(OrdMap::new()), nlink: 1, mtime: 0, readonly: false };
        Vfs { inodes: OrdMap::unit(ROOT, root), next_ino: ROOT + 1 }
    }
}

impl Vfs {
    pub fn inode(&self, ino: Ino) -> FsResult<&Inode> {
        self.inodes.get(&ino).ok_or(FsError::NoEnt)
    }

    fn inode_mut(&mut self, ino: Ino) -> FsResult<&mut Inode> {
        self.inodes.get_mut(&ino).ok_or(FsError::NoEnt)
    }

    /// Number of live inodes.
    pub fn inode_count(&self) -> usize {
        self.inodes.len()
    }

    fn entries(&self, dir: Ino) -> FsResult<&OrdMap<String, Ino>> {
        match &self.inode(dir)?.kind {
            Kind::Dir(entries) => Ok(entries),
            Kind::File(_) => Err(FsError::NotDir),
        }
    }

    /// Resolves `path` relative to directory `dir`. `..` may not climb above
    /// `dir` (WASI capability rules); absolute paths are treated as relative.
    pub fn resolve(&self, dir: Ino, path: &str) -> FsResult<Ino> {
        let mut stack = vec![dir];
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    if stack.len() == 1 {
                        return Err(FsError::NotCapable);
                    }
                    stack.pop();
                }
                name => {
                    let here = *stack.last().expect("non-empty");
                    let next = *self.entries(here)?.get(name).ok_or(FsError::NoEnt)?;
                    stack.push(next);
                }
            }
        }
        if path.ends_with('/') && !self.inode(*stack.last().expect("non-empty"))?.is_dir() {
            return Err(FsError::NotDir);
        }
        Ok(*stack.last().expect("non-empty"))
    }

    /// Splits `path` into its parent directory (resolved) and final name.
    pub fn resolve_parent<'p>(&self, dir: Ino, path: &'p str) -> FsResult<(Ino, &'p str)> {
        let trimmed = path.trim_end_matches('/');
        let (parent, name) = match trimmed.rfind('/') {
            Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
            None => ("", trimmed),
        };
        match name {
            "" | "." | ".." => Err(FsError::Inval),
            _ => {
                let parent = self.resolve(dir, parent)?;
                self.entries(parent)?;
                Ok((parent, name))
            }
        }
    }

    pub fn lookup(&self, dir: Ino, name: &str) -> FsResult<Option<Ino>> {
        Ok(self.entries(dir)?.get(name).copied())
    }

    fn writable_dir(&mut self, dir: Ino) -> FsResult<&mut OrdMap<String, Ino>> {
        let inode = self.inode_mut(dir)?;
        if inode.readonly {
            return Err(FsError::ReadOnly);
        }
        match &mut inode.kind {
            Kind::Dir(entries) => Ok(entries),
            Kind::File(_) => Err(FsError::NotDir),
        }
    }

    fn alloc(&mut self, kind: Kind, now: u64, readonly: bool) -> Ino {
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(ino, Inode { kind, nlink: 1, mtime: now, readonly });
        ino
    }

    fn link(&mut self, dir: Ino, name: &str, kind: Kind, now: u64) -> FsResult<Ino> {
        let readonly = self.inode(dir)?.readonly;
        if self.writable_dir(dir)?.contains_key(name) {
            return Err(FsError::Exist);
        }
        let ino = self.alloc(kind, now, readonly);
        self.writable_dir(dir)?.insert(name.to_string(), ino);
        self.inode_mut(dir)?.mtime = now;
        Ok(ino)
    }

    pub fn create_file(&mut self, dir: Ino, name: &str, now: u64) -> FsResult<Ino> {
        self.link(dir, name, Kind::File(Arc::default()), now)
    }

    pub fn create_dir(&mut self, dir: Ino, name: &str, now: u64) -> FsResult<Ino> {
        self.link(dir, name, Kind::Dir(OrdMap::new()), now)
    }

    /// Removes the entry `name` from `dir`; `want_dir` selects rmdir vs
    /// unlink semantics. Returns the inode, now with `nlink == 0`; the caller
    /// [`forget`](Self::forget)s it once no fd refers to it.
    pub fn remove(&mut self, dir: Ino, name: &str, want_dir: bool, now: u64) -> FsResult<Ino> {
        let ino = self.lookup(dir, name)?.ok_or(FsError::NoEnt)?;
        let inode = self.inode(ino)?;
        match (&inode.kind, want_dir) {
            (Kind::Dir(_), false) => return Err(FsError::IsDir),
            (Kind::File(_), true) => return Err(FsError::NotDir),
            (Kind::Dir(entries), true) if !entries.is_empty() => return Err(FsError::NotEmpty),
            _ => {}
        }
        self.writable_dir(dir)?.remove(name);
        self.inode_mut(dir)?.mtime = now;
        self.inode_mut(ino)?.nlink = 0;
        Ok(ino)
    }

    /// Drops an unlinked inode's contents.
    pub fn forget(&mut self, ino: Ino) {
        if self.inodes.get(&ino).is_some_and(|i| i.nlink == 0) {
            self.inodes.remove(&ino);
        }
    }

    /// Moves `from_dir/from` to `to_dir/to`, replacing a compatible target.
    /// Returns the replaced inode (unlinked), if any.
    pub fn rename(
        &mut self,
        from_dir: Ino,
        from: &str,
        to_dir: Ino,
        to: &str,
        now: u64,
    ) -> FsResult<Option<Ino>> {
        let src = self.lookup(from_dir, from)?.ok_or(FsError::NoEnt)?;
        self.writable_dir(from_dir)?;
        self.writable_dir(to_dir)?;
        let src_is_dir = self.inode(src)?.is_dir();
        if src_is_dir && (to_dir == src || self.contains(src, to_dir)?) {
            return Err(FsError::Inval);
        }
        let replaced = match self.lookup(to_dir, to)? {
            Some(dst) if dst == src => return Ok(None),
            Some(dst) => {
                match (&self.inode(dst)?.kind, src_is_dir) {
                    (Kind::Dir(_), false) => return Err(FsError::IsDir),
                    (Kind::File(_), true) => return Err(FsError::NotDir),
                    (Kind::Dir(entries), true) if !entries.is_empty() => return Err(FsError::NotEmpty),
                    _ => {}
                }
                self.inode_mut(dst)?.nlink = 0;
                Some(dst)
            }
            None => None,
        };
        self.writable_dir(from_dir)?.remove(from);
        self.writable_dir(to_dir)?.insert(to.to_string(), src);
        self.inode_mut(from_dir)?.mtime = now;
        self.inode_mut(to_dir)?.mtime = now;
        Ok(replaced)
    }

    /// Whether `ino` is somewhere below directory `dir`.
    fn contains(&self, dir: Ino, ino: Ino) -> FsResult<bool> {
        let mut todo = vec![dir];
        while let Some(d) = todo.pop() {
            for (_, &child) in self.entries(d)?.iter() {
                if child == ino {
                    return Ok(true);
                }
                if self.inode(child)?.is_dir() {
                    todo.push(child);
                }
            }
        }
        Ok(false)
    }

    pub fn read(&self, ino: Ino, offset: u64, buf: &mut [u8]) -> FsResult<usize> {
        match &self.inode(ino)?.kind {
            Kind::File(f) => Ok(f.read(offset, buf)),
            Kind::Dir(_) => Err(FsError::IsDir),
        }
    }

    fn file_mut(&mut self, ino: Ino, now: u64) -> FsResult<&mut FileData> {
        let inode = self.inode_mut(ino)?;
        if inode.readonly {
            return Err(FsError::ReadOnly);
        }
        inode.mtime = now;
        match &mut inode.kind {
            Kind::File(f) => Ok(Arc::make_mut(f)),
            Kind::Dir(_) => Err(FsError::IsDir),
        }
    }

    pub fn write(&mut self, ino: Ino, offset: u64, data: &[u8], now: u64) -> FsResult<()> {
        self.file_mut(ino, now)?.write(offset, data);
        Ok(())
    }

    pub fn truncate(&mut self, ino: Ino, len: u64, now: u64) -> FsResult<()> {
        self.file_mut(ino, now)?.truncate(len);
        Ok(())
    }

    /// Entries of directory `dir`, sorted by name.
    pub fn list(&self, dir: Ino) -> FsResult<Vec<(String, Ino)>> {
        Ok(self.entries(dir)?.iter().map(|(n, &i)| (n.clone(), i)).collect())
    }

    /// Every path in the tree, sorted, with file contents (`None` for
    /// directories). For comparing filesystems in tests.
    pub fn walk(&self) -> Vec<(String, Option<Vec<u8>>)> {
        let mut out = Vec::new();
        let mut todo = vec![(String::new(), ROOT)];
        while let Some((path, dir)) = todo.pop() {
            for (name, ino) in self.list(dir).unwrap_or_default() {
                let child = format!("{path}/{name}");
                match &self.inodes[&ino].kind {
                    Kind::Dir(_) => {
                        out.push((child.clone(), None));
                        todo.push((child, ino));
                    }
                    Kind::File(f) => {
                        let mut bytes = vec![0; f.len() as usize];
                        f.read(0, &mut bytes);
                        out.push((child, Some(bytes)));
                    }
                }
            }
        }
        out.sort();
        out
    }

    /// Creates every missing directory along `path` (from the root).
    pub fn create_dir_all(&mut self, path: &str, now: u64) -> FsResult<Ino> {
        let mut dir = ROOT;
        for part in path.split('/').filter(|p| !p.is_empty() && *p != ".") {
            dir = match self.lookup(dir, part)? {
                Some(ino) if self.inode(ino)?.is_dir() => ino,
                Some(_) => return Err(FsError::NotDir),
                None => self.create_dir(dir, part, now)?,
            };
        }
        Ok(dir)
    }

    /// Copies the host directory tree at `host` into the VFS at `guest`,
    /// read-only. Contents are shared by every fork.
    pub fn mount_host(&mut self, guest: &str, host: &Path) -> std::io::Result<()> {
        let dir = self.create_dir_all(guest, 0).map_err(std::io::Error::other)?;
        self.copy_host_dir(dir, host)?;
        self.mark_readonly(dir);
        Ok(())
    }

    fn copy_host_dir(&mut self, dir: Ino, host: &Path) -> std::io::Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(host)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let ty = entry.file_type()?;
            let path = entry.path();
            // Follow symlinks: the VFS has none.
            let is_dir = if ty.is_symlink() { path.is_dir() } else { ty.is_dir() };
            if is_dir {
                let sub = self.create_dir(dir, &name, 0).map_err(std::io::Error::other)?;
                self.copy_host_dir(sub, &path)?;
            } else {
                let data = std::fs::read(&path)?;
                let ino = self.create_file(dir, &name, 0).map_err(std::io::Error::other)?;
                self.write(ino, 0, &data, 0).map_err(std::io::Error::other)?;
            }
        }
        Ok(())
    }

    fn mark_readonly(&mut self, ino: Ino) {
        let children = self.list(ino).unwrap_or_default();
        if let Some(inode) = self.inodes.get_mut(&ino) {
            inode.readonly = true;
        }
        for (_, child) in children {
            self.mark_readonly(child);
        }
    }
}

#[cfg(test)]
mod tests;
