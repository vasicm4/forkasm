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

use std::ops::Range;

use rayon::prelude::*;
use wasm_encoder::Encode;
use wasmparser::{
    BinaryReader, DataKind, Encoding, FunctionBody, Operator, Parser, Payload, TypeRef, ValType,
};

/// Prefix of the exports the rewrite adds for mutable globals.
pub const GLOBAL_EXPORT_PREFIX: &str = "__forkasm_g";
/// Export name of the module's former `start` function.
pub const START_EXPORT: &str = "__forkasm_start";

const WASM_PAGE: u64 = 64 * 1024;

const EXPORT_SECTION: u8 = 7;
const START_SECTION: u8 = 8;
const ELEMENT_SECTION: u8 = 9;
const CODE_SECTION: u8 = 10;
const DATA_SECTION: u8 = 11;
const DATA_COUNT_SECTION: u8 = 12;

/// Why a module could not be rewritten.
#[derive(Debug, thiserror::Error)]
pub enum RewriteError {
    /// The module is not valid WebAssembly.
    #[error("invalid module: {0}")]
    Parse(String),
    /// The module is valid but uses a feature forkasm cannot snapshot.
    #[error("unsupported module: {0}")]
    Unsupported(String),
}

impl From<wasmparser::BinaryReaderError> for RewriteError {
    fn from(e: wasmparser::BinaryReaderError) -> Self {
        RewriteError::Parse(e.to_string())
    }
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
    /// The rewritten module.
    pub wasm: Vec<u8>,
    /// The memory contents its data segments described.
    pub image: MemImage,
    /// Export names of the mutable globals, in global-index order.
    pub globals: Vec<String>,
    /// Whether the module had a `start` function, now exported as
    /// [`START_EXPORT`].
    pub has_start: bool,
}

/// A section of the input, kept in order for re-emitting.
enum Section<'a> {
    Raw(u8, Range<usize>),
    Custom(&'a str, &'a [u8]),
}

/// What the first pass learns about a module.
#[derive(Default)]
struct Scan<'a> {
    sections: Vec<Section<'a>>,
    /// Contents of the export section, if there is one.
    exports: Option<&'a [u8]>,
    /// `(global index, export name)` of each mutable global.
    globals: Vec<(u32, String)>,
    start: Option<u32>,
    memories: Vec<wasmparser::MemoryType>,
    segments: Vec<(u64, &'a [u8])>,
    bodies: Vec<FunctionBody<'a>>,
}

/// Rewrites a module at the section level: every section but export, start,
/// data count and data is copied verbatim, so the cost is one parse, not a
/// rebuild of every function (CPython's 19 MB module takes ~50 ms).
pub fn rewrite(wasm: &[u8]) -> Result<Rewritten, RewriteError> {
    let scan = scan(wasm)?;

    let [mem] = scan.memories[..] else {
        return unsupported(format!("expected exactly one memory, found {}", scan.memories.len()));
    };
    if mem.memory64 || mem.shared {
        return unsupported("64-bit or shared memory");
    }
    let min_bytes = mem.initial * WASM_PAGE;
    let mut image = MemImage { min_bytes, segments: Vec::with_capacity(scan.segments.len()) };
    for &(offset, bytes) in &scan.segments {
        if offset + bytes.len() as u64 > min_bytes {
            return unsupported("data segment out of bounds");
        }
        image.segments.push((offset, bytes.to_vec()));
    }
    // Deterministic: report the first offending instruction in code order.
    if let Some(op) = scan.bodies.par_iter().map(table_mutation).find_map_first(|r| r.transpose()) {
        return unsupported(format!("instruction {} (tables and passive data must stay immutable)", op?));
    }

    let wasm = emit(wasm, &scan)?;
    let globals = scan.globals.into_iter().map(|(_, name)| name).collect();
    Ok(Rewritten { wasm, image, globals, has_start: scan.start.is_some() })
}

fn scan(wasm: &[u8]) -> Result<Scan<'_>, RewriteError> {
    let mut s = Scan::default();
    let mut imported_globals = 0u32;
    for payload in Parser::new(0).parse_all(wasm) {
        let payload = payload?;
        match &payload {
            Payload::Version { encoding: Encoding::Component, .. } => return unsupported("component"),
            Payload::ImportSection(r) => {
                for import in r.clone().into_imports() {
                    match import?.ty {
                        TypeRef::Global(g) if g.mutable => {
                            return unsupported(format!("imported mutable global {imported_globals}"));
                        }
                        TypeRef::Global(_) => imported_globals += 1,
                        TypeRef::Memory(_) => return unsupported("imported memory"),
                        TypeRef::Table(_) => return unsupported("imported table"),
                        _ => {}
                    }
                }
            }
            Payload::MemorySection(r) => {
                for m in r.clone() {
                    s.memories.push(m?);
                }
            }
            Payload::GlobalSection(r) => {
                for (i, g) in r.clone().into_iter().enumerate() {
                    let ty = g?.ty;
                    if !ty.mutable {
                        continue;
                    }
                    let index = imported_globals + i as u32;
                    if !matches!(
                        ty.content_type,
                        ValType::I32 | ValType::I64 | ValType::F32 | ValType::F64 | ValType::V128
                    ) {
                        return unsupported(format!("mutable global {index} of reference type"));
                    }
                    s.globals.push((index, format!("{GLOBAL_EXPORT_PREFIX}{index}")));
                }
            }
            Payload::ExportSection(r) => s.exports = Some(&wasm[r.range()]),
            Payload::StartSection { func, .. } => s.start = Some(*func),
            Payload::DataSection(r) => {
                for d in r.clone() {
                    let d = d?;
                    let DataKind::Active { offset_expr, .. } = d.kind else {
                        return unsupported("passive data segment");
                    };
                    let mut ops = offset_expr.get_operators_reader();
                    let offset = match (ops.read()?, ops.read()?) {
                        (Operator::I32Const { value }, Operator::End) => value as u32 as u64,
                        _ => return unsupported("data segment with non-constant offset"),
                    };
                    s.segments.push((offset, d.data));
                }
            }
            Payload::CodeSectionEntry(body) => s.bodies.push(body.clone()),
            _ => {}
        }
        match &payload {
            Payload::CustomSection(c) => s.sections.push(Section::Custom(c.name(), c.data())),
            p => {
                if let Some((id, range)) = p.as_section() {
                    s.sections.push(Section::Raw(id, range));
                }
            }
        }
    }
    Ok(s)
}

