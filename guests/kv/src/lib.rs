//! G1: an in-memory key-value store guest for forkasm (SPEC §6).
//!
//! Build: `cargo build --release --target wasm32-wasip1` (a WASI reactor).
//! Every export is called by the host one at a time, so the guest keeps its
//! state in `static`s behind a single-threaded cell.

use std::cell::RefCell;
use std::collections::BTreeMap;

const PAGE: usize = 4096;

#[derive(Default)]
struct State {
    map: BTreeMap<u64, u64>,
    /// A large buffer for `touch_pages`, page-aligned within itself.
    buf: Vec<u8>,
    calls: u64,
    /// Files kept open across calls (`hold_open`).
    held: Vec<std::fs::File>,
}

struct Global(RefCell<Option<State>>);
// SAFETY: wasm32-wasip1 guests are single-threaded.
unsafe impl Sync for Global {}

static STATE: Global = Global(RefCell::new(None));

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut s = STATE.0.borrow_mut();
    let s = s.get_or_insert_with(State::default);
    s.calls += 1;
    f(s)
}

/// Initialises the store and prints a greeting (exercises `fd_write`, args
/// and env).
#[unsafe(no_mangle)]
pub extern "C" fn init() {
    let args = std::env::args().count();
    let envs = std::env::vars().count();
    with(|_| ());
    println!("kv: ready (args={args}, env={envs})");
}

#[unsafe(no_mangle)]
pub extern "C" fn put(k: u64, v: u64) {
    with(|s| s.map.insert(k, v));
}

/// Returns the value for `k`, or `u64::MAX` when missing.
#[unsafe(no_mangle)]
pub extern "C" fn get(k: u64) -> u64 {
    with(|s| s.map.get(&k).copied().unwrap_or(u64::MAX))
}

#[unsafe(no_mangle)]
pub extern "C" fn len() -> u32 {
    with(|s| s.map.len() as u32)
}

/// Sizes the page buffer to `pages` pages (zeroed).
#[unsafe(no_mangle)]
pub extern "C" fn buf_init(pages: u32) {
    with(|s| s.buf = vec![0; (pages as usize + 1) * PAGE]);
}

/// Writes `val` into one byte of each buffer page in `[start, start + n)`, so
/// exactly those host pages become dirty. Returns how many pages were touched.
#[unsafe(no_mangle)]
pub extern "C" fn touch_pages(start: u32, n: u32, val: u32) -> u32 {
    with(|s| {
        let base = s.buf.as_ptr() as usize;
        let first = base.next_multiple_of(PAGE) - base;
        let mut touched = 0;
        for p in start..start + n {
            let i = first + p as usize * PAGE + (p as usize % 64);
            if let Some(b) = s.buf.get_mut(i) {
                *b = b.wrapping_add(val as u8);
                touched += 1;
            }
        }
        touched
    })
}

/// FNV-1a over the store, the page buffer and the call counter.
#[unsafe(no_mangle)]
pub extern "C" fn checksum() -> u64 {
    with(|s| {
        let mut h = Fnv::new();
        for (k, v) in &s.map {
            h.eat(&k.to_le_bytes());
            h.eat(&v.to_le_bytes());
        }
        h.eat(&s.buf);
        h.eat(&s.calls.to_le_bytes());
        h.0
    })
}

/// A random number from the host (`random_get`).
#[unsafe(no_mangle)]
pub extern "C" fn rand() -> u64 {
    let mut b = [0u8; 8];
    // SAFETY: `b` is valid for 8 writable bytes.
    let rc = unsafe { wasi::random_get(b.as_mut_ptr(), b.len()) };
    assert_eq!(rc, 0);
    u64::from_le_bytes(b)
}

/// The host's clock (`clock_time_get`, realtime), in nanoseconds.
#[unsafe(no_mangle)]
pub extern "C" fn now() -> u64 {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    d.as_nanos() as u64
}

/// Allocates `len` bytes for the host to fill (leaked; host-owned).
#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: u32) -> *mut u8 {
    Box::leak(vec![0u8; len as usize].into_boxed_slice()).as_mut_ptr()
}

/// Prints the UTF-8 string at `ptr..ptr+len` on stdout.
///
/// # Safety
/// `ptr..ptr+len` must be valid memory, e.g. from [`alloc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn say(ptr: *const u8, len: u32) {
    // SAFETY: caller contract.
    let s = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
    println!("{}", String::from_utf8_lossy(s));
}

/// Writes `data` to the file at `path` (both from [`alloc`]). Returns 0 on
/// success, otherwise the raw OS error (or -1).
///
/// # Safety
/// Both ranges must be valid memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn write_file(path: *const u8, path_len: u32, data: *const u8, len: u32) -> i32 {
    // SAFETY: caller contract.
    let (path, data) = unsafe {
        (std::slice::from_raw_parts(path, path_len as usize), std::slice::from_raw_parts(data, len as usize))
    };
    match std::fs::write(String::from_utf8_lossy(path).as_ref(), data) {
        Ok(()) => 0,
        Err(e) => e.raw_os_error().unwrap_or(-1),
    }
}

