//! Baseline B: forking the host process (SPEC §5.3).

mod common;

use common::{kv_wasm, runtime, strategies};
use forkasm::{SpawnOpts, Strategy, Val};

#[test]
fn process_fork_runs_guest_in_child_and_leaves_parent_alone() {
    for strategy in strategies() {
        let rt = runtime(strategy);
        let module = rt.load(kv_wasm()).unwrap();
        let mut sb = rt.spawn(&module, SpawnOpts::default()).unwrap();
        sb.call("init", &[]).unwrap();
        sb.call("put", &[Val::I64(1), Val::I64(7)]).unwrap();
        sb.call("buf_init", &[Val::I32(64)]).unwrap();
        if strategy == Strategy::Lazy {
            // Unfilled pages would read as zeros in the child.
            assert!(sb.process_fork_call("get", &[Val::I64(1)]).is_err());
            continue;
        }
        let before = sb.memory();
        let r = sb.process_fork_call("get", &[Val::I64(1)]).unwrap();
        assert_eq!(r.value, 7, "{strategy:?}");
        assert!(r.fork <= r.ready && r.ready <= r.result);
        // The child writes 64 pages; the parent never sees them.
        let r = sb.process_fork_call("touch_pages", &[Val::I32(0), Val::I32(64), Val::I32(9)]).unwrap();
        assert_ne!(r.value, u64::MAX);
        assert!(sb.memory() == before, "child writes leaked into the parent");
        // Still a working sandbox, and still forkable the cheap way.
        let mut kid = rt.spawn_from(&sb.snapshot().unwrap(), 0).unwrap();
        assert_eq!(kid.call_u64("get", &[Val::I64(1)]).unwrap(), 7);
    }
}