/// The first instruction in `body` that mutates a table or uses passive data.
fn table_mutation(body: &FunctionBody<'_>) -> Result<Option<&'static str>, RewriteError> {
    let mut ops = body.get_operators_reader()?;
    while !ops.eof() {
        let op = match ops.read()? {
            Operator::TableSet { .. } => "table.set",
            Operator::TableGrow { .. } => "table.grow",
            Operator::TableFill { .. } => "table.fill",
            Operator::TableCopy { .. } => "table.copy",
            Operator::TableInit { .. } => "table.init",
            Operator::ElemDrop { .. } => "elem.drop",
            Operator::MemoryInit { .. } => "memory.init",
            Operator::DataDrop { .. } => "data.drop",
            _ => continue,
        };
        return Ok(Some(op));
    }
    Ok(None)
}

fn emit(wasm: &[u8], scan: &Scan<'_>) -> Result<Vec<u8>, RewriteError> {
    let exports = export_section(scan)?;
    let mut out = wasm_encoder::Module::new();
    let mut exports_written = false;
    for section in &scan.sections {
        match section {
            Section::Custom(name, data) => {
                out.section(&wasm_encoder::CustomSection { name: (*name).into(), data: (*data).into() });
            }
            &Section::Raw(id, ref range) => {
                // The export section goes where it was, or where it belongs
                // if the module had none.
                if !exports_written
                    && matches!(
                        id,
                        EXPORT_SECTION
                            | START_SECTION
                            | ELEMENT_SECTION
                            | DATA_COUNT_SECTION
                            | CODE_SECTION
                            | DATA_SECTION
                    )
                {
                    if let Some(e) = &exports {
                        out.section(&wasm_encoder::RawSection { id: EXPORT_SECTION, data: e });
                    }
                    exports_written = true;
                }
                if !matches!(id, EXPORT_SECTION | START_SECTION | DATA_COUNT_SECTION | DATA_SECTION) {
                    out.section(&wasm_encoder::RawSection { id, data: &wasm[range.clone()] });
                }
            }
        }
    }
    if !exports_written && let Some(e) = &exports {
        out.section(&wasm_encoder::RawSection { id: EXPORT_SECTION, data: e });
    }
    Ok(out.finish())
}

/// The export section's contents: the original exports, copied verbatim,
/// then the added ones. `None` if there would be none.
fn export_section(scan: &Scan<'_>) -> Result<Option<Vec<u8>>, RewriteError> {
    let (count, entries) = match scan.exports {
        Some(data) => {
            let mut r = BinaryReader::new(data, 0);
            let count = r.read_var_u32()?;
            (count, &data[r.current_position()..])
        }
        None => (0, &[][..]),
    };
    let added: Vec<(&str, wasm_encoder::ExportKind, u32)> = scan
        .globals
        .iter()
        .map(|(i, name)| (name.as_str(), wasm_encoder::ExportKind::Global, *i))
        .chain(scan.start.map(|f| (START_EXPORT, wasm_encoder::ExportKind::Func, f)))
        .collect();
    if scan.exports.is_none() && added.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(entries.len() + 32 * added.len() + 5);
    (count + added.len() as u32).encode(&mut out);
    out.extend_from_slice(entries);
    for (name, kind, index) in added {
        name.encode(&mut out);
        kind.encode(&mut out);
        index.encode(&mut out);
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rw(wat: &str) -> Result<Rewritten, RewriteError> {
        rewrite(&wat::parse_str(wat).unwrap())
    }

    fn exports(wasm: &[u8]) -> Vec<String> {
        let mut names = Vec::new();
        for p in Parser::new(0).parse_all(wasm) {
            if let Payload::ExportSection(r) = p.unwrap() {
                names.extend(r.into_iter().map(|e| e.unwrap().name.to_owned()));
            }
        }
        names
    }

    /// Payload kinds the rewritten module still has.
    fn has(wasm: &[u8], pred: fn(&Payload<'_>) -> bool) -> bool {
        Parser::new(0).parse_all(wasm).any(|p| pred(&p.unwrap()))
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
        assert!(!has(&r.wasm, |p| matches!(p, Payload::DataSection(_) | Payload::DataCountSection { .. })));
        assert!(!has(&r.wasm, |p| matches!(p, Payload::StartSection { .. })));
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
