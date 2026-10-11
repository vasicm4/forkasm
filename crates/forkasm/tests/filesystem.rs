//! The VFS through the guest's own `std::fs` calls: file I/O, isolation
//! between forks (files, directories, open fds and their offsets), read-only
//! host mounts and determinism (SPEC §5.5, §7).

mod common;

use common::{STRATEGIES, kv_wasm, runtime};
use forkasm::{Runtime, Sandbox, SpawnOpts, Strategy, Val};

/// Copies `s` into the guest and returns `(ptr, len)` as wasm values.
fn arg(sb: &mut Sandbox, s: &[u8]) -> [Val; 2] {
    let ptr = sb.call_u64("alloc", &[Val::I32(s.len().max(1) as i32)]).unwrap();
    sb.write_memory(ptr as usize, s).unwrap();
    [Val::I32(ptr as i32), Val::I32(s.len() as i32)]
}

fn call_path(sb: &mut Sandbox, f: &str, path: &str) -> i64 {
    let a = arg(sb, path.as_bytes());
    sb.call_u64(f, &a).unwrap() as i64
}

fn call_2(sb: &mut Sandbox, f: &str, x: &[u8], y: &[u8]) -> i64 {
    let (a, b) = (arg(sb, x), arg(sb, y));
    sb.call_u64(f, &[a[0], a[1], b[0], b[1]]).unwrap() as i32 as i64
}

fn write(sb: &mut Sandbox, path: &str, data: &[u8]) -> i64 {
    call_2(sb, "write_file", path.as_bytes(), data)
}

fn append(sb: &mut Sandbox, path: &str, data: &[u8]) -> i64 {
    call_2(sb, "append_file", path.as_bytes(), data)
}

fn hash(sb: &mut Sandbox, path: &str) -> i64 {
    call_path(sb, "file_hash", path)
}

/// The guest's hash function, computed on the host.
fn expected(bytes: &[u8]) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    (h >> 1) as i64
}

const ENOENT: i64 = 44;
const ENOTEMPTY: i64 = 55;
const EROFS: i64 = 69;

fn kv(strategy: Strategy) -> (Runtime, Sandbox) {
    let rt = runtime(strategy);
    let module = rt.load(kv_wasm()).unwrap();
    let mut root = rt.spawn(&module, SpawnOpts::default()).unwrap();
    root.call("init", &[]).unwrap();
    (rt, root)
}

#[test]
fn guest_file_io() {
    for strategy in STRATEGIES {
        let (_rt, mut sb) = kv(strategy);
        assert_eq!(call_path(&mut sb, "fs_mkdir", "/data/sub"), 0);
        assert_eq!(write(&mut sb, "/data/a.txt", b"hello"), 0);
        assert_eq!(append(&mut sb, "/data/a.txt", b", world"), 0);
        assert_eq!(hash(&mut sb, "/data/a.txt"), expected(b"hello, world"));
        assert_eq!(sb.read_file("/data/a.txt").unwrap(), b"hello, world");
        assert_eq!(call_path(&mut sb, "file_len", "/data/a.txt"), 12);
        // Relative paths resolve against the root preopen.
        assert_eq!(hash(&mut sb, "data/a.txt"), expected(b"hello, world"));
        // A large file spanning several 64 KiB chunks.
        let big: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
        assert_eq!(write(&mut sb, "/data/big", &big), 0);
        assert_eq!(hash(&mut sb, "/data/big"), expected(&big));
        assert_eq!(call_path(&mut sb, "list_count", "/data"), 3);
        assert_eq!(call_2(&mut sb, "fs_rename", b"/data/a.txt", b"/data/sub/b.txt"), 0);
        assert_eq!(hash(&mut sb, "/data/a.txt"), -ENOENT);
        assert_eq!(hash(&mut sb, "/data/sub/b.txt"), expected(b"hello, world"));
        assert_eq!(call_path(&mut sb, "fs_remove", "/data/sub"), ENOTEMPTY);
        assert_eq!(call_path(&mut sb, "fs_remove", "/data/sub/b.txt"), 0);
        assert_eq!(call_path(&mut sb, "fs_remove", "/data/sub"), 0);
        assert_eq!(call_path(&mut sb, "list_count", "/data"), 1);
        // The host sees and edits the same tree.
        sb.write_file("/data/host.txt", b"from host").unwrap();
        assert_eq!(hash(&mut sb, "/data/host.txt"), expected(b"from host"));
        assert_eq!(sb.stats().open_fds, 4, "stdio + root preopen; nothing leaked");
    }
}

