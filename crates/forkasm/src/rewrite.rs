//! Build-time module rewrite (SPEC §5.1).
//!
//! Makes a module's whole mutable state reachable from the host:
//! - every mutable global is exported as `__forkasm_g{N}` (N = global index);
//! - active data segments are removed and returned as a [`MemImage`], which
//!   the host bakes into the base layer once (Wasmtime would otherwise rewrite
//!   them into memory on every spawn, clobbering the snapshot);
//! - a `start` function is removed and exported as [`START_EXPORT`] so the
//!   host runs it only for root sandboxes.
//!
//! Modules whose state could escape a snapshot are rejected: imported or
//! multiple memories, imported mutable globals, reference-typed mutable
//! globals, passive data segments, and any instruction that mutates a table.

use walrus::ir::{Instr, InstrLocId, Value, Visitor, dfs_in_order};
use walrus::{ConstExpr, DataKind, ExportItem, GlobalKind, MemoryId, Module, ValType};

/// Prefix of the exports the rewrite adds for mutable globals.
pub const GLOBAL_EXPORT_PREFIX: &str = "__forkasm_g";
/// Export name of the module's former `start` function.
pub const START_EXPORT: &str = "__forkasm_start";

const WASM_PAGE: u64 = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum RewriteError {
    #[error("invalid module: {0}")]
    Parse(String),
    #[error("unsupported module: {0}")]
    Unsupported(String),
}

fn unsupported<T>(msg: impl Into<String>) -> Result<T, RewriteError> {
    Err(RewriteError::Unsupported(msg.into()))
}

/// The initial contents of the module's memory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemImage {
    /// Initial memory size in bytes (the declared minimum).
    pub min_bytes: u64,
    /// `(offset, bytes)` pairs, applied in order.
    pub segments: Vec<(u64, Vec<u8>)>,
}

/// A rewritten module and what the host needs to know about it.
#[derive(Debug)]
pub struct Rewritten {
    pub wasm: Vec<u8>,
    pub image: MemImage,
    /// Export names of the mutable globals, in global-index order.
    pub globals: Vec<String>,
    /// Whether the module had a `start` function, now exported as
    /// [`START_EXPORT`].
    pub has_start: bool,
}

pub fn rewrite(wasm: &[u8]) -> Result<Rewritten, RewriteError> {
    let mut module = Module::from_buffer(wasm).map_err(|e| RewriteError::Parse(e.to_string()))?;

    let memory = check_memory(&module)?;
    let image = extract_data(&mut module, memory)?;
    let globals = export_globals(&mut module)?;
    check_tables(&module)?;
    let has_start = match module.start.take() {
        Some(f) => {
            module.exports.add(START_EXPORT, f);
            true
        }
        None => false,
    };

    Ok(Rewritten { wasm: module.emit_wasm(), image, globals, has_start })
}

fn check_memory(module: &Module) -> Result<MemoryId, RewriteError> {
    let mems: Vec<_> = module.memories.iter().collect();
    let [mem] = mems[..] else {
        return unsupported(format!("expected exactly one memory, found {}", mems.len()));
    };
    if mem.import.is_some() {
        return unsupported("imported memory");
    }
    if mem.memory64 || mem.shared {
        return unsupported("64-bit or shared memory");
    }
    Ok(mem.id())
}

fn extract_data(module: &mut Module, memory: MemoryId) -> Result<MemImage, RewriteError> {
    let min_bytes = module.memories.get(memory).initial * WASM_PAGE;
    let mut image = MemImage { min_bytes, segments: Vec::new() };
    let ids: Vec<_> = module.data.iter().map(|d| d.id()).collect();
    for id in ids {
        let data = module.data.get(id);
        let offset = match &data.kind {
            DataKind::Passive => return unsupported("passive data segment"),
            DataKind::Active { offset: ConstExpr::Value(Value::I32(off)), .. } => *off as u32 as u64,
            DataKind::Active { .. } => return unsupported("data segment with non-constant offset"),
        };
        if offset + data.value.len() as u64 > min_bytes {
            return unsupported("data segment out of bounds");
        }
        image.segments.push((offset, data.value.clone()));
        module.data.delete(id);
        module.memories.get_mut(memory).data_segments.remove(&id);
    }
    Ok(image)
}

