use super::*;

fn read_all(vfs: &Vfs, path: &str) -> Vec<u8> {
    let ino = vfs.resolve(ROOT, path).unwrap();
    let mut buf = vec![0; vfs.inode(ino).unwrap().size() as usize];
    assert_eq!(vfs.read(ino, 0, &mut buf).unwrap(), buf.len());
    buf
}

fn put(vfs: &mut Vfs, path: &str, data: &[u8]) -> Ino {
    let (dir, name) = vfs.resolve_parent(ROOT, path).unwrap();
    let ino = match vfs.lookup(dir, name).unwrap() {
        Some(ino) => ino,
        None => vfs.create_file(dir, name, 1).unwrap(),
    };
    vfs.write(ino, 0, data, 2).unwrap();
    ino
}

fn file(vfs: &Vfs, ino: Ino) -> &FileData {
    match &vfs.inode(ino).unwrap().kind {
        Kind::File(f) => f,
        Kind::Dir(_) => panic!("not a file"),
    }
}

#[test]
fn file_data_reads_writes_holes_and_truncates() {
    let mut f = FileData::default();
    f.write(10, b"abc");
    assert_eq!(f.len(), 13);
    let mut buf = [9u8; 20];
    assert_eq!(f.read(0, &mut buf), 13);
    assert_eq!(&buf[..13], b"\0\0\0\0\0\0\0\0\0\0abc");
    // Across a chunk boundary, then far past it (a hole chunk in between).
    let big: Vec<u8> = (0..CHUNK + 100).map(|i| i as u8).collect();
    f.write(CHUNK as u64 - 50, &big);
    f.write(5 * CHUNK as u64, b"z");
    let mut out = vec![0; big.len()];
    assert_eq!(f.read(CHUNK as u64 - 50, &mut out), big.len());
    assert_eq!(out, big);
    let mut hole = [7u8; 10];
    f.read(4 * CHUNK as u64, &mut hole);
    assert_eq!(hole, [0; 10]);
    // Truncate then extend: the cut bytes read back as zeros.
    f.truncate(12);
    f.write(20, b"q");
    let mut buf = [9u8; 21];
    assert_eq!(f.read(0, &mut buf), 21);
    assert_eq!(&buf[10..], b"ab\0\0\0\0\0\0\0\0q");
}

#[test]
fn fork_shares_until_written() {
    let mut a = Vfs::default();
    a.create_dir_all("/d/e", 0).unwrap();
    let data: Vec<u8> = (0..3 * CHUNK).map(|i| (i / 7) as u8).collect();
    let ino = put(&mut a, "/d/e/f", &data);
    let mut b = a.clone();
    assert_eq!(file(&a, ino).shared_chunks(file(&b, ino)), 3);

    // One byte in the child copies one chunk; the parent is unchanged.
    b.write(ino, CHUNK as u64 + 5, b"X", 3).unwrap();
    assert_eq!(file(&a, ino).shared_chunks(file(&b, ino)), 2);
    assert_eq!(read_all(&a, "/d/e/f"), data);
    assert_eq!(read_all(&b, "/d/e/f")[CHUNK + 5], b'X');

    // Namespace changes are isolated too.
    put(&mut a, "/d/new", b"parent");
    b.remove(b.resolve(ROOT, "/d/e").unwrap(), "f", false, 4).unwrap();
    assert!(b.resolve(ROOT, "/d/new").is_err());
    assert_eq!(read_all(&a, "/d/e/f"), data);
    assert_eq!(b.resolve(ROOT, "/d/e/f"), Err(FsError::NoEnt));
}

#[test]
fn resolve_rules() {
    let mut v = Vfs::default();
    let d = v.create_dir_all("a/b", 0).unwrap();
    put(&mut v, "/a/f", b"x");
    assert_eq!(v.resolve(ROOT, "a/./b/../b"), Ok(d));
    assert_eq!(v.resolve(ROOT, "/a/b/"), Ok(d));
    assert_eq!(v.resolve(ROOT, "a/f/"), Err(FsError::NotDir));
    assert_eq!(v.resolve(ROOT, "a/f/x"), Err(FsError::NotDir));
    assert_eq!(v.resolve(ROOT, ".."), Err(FsError::NotCapable));
    assert_eq!(v.resolve(d, "../.."), Err(FsError::NotCapable));
    assert_eq!(v.resolve_parent(ROOT, "a/b/c"), Ok((d, "c")));
    assert_eq!(v.resolve_parent(ROOT, "a/b/"), Ok((v.resolve(ROOT, "a").unwrap(), "b")));
    assert_eq!(v.resolve_parent(ROOT, "a/.."), Err(FsError::Inval));
}

#[test]
fn remove_and_rename_rules() {
    let mut v = Vfs::default();
    let a = v.create_dir_all("a", 0).unwrap();
    let b = v.create_dir_all("a/b", 0).unwrap();
    let f = put(&mut v, "/a/f", b"1");
    let g = put(&mut v, "/a/g", b"2");
    assert_eq!(v.remove(ROOT, "a", true, 1), Err(FsError::NotEmpty));
    assert_eq!(v.remove(a, "b", false, 1), Err(FsError::IsDir));
    assert_eq!(v.remove(a, "f", true, 1), Err(FsError::NotDir));
    // Renaming a file over another replaces it.
    assert_eq!(v.rename(a, "f", a, "g", 1), Ok(Some(g)));
    assert_eq!(v.lookup(a, "g"), Ok(Some(f)));
    // File over directory and directory into itself are refused.
    assert_eq!(v.rename(a, "g", a, "b", 1), Err(FsError::IsDir));
    assert_eq!(v.rename(ROOT, "a", b, "a2", 1), Err(FsError::Inval));
    // An unlinked inode lives on until forgotten.
    let gone = v.remove(a, "g", false, 2).unwrap();
    assert_eq!(gone, f);
    assert_eq!(v.read(f, 0, &mut [0; 1]), Ok(1));
    v.forget(f);
    assert_eq!(v.read(f, 0, &mut [0; 1]), Err(FsError::NoEnt));
}

#[test]
fn host_mounts_are_read_only_and_shared() {
    let dir = std::env::temp_dir().join(format!("forkasm-vfs-test-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/m.py"), b"print(1)").unwrap();
    std::fs::write(dir.join("top.txt"), b"top").unwrap();
    let mut v = Vfs::default();
    v.mount_host("/lib/py", &dir).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();

    assert_eq!(read_all(&v, "/lib/py/sub/m.py"), b"print(1)");
    let ino = v.resolve(ROOT, "/lib/py/top.txt").unwrap();
    assert_eq!(v.write(ino, 0, b"x", 1), Err(FsError::ReadOnly));
    let sub = v.resolve(ROOT, "/lib/py/sub").unwrap();
    assert_eq!(v.create_file(sub, "new", 1), Err(FsError::ReadOnly));
    assert_eq!(v.remove(sub, "m.py", false, 1), Err(FsError::ReadOnly));
    // The mount point's parent stays writable.
    put(&mut v, "/lib/other", b"ok");
    let fork = v.clone();
    assert_eq!(file(&v, ino).shared_chunks(file(&fork, ino)), 1);
}