#[test]
fn file_isolation_between_forks() {
    for strategy in STRATEGIES {
        let (rt, mut root) = kv(strategy);
        assert_eq!(call_path(&mut root, "fs_mkdir", "/d"), 0);
        assert_eq!(write(&mut root, "/d/a", b"v1"), 0);
        let snap = root.snapshot().unwrap();
        let mut a = rt.spawn_from(&snap, 0).unwrap();
        let mut b = rt.spawn_from(&snap, 1).unwrap();

        assert_eq!(append(&mut a, "/d/a", b"+a"), 0);
        assert_eq!(call_path(&mut b, "fs_remove", "/d/a"), 0);
        assert_eq!(write(&mut b, "/d/b", b"only b"), 0);
        assert_eq!(write(&mut root, "/d/a", b"v2"), 0);

        assert_eq!(hash(&mut a, "/d/a"), expected(b"v1+a"), "{strategy:?}");
        assert_eq!(hash(&mut b, "/d/a"), -ENOENT);
        assert_eq!(hash(&mut root, "/d/a"), expected(b"v2"));
        assert_eq!(hash(&mut a, "/d/b"), -ENOENT);
        assert_eq!(hash(&mut root, "/d/b"), -ENOENT);
        let mut late = rt.spawn_from(&snap, 2).unwrap();
        assert_eq!(hash(&mut late, "/d/a"), expected(b"v1"));
        assert_eq!(call_path(&mut late, "list_count", "/d"), 1);
    }
}

#[test]
fn open_files_survive_forks_with_their_own_offsets() {
    for strategy in STRATEGIES {
        let (rt, mut root) = kv(strategy);
        assert_eq!(write(&mut root, "/f", b"0123456789"), 0);
        let h = call_path(&mut root, "hold_open", "/f");
        assert!(h >= 0);
        let held_read = |sb: &mut Sandbox, n: i32| {
            sb.call_u64("held_read", &[Val::I32(h as i32), Val::I32(n)]).unwrap() as i64
        };
        assert_eq!(held_read(&mut root, 3), expected(b"012"));
        let snap = root.snapshot().unwrap();
        let mut kid = rt.spawn_from(&snap, 0).unwrap();

        // The offset was captured; each side advances its own copy.
        assert_eq!(held_read(&mut kid, 3), expected(b"345"));
        assert_eq!(held_read(&mut kid, 3), expected(b"678"));
        assert_eq!(held_read(&mut root, 3), expected(b"345"));

        // Unlinked in the child: the open fd still reads it there; the
        // parent's file is untouched.
        assert_eq!(call_path(&mut kid, "fs_remove", "/f"), 0);
        assert_eq!(hash(&mut kid, "/f"), -ENOENT);
        let held_hash = |sb: &mut Sandbox| sb.call_u64("held_hash", &[Val::I32(h as i32)]).unwrap() as i64;
        assert_eq!(held_hash(&mut kid), expected(b"0123456789"));
        assert_eq!(hash(&mut root, "/f"), expected(b"0123456789"));
        let inodes = kid.stats().inodes;
        kid.call("release_all", &[]).unwrap();
        assert_eq!(kid.stats().inodes, inodes - 1, "closing the last fd frees the unlinked file");
        assert_eq!(kid.stats().open_fds, 4);
    }
}

#[test]
fn host_mounts_are_shared_and_read_only() {
    let dir = std::env::temp_dir().join(format!("forkasm-fs-test-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("pkg")).unwrap();
    std::fs::write(dir.join("pkg/mod.py"), b"x = 1\n").unwrap();
    for strategy in STRATEGIES {
        let rt = runtime(strategy);
        let module = rt.load(kv_wasm()).unwrap();
        let opts = SpawnOpts { mounts: vec![("/lib/py".into(), dir.clone())], ..SpawnOpts::default() };
        let mut root = rt.spawn(&module, opts).unwrap();
        root.call("init", &[]).unwrap();
        assert_eq!(hash(&mut root, "/lib/py/pkg/mod.py"), expected(b"x = 1\n"));
        assert_eq!(append(&mut root, "/lib/py/pkg/mod.py", b"y"), EROFS);
        assert_eq!(write(&mut root, "/lib/py/new.py", b"y"), EROFS);
        assert_eq!(call_path(&mut root, "fs_remove", "/lib/py/pkg/mod.py"), EROFS);
        // Writable next to the mount.
        assert_eq!(write(&mut root, "/lib/other.py", b"ok"), 0);
        let snap = root.snapshot().unwrap();
        let mut kid = rt.spawn_from(&snap, 0).unwrap();
        assert_eq!(hash(&mut kid, "/lib/py/pkg/mod.py"), expected(b"x = 1\n"));
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn filesystem_replays_deterministically() {
    for strategy in STRATEGIES {
        let (rt, mut root) = kv(strategy);
        assert_eq!(write(&mut root, "/seed", b"s"), 0);
        let snap = root.snapshot().unwrap();
        let run = |rt: &Runtime| {
            let mut sb = rt.spawn_from(&snap, 5).unwrap();
            for i in 0..20u8 {
                let r = sb.call_u64("rand", &[]).unwrap();
                assert_eq!(append(&mut sb, &format!("/f{}", r % 4), &[i]), 0);
            }
            (sb.wasi().vfs.walk(), sb.memory())
        };
        assert!(run(&rt) == run(&rt), "{strategy:?}: replay diverged");
    }
}
