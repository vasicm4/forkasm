//! G2: CPython forking between statements (SPEC §6), checked under every
//! strategy and against S0.

mod common;

use common::{assert_same, layered_configs, py_root, runtime, strategies};
use forkasm::python::{exec, get_int};
use forkasm::{Config, Runtime, Strategy};

fn out(sb: &forkasm::Sandbox) -> String {
    String::from_utf8_lossy(&sb.stdout()).into_owned()
}

#[test]
fn forks_between_statements() {
    for strategy in strategies() {
        let rt = runtime(strategy);
        let mut root = py_root(&rt);
        exec(&mut root, "x = 41\nprint('hello', x)").unwrap();
        assert_eq!(out(&root), "hello 41\n");
        let snap = root.snapshot().unwrap();
        let mut kids = rt.spawn_many(&snap, 2).unwrap();
        exec(&mut kids[0], "x += 1").unwrap();
        exec(&mut kids[1], "x *= 2\nprint('kid', x)").unwrap();
        exec(&mut root, "x = 0").unwrap();
        assert_eq!(get_int(&mut kids[0], "x").unwrap(), Some(42), "{strategy:?}");
        assert_eq!(get_int(&mut kids[1], "x").unwrap(), Some(82));
        assert_eq!(get_int(&mut root, "x").unwrap(), Some(0));
        assert_eq!(out(&kids[0]), "hello 41\n");
        assert_eq!(out(&kids[1]), "hello 41\nkid 82\n");
    }
}

#[test]
fn exceptions_are_reported_and_the_interpreter_continues() {
    let rt = runtime(Strategy::Layered);
    let mut root = py_root(&rt);
    let err = exec(&mut root, "1 / 0").unwrap_err().to_string();
    assert!(err.contains("ZeroDivisionError"), "{err}");
    exec(&mut root, "y = 5").unwrap();
    let mut kid = rt.spawn_from(&root.snapshot().unwrap(), 0).unwrap();
    assert_eq!(get_int(&mut kid, "y").unwrap(), Some(5));
    assert_eq!(get_int(&mut kid, "nope").unwrap(), None);
}

#[test]
fn stdlib_imports_and_files() {
    for strategy in strategies() {
        let rt = runtime(strategy);
        let mut root = py_root(&rt);
        exec(
            &mut root,
            "import json, os, hashlib\n\
             os.makedirs('/work', exist_ok=True)\n\
             open('/work/a.json', 'w').write(json.dumps({'n': 1}))",
        )
        .unwrap();
        let snap = root.snapshot().unwrap();
        let mut kid = rt.spawn_from(&snap, 0).unwrap();
        exec(
            &mut kid,
            "d = json.load(open('/work/a.json'))\n\
             d['n'] += 1\n\
             open('/work/a.json', 'w').write(json.dumps(d))\n\
             n = d['n']",
        )
        .unwrap();
        assert_eq!(get_int(&mut kid, "n").unwrap(), Some(2));
        assert_eq!(kid.read_file("/work/a.json").unwrap(), br#"{"n": 2}"#, "{strategy:?}");
        assert_eq!(root.read_file("/work/a.json").unwrap(), br#"{"n": 1}"#);
        let err = exec(&mut kid, "open('/usr/local/lib/os.py', 'w')").unwrap_err().to_string();
        assert!(err.contains("Read-only file system"), "{err}");
    }
}

#[test]
fn branches_replay_deterministically() {
    let rt = runtime(Strategy::Layered);
    let mut root = py_root(&rt);
    exec(&mut root, "import os, random, time").unwrap();
    let snap = root.snapshot().unwrap();
    let run = |branch, src| {
        let mut kid = rt.spawn_from(&snap, branch).unwrap();
        exec(&mut kid, src).unwrap();
        (out(&kid), kid.memory())
    };
    // `random` was seeded at import, before the fork, so every branch
    // continues the same sequence (like os.fork); `os.urandom` reads the
    // per-branch WASI RNG, and so does reseeding after the fork.
    let fresh = "print(random.random(), os.urandom(8).hex(), time.time())";
    let (a, mem_a) = run(3, fresh);
    let (b, mem_b) = run(3, fresh);
    let (c, _) = run(4, fresh);
    assert_eq!(a, b);
    assert!(mem_a == mem_b, "same branch, different memory");
    let (a_rand, a_urandom) = a.split_at(a.find(' ').unwrap());
    let (c_rand, c_urandom) = c.split_at(c.find(' ').unwrap());
    assert_eq!(a_rand, c_rand, "inherited random state");
    assert_ne!(a_urandom, c_urandom, "different branches should read different urandom bytes");
    let reseeded = "random.seed()\nprint(random.random())";
    assert_ne!(run(3, reseeded).0, run(4, reseeded).0);
}

/// The same statements and forks under S0 and every S1 variant leave
/// byte-identical sandboxes.
#[test]
fn matches_s0() {
    let program = [
        "import collections, re",
        "words = 'the quick brown fox jumps over the lazy dog the end'.split()",
        "c = collections.Counter(words)",
        "big = [i * i for i in range(20000)]",
        "s = sum(big) % 1000003",
        "del big",
        "print(c.most_common(2), s)",
        "class P:\n    def __init__(self, v): self.v = v\nps = [P(i) for i in range(500)]",
        "t = re.sub(r'o', '0', ' '.join(words))",
    ];
    let mut configs = vec![Config { strategy: Strategy::Copy, ..Config::default() }];
    configs.extend(layered_configs());
    let runtimes: Vec<Runtime> = configs.into_iter().map(|c| Runtime::new(c).unwrap()).collect();
    let mut sbs: Vec<_> = runtimes.iter().map(py_root).collect();
    for (i, stmt) in program.iter().enumerate() {
        for (sb, rt) in sbs.iter_mut().zip(&runtimes) {
            exec(sb, stmt).unwrap();
            // Fork every other statement and continue in the child.
            if i % 2 == 1 {
                let snap = sb.snapshot().unwrap();
                *sb = rt.spawn_from(&snap, i as u64).unwrap();
            }
        }
        let (s0, rest) = sbs.split_first_mut().unwrap();
        for sb in rest {
            assert_same(s0, sb, &format!("after statement {i}"));
        }
    }
}