fn export_globals(module: &mut Module) -> Result<Vec<String>, RewriteError> {
    let mut names = Vec::new();
    let mutable: Vec<_> = module
        .globals
        .iter()
        .enumerate()
        .filter(|(_, g)| g.mutable)
        .map(|(i, g)| (i, g.id(), g.ty, matches!(g.kind, GlobalKind::Import(_))))
        .collect();
    for (index, id, ty, imported) in mutable {
        if imported {
            return unsupported(format!("imported mutable global {index}"));
        }
        if !matches!(ty, ValType::I32 | ValType::I64 | ValType::F32 | ValType::F64 | ValType::V128) {
            return unsupported(format!("mutable global {index} of reference type"));
        }
        let name = format!("{GLOBAL_EXPORT_PREFIX}{index}");
        module.exports.add(&name, ExportItem::Global(id));
        names.push(name);
    }
    Ok(names)
}

fn check_tables(module: &Module) -> Result<(), RewriteError> {
    if module.tables.iter().any(|t| t.import.is_some()) {
        return unsupported("imported table");
    }
    for (_, func) in module.funcs.iter_local() {
        let mut v = TableMutation(None);
        dfs_in_order(&mut v, func, func.entry_block());
        if let Some(op) = v.0 {
            return unsupported(format!("instruction {op} (tables and passive data must stay immutable)"));
        }
    }
    Ok(())
}

/// Records the first instruction that mutates a table or uses passive data.
struct TableMutation(Option<&'static str>);

impl<'i> Visitor<'i> for TableMutation {
    fn visit_instr(&mut self, instr: &'i Instr, _: &'i InstrLocId) {
        let op = match instr {
            Instr::TableSet(_) => "table.set",
            Instr::TableGrow(_) => "table.grow",
            Instr::TableFill(_) => "table.fill",
            Instr::TableCopy(_) => "table.copy",
            Instr::TableInit(_) => "table.init",
            Instr::ElemDrop(_) => "elem.drop",
            Instr::MemoryInit(_) => "memory.init",
            Instr::DataDrop(_) => "data.drop",
            _ => return,
        };
        self.0.get_or_insert(op);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rw(wat: &str) -> Result<Rewritten, RewriteError> {
        rewrite(&wat::parse_str(wat).unwrap())
    }

    fn exports(wasm: &[u8]) -> Vec<String> {
        let m = Module::from_buffer(wasm).unwrap();
        m.exports.iter().map(|e| e.name.clone()).collect()
    }

    #[test]
    fn exports_mutable_globals_and_strips_data() {
        let r = rw(r#"(module
            (memory 2)
            (global $sp (mut i32) (i32.const 1024))
            (global $k i32 (i32.const 7))
            (global $x (mut i64) (i64.const 0))
            (data (i32.const 16) "abc")
            (data (i32.const 70000) "z")
            (func $s) (start $s))"#)
        .unwrap();
        assert_eq!(r.globals, ["__forkasm_g0", "__forkasm_g2"]);
        assert_eq!(
            r.image,
            MemImage {
                min_bytes: 2 * WASM_PAGE,
                segments: vec![(16, b"abc".to_vec()), (70000, b"z".to_vec())]
            }
        );
        assert!(r.has_start);
        let m = Module::from_buffer(&r.wasm).unwrap();
        assert_eq!(m.data.iter().count(), 0);
        assert!(m.start.is_none());
        let mut ex = exports(&r.wasm);
        ex.sort();
        assert_eq!(ex, ["__forkasm_g0", "__forkasm_g2", START_EXPORT]);
        // The rewritten module still validates.
        wasmtime::Module::validate(&wasmtime::Engine::default(), &r.wasm).unwrap();
    }

    #[test]
    fn rejects_unsupported_modules() {
        let cases = [
            ("(module)", "exactly one memory"),
            ("(module (memory 1) (memory 1))", "exactly one memory"),
            (r#"(module (import "e" "m" (memory 1)))"#, "imported memory"),
            ("(module (memory 1) (data \"x\"))", "passive data"),
            (r#"(module (import "e" "g" (global (mut i32))) (memory 1))"#, "imported mutable global"),
            ("(module (memory 1) (global (mut funcref) (ref.null func)))", "reference type"),
            ("(module (memory 1) (data (i32.const 65535) \"xy\"))", "out of bounds"),
            (
                "(module (memory 1) (table 1 funcref) (func (table.set 0 (i32.const 0) (ref.null func))))",
                "table.set",
            ),
            (
                "(module (memory 1) (table 1 funcref) (func (drop (table.grow 0 (ref.null func) (i32.const 1)))))",
                "table.grow",
            ),
        ];
        for (wat, want) in cases {
            let err = rw(wat).unwrap_err().to_string();
            assert!(err.contains(want), "{wat}: {err}");
        }
    }

    #[test]
    fn immutable_tables_are_fine() {
        let r = rw(r#"(module (memory 1) (table 1 funcref) (elem (i32.const 0) $f)
            (func $f) (func (call_indirect (i32.const 0))))"#);
        assert!(r.is_ok());
    }
}