/// Reads the UTF-8 string at `ptr..ptr+len`.
///
/// # Safety
/// The range must be valid memory.
unsafe fn string(ptr: *const u8, len: u32) -> String {
    // SAFETY: caller contract.
    String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr, len as usize) }).into_owned()
}

fn code(r: std::io::Result<()>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => e.raw_os_error().unwrap_or(-1),
    }
}

fn fnv(bytes: &[u8]) -> i64 {
    let mut h = Fnv::new();
    h.eat(bytes);
    (h.0 >> 1) as i64
}

/// Appends `data` to the file at `path` (created if missing). Returns 0 or
/// the raw OS error.
///
/// # Safety
/// Both ranges must be valid memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn append_file(path: *const u8, path_len: u32, data: *const u8, len: u32) -> i32 {
    use std::io::Write;
    // SAFETY: caller contract.
    let (path, data) = unsafe { (string(path, path_len), std::slice::from_raw_parts(data, len as usize)) };
    code(std::fs::OpenOptions::new().append(true).create(true).open(path).and_then(|mut f| f.write_all(data)))
}

/// A hash of the file's contents (≥ 0), or minus the raw OS error.
///
/// # Safety
/// The range must be valid memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn file_hash(path: *const u8, len: u32) -> i64 {
    // SAFETY: caller contract.
    match std::fs::read(unsafe { string(path, len) }) {
        Ok(bytes) => fnv(&bytes),
        Err(e) => -(e.raw_os_error().unwrap_or(1) as i64),
    }
}

/// The file's length from its metadata, or minus the raw OS error.
///
/// # Safety
/// The range must be valid memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn file_len(path: *const u8, len: u32) -> i64 {
    // SAFETY: caller contract.
    match std::fs::metadata(unsafe { string(path, len) }) {
        Ok(m) => m.len() as i64,
        Err(e) => -(e.raw_os_error().unwrap_or(1) as i64),
    }
}

/// # Safety
/// The range must be valid memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fs_mkdir(path: *const u8, len: u32) -> i32 {
    // SAFETY: caller contract.
    code(std::fs::create_dir_all(unsafe { string(path, len) }))
}

/// Removes a file, or an empty directory.
///
/// # Safety
/// The range must be valid memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fs_remove(path: *const u8, len: u32) -> i32 {
    // SAFETY: caller contract.
    let path = unsafe { string(path, len) };
    match std::fs::metadata(&path) {
        Ok(m) if m.is_dir() => code(std::fs::remove_dir(&path)),
        _ => code(std::fs::remove_file(&path)),
    }
}

/// # Safety
/// Both ranges must be valid memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fs_rename(from: *const u8, from_len: u32, to: *const u8, to_len: u32) -> i32 {
    // SAFETY: caller contract.
    code(std::fs::rename(unsafe { string(from, from_len) }, unsafe { string(to, to_len) }))
}

/// Number of entries in a directory, or minus the raw OS error.
///
/// # Safety
/// The range must be valid memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn list_count(path: *const u8, len: u32) -> i64 {
    // SAFETY: caller contract.
    match std::fs::read_dir(unsafe { string(path, len) }) {
        Ok(entries) => entries.count() as i64,
        Err(e) => -(e.raw_os_error().unwrap_or(1) as i64),
    }
}

/// Opens a file and keeps it open; returns its handle index or minus the raw
/// OS error.
///
/// # Safety
/// The range must be valid memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hold_open(path: *const u8, len: u32) -> i32 {
    // SAFETY: caller contract.
    match std::fs::File::open(unsafe { string(path, len) }) {
        Ok(f) => with(|s| {
            s.held.push(f);
            s.held.len() as i32 - 1
        }),
        Err(e) => -e.raw_os_error().unwrap_or(1),
    }
}

/// A hash of a held file's contents, read from its start.
#[unsafe(no_mangle)]
pub extern "C" fn held_hash(handle: u32) -> i64 {
    use std::io::{Read, Seek};
    with(|s| {
        let f = &mut s.held[handle as usize];
        let mut bytes = Vec::new();
        match f.rewind().and_then(|()| f.read_to_end(&mut bytes)) {
            Ok(_) => fnv(&bytes),
            Err(e) => -(e.raw_os_error().unwrap_or(1) as i64),
        }
    })
}

/// A hash of the next `n` bytes read from a held file (advancing its offset).
#[unsafe(no_mangle)]
pub extern "C" fn held_read(handle: u32, n: u32) -> i64 {
    use std::io::Read;
    with(|s| {
        let mut bytes = vec![0; n as usize];
        match s.held[handle as usize].read(&mut bytes) {
            Ok(got) => fnv(&bytes[..got]),
            Err(e) => -(e.raw_os_error().unwrap_or(1) as i64),
        }
    })
}

/// Closes every held file.
#[unsafe(no_mangle)]
pub extern "C" fn release_all() {
    with(|s| s.held.clear());
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    fn eat(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
    }
}

mod wasi {
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    unsafe extern "C" {
        pub fn random_get(buf: *mut u8, len: usize) -> u16;
    }
}
