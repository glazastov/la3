//! Phase 1.3 — by-value type layout.
//!
//! Split out of `typeck.rs`: the size/alignment a value of each type occupies
//! when laid out by value (what the back-end emits), plus the `la3 layout`
//! dump. As a child module it can use `TypeChecker`'s private tables.

use std::collections::{HashMap, HashSet};

use crate::diag::Diagnostic;

use super::*;

impl TypeChecker {
    // -----------------------------------------------------------------------
    // Layout (Phase 1.3)
    //
    // The size/alignment a value of a type occupies when laid out *by value*,
    // as the back-end will. Heap-owning handles (str, List, Map, Set) and
    // borrows are a single pointer; a slice is a (ptr, len) fat pointer.
    // Aggregates (tuple, struct, fixed array) use C-style layout: fields in
    // declaration order, each at its natural alignment, the whole rounded up to
    // its own alignment. Enums are tagged unions — a 1-byte discriminant (more
    // for >256 variants) followed by the largest variant payload.
    // -----------------------------------------------------------------------

    /// Size and alignment, in bytes. `None` for a type whose size is not known
    /// here (a generic `Param`, `Unknown`, or an unsized array).
    fn size_align(&self, t: &Ty) -> Option<(u64, u64)> {
        use IntKind::*;
        Some(match t {
            Ty::Bool => (1, 1),
            Ty::Int(I8 | U8) => (1, 1),
            Ty::Int(I16 | U16) => (2, 2),
            Ty::Int(I32 | U32) => (4, 4),
            Ty::Int(I64 | U64 | Isize | Usize) => (8, 8),
            Ty::IntLit => (4, 4), // defaults to i32
            Ty::Float(FloatKind::F32) => (4, 4),
            Ty::Float(FloatKind::F64) => (8, 8),
            Ty::FloatLit => (8, 8), // defaults to f64
            Ty::Char => (4, 4),     // a Unicode scalar value
            Ty::Unit | Ty::Never | Ty::Nil => (0, 1),
            // Heap-owning handles, borrows, raw pointers, futures and bare fn
            // pointers are all a single machine pointer.
            Ty::Str
            | Ty::List(_)
            | Ty::Map(..)
            | Ty::Set(_)
            | Ty::Ref(_)
            | Ty::Ptr(_)
            | Ty::Future(_)
            | Ty::Fn(..) => (8, 8),
            // A slice is a (ptr, len) fat pointer.
            Ty::Slice(_) => (16, 8),
            Ty::Array(elem, Some(n)) => {
                let (es, ea) = self.size_align(elem)?;
                (stride(es, ea) * (*n as u64), ea)
            }
            Ty::Array(_, None) => return None,
            // { start: i64, end: i64, inclusive: bool }
            Ty::Range(_) => {
                let (_, s, a) = self.aggregate_sa(&[Ty::Int(I64), Ty::Int(I64), Ty::Bool])?;
                (s, a)
            }
            Ty::Tuple(ts) => {
                let (_, s, a) = self.aggregate_sa(ts)?;
                (s, a)
            }
            Ty::Struct(name, args) => {
                let (_, s, a) = self.struct_field_layout(name, args)?;
                (s, a)
            }
            Ty::Enum(name, args) => {
                let info = self.enum_layout_info(name, args)?;
                (info.size, info.align)
            }
            Ty::Union(members) => self.union_sa(members)?,
            Ty::Param(_) | Ty::Unknown => return None,
        })
    }

    /// Offsets, total size and alignment of a sequence of fields laid out
    /// C-style. Used by tuples, structs, and enum-variant payloads.
    fn aggregate_sa(&self, fields: &[Ty]) -> Option<(Vec<u64>, u64, u64)> {
        let mut offset = 0u64;
        let mut align = 1u64;
        let mut offsets = Vec::with_capacity(fields.len());
        for f in fields {
            let (fs, fa) = self.size_align(f)?;
            offset = align_up(offset, fa);
            offsets.push(offset);
            offset += fs;
            align = align.max(fa);
        }
        Some((offsets, align_up(offset, align), align))
    }

    /// A struct's field types with its generic parameters substituted by `args`.
    fn struct_fields_resolved(&self, name: &str, args: &[Ty]) -> Option<Vec<(String, Ty)>> {
        let info = self.structs.get(name)?;
        let gens: HashSet<String> = info.generics.iter().cloned().collect();
        let bindings: HashMap<String, Ty> = info
            .generics
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        Some(
            info.fields
                .iter()
                .map(|(fname, fty)| {
                    let resolved = self.resolve_in(fty, &gens);
                    (fname.clone(), subst(&resolved, &bindings))
                })
                .collect(),
        )
    }

    /// `(field offsets, size, align)` for a struct.
    fn struct_field_layout(&self, name: &str, args: &[Ty]) -> Option<(Vec<u64>, u64, u64)> {
        let fields = self.struct_fields_resolved(name, args)?;
        let tys: Vec<Ty> = fields.into_iter().map(|(_, t)| t).collect();
        self.aggregate_sa(&tys)
    }

    /// A enum's variants with payload types resolved, including the built-in
    /// `Option<T>` and `Result<T>` whose variants are not declared in source.
    /// Each entry is `(variant name, [(optional field name, type)])`.
    #[allow(clippy::type_complexity)]
    fn enum_variants_resolved(
        &self,
        name: &str,
        args: &[Ty],
    ) -> Option<Vec<(String, Vec<(Option<String>, Ty)>)>> {
        if name == "Option" {
            let inner = args.first().cloned().unwrap_or(Ty::Unknown);
            return Some(vec![
                ("None".into(), vec![]),
                ("Some".into(), vec![(None, inner)]),
            ]);
        }
        if name == "Result" {
            let inner = args.first().cloned().unwrap_or(Ty::Unknown);
            return Some(vec![
                ("Ok".into(), vec![(None, inner)]),
                ("Err".into(), vec![(None, Ty::Str)]),
            ]);
        }
        let info = self.enums.get(name)?;
        let gens: HashSet<String> = info.generics.iter().cloned().collect();
        let bindings: HashMap<String, Ty> = info
            .generics
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        let resolve = |te: &TypeExpr| subst(&self.resolve_in(te, &gens), &bindings);
        Some(
            info.variants
                .iter()
                .map(|v| {
                    let payload = match &v.kind {
                        VariantKind::Unit => vec![],
                        VariantKind::Tuple(tys) => tys.iter().map(|t| (None, resolve(t))).collect(),
                        VariantKind::Struct(fs) => fs
                            .iter()
                            .map(|(n, t)| (Some(n.clone()), resolve(t)))
                            .collect(),
                    };
                    (v.name.clone(), payload)
                })
                .collect(),
        )
    }

    /// Does a value of this type own a heap resource that must be released by a
    /// `drop` (reference Section 11, deterministic destruction)? Heap-owning
    /// built-ins (`str`/`List`/`Map`/`Set`/futures) do; an aggregate does iff a
    /// field/element/variant-payload does. Scalars, references, raw pointers,
    /// slices (borrowed views), and `fn` do not. This is the front-end half of
    /// the drop contract — MIR ownership-lowering (Phase 3.5) consumes it to
    /// decide *where* to insert the drops the borrow check proved safe.
    pub(super) fn ty_needs_drop(&self, t: &Ty) -> bool {
        match t {
            Ty::Str | Ty::List(_) | Ty::Map(_, _) | Ty::Set(_) | Ty::Future(_) => true,
            Ty::Tuple(ts) => ts.iter().any(|x| self.ty_needs_drop(x)),
            Ty::Array(e, _) => self.ty_needs_drop(e),
            Ty::Struct(name, args) => self
                .struct_fields_resolved(name, args)
                .is_some_and(|fs| fs.iter().any(|(_, ft)| self.ty_needs_drop(ft))),
            Ty::Enum(name, args) => self.enum_variants_resolved(name, args).is_some_and(|vs| {
                vs.iter()
                    .any(|(_, payload)| payload.iter().any(|(_, ft)| self.ty_needs_drop(ft)))
            }),
            // Scalars, `nil`/`()`/`!`, `&T`/`*T`, slices (borrowed), `fn`,
            // generics, and unresolved types carry no owned heap.
            _ => false,
        }
    }

    /// Full tagged-union layout of an enum.
    fn enum_layout_info(&self, name: &str, args: &[Ty]) -> Option<EnumLayoutInfo> {
        let variants = self.enum_variants_resolved(name, args)?;
        let tag_size = tag_bytes(variants.len());
        let mut payload_size = 0u64;
        let mut payload_align = 1u64;
        let mut var_offsets: Vec<Vec<u64>> = Vec::with_capacity(variants.len());
        for (_, fields) in &variants {
            let tys: Vec<Ty> = fields.iter().map(|(_, t)| t.clone()).collect();
            let (offs, sz, al) = self.aggregate_sa(&tys)?;
            var_offsets.push(offs);
            payload_size = payload_size.max(sz);
            payload_align = payload_align.max(al);
        }
        let align = payload_align.max(tag_size); // tag is an integer of tag_size
        let payload_offset = align_up(tag_size, payload_align.max(1));
        let size = align_up(payload_offset + payload_size, align.max(1));
        Some(EnumLayoutInfo {
            size,
            align: align.max(1),
            tag_size,
            payload_offset,
            var_offsets,
        })
    }

    /// Size/align of a union type (`str | i64`, `T | nil`, ...). Laid out as a
    /// tagged union over its members; `nil` contributes a zero-size payload.
    fn union_sa(&self, members: &[Ty]) -> Option<(u64, u64)> {
        let mut payload_size = 0u64;
        let mut payload_align = 1u64;
        for m in members {
            if matches!(m, Ty::Nil) {
                continue;
            }
            let (s, a) = self.size_align(m)?;
            payload_size = payload_size.max(s);
            payload_align = payload_align.max(a);
        }
        let tag_size = tag_bytes(members.len());
        let align = payload_align.max(tag_size);
        let payload_offset = align_up(tag_size, payload_align.max(1));
        Some((
            align_up(payload_offset + payload_size, align.max(1)),
            align.max(1),
        ))
    }
}

/// Internal tagged-union layout, exposed to the dump via [`EnumLayout`].
struct EnumLayoutInfo {
    size: u64,
    align: u64,
    tag_size: u64,
    payload_offset: u64,
    /// Field offsets *within the payload* (add `payload_offset` for the offset
    /// from the start of the value) for each variant.
    var_offsets: Vec<Vec<u64>>,
}

/// Round `offset` up to the next multiple of `align` (a power of two ≥ 1).
fn align_up(offset: u64, align: u64) -> u64 {
    if align <= 1 {
        offset
    } else {
        offset.div_ceil(align) * align
    }
}

/// Per-element stride of an array: the element size rounded up to its align.
fn stride(size: u64, align: u64) -> u64 {
    align_up(size, align.max(1))
}

/// Bytes needed for an enum discriminant covering `n` variants.
fn tag_bytes(n: usize) -> u64 {
    if n <= 256 {
        1
    } else if n <= 65536 {
        2
    } else {
        4
    }
}

// ---------------------------------------------------------------------------
// Public layout view (Phase 1.3)
// ---------------------------------------------------------------------------

/// One field of an aggregate, with its byte offset and own size/align.
pub struct FieldLayout {
    pub name: String,
    pub ty: String,
    pub offset: u64,
    pub size: u64,
    pub align: u64,
}

pub struct StructLayout {
    pub name: String,
    pub size: u64,
    pub align: u64,
    pub fields: Vec<FieldLayout>,
    /// Whether the type owns a heap resource needing a `drop` (Phase 1.6.5).
    pub needs_drop: bool,
}

pub struct VariantLayout {
    pub name: String,
    pub fields: Vec<FieldLayout>,
}

pub struct EnumLayout {
    pub name: String,
    pub size: u64,
    pub align: u64,
    pub tag_size: u64,
    pub payload_offset: u64,
    pub variants: Vec<VariantLayout>,
    /// Whether the type owns a heap resource needing a `drop` (Phase 1.6.5).
    pub needs_drop: bool,
}

/// The computed layouts of a program's concrete (non-generic) types, plus any
/// generic declarations skipped (they have no single layout until monomorphized)
/// and the program's type-check diagnostics.
pub struct Layouts {
    pub structs: Vec<StructLayout>,
    pub enums: Vec<EnumLayout>,
    pub skipped: Vec<String>,
    pub errors: Vec<Diagnostic>,
}

impl Layouts {
    /// Human-readable dump for the `la3 layout` command.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        for s in &self.structs {
            out.push_str(&format!(
                "struct {}  size={} align={} drop={}\n",
                s.name,
                s.size,
                s.align,
                if s.needs_drop { "yes" } else { "no" }
            ));
            for f in &s.fields {
                out.push_str(&format!(
                    "    @{:<4} {}: {}  (size={} align={})\n",
                    f.offset, f.name, f.ty, f.size, f.align
                ));
            }
        }
        for e in &self.enums {
            out.push_str(&format!(
                "enum {}  size={} align={} tag={}B payload@{} drop={}\n",
                e.name,
                e.size,
                e.align,
                e.tag_size,
                e.payload_offset,
                if e.needs_drop { "yes" } else { "no" }
            ));
            for v in &e.variants {
                if v.fields.is_empty() {
                    out.push_str(&format!("    {} (unit)\n", v.name));
                } else {
                    out.push_str(&format!("    {}\n", v.name));
                    for f in &v.fields {
                        out.push_str(&format!(
                            "        @{:<4} {}: {}  (size={} align={})\n",
                            f.offset, f.name, f.ty, f.size, f.align
                        ));
                    }
                }
            }
        }
        for s in &self.skipped {
            out.push_str(&format!("(skipped {})\n", s));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Codegen layout oracle (Phase 5.4)
// ---------------------------------------------------------------------------
//
// The back-end lays aggregates out by value exactly as this module computes, so
// codegen queries the same machinery rather than re-deriving offsets. Unlike the
// `la3 layout` dump (which stringifies field types), this returns resolved `Ty`s
// with byte offsets, which is what the LLVM lowering needs.

/// Resolved tagged-union layout of an enum for codegen. (Whole-enum size/align
/// come from [`LayoutOracle::size_align`].)
pub struct EnumInfo {
    pub tag_size: u64,
    pub payload_offset: u64,
    /// Per variant, its payload fields as `(offset within the payload, type)`.
    pub variants: Vec<Vec<(u64, Ty)>>,
}

/// A view over a type-checked program that answers the by-value layout questions
/// codegen asks (sizes, field offsets, enum tag/payload geometry).
pub struct LayoutOracle {
    tc: TypeChecker,
}

/// Build the layout oracle by running the type checker's collection pass.
/// (Reached from the back-end, which `main` wires in from Phase 5.5/11.)
#[allow(dead_code)]
pub fn layout_oracle(prog: &Program) -> LayoutOracle {
    let mut tc = TypeChecker::new(prog);
    tc.run(prog);
    LayoutOracle { tc }
}

impl LayoutOracle {
    /// Size and alignment in bytes (`None` for unsized/unknown).
    pub fn size_align(&self, ty: &Ty) -> Option<(u64, u64)> {
        self.tc.size_align(ty)
    }

    /// `(offset, type)` for each field of a tuple or struct, in order.
    pub fn agg_fields(&self, ty: &Ty) -> Option<Vec<(u64, Ty)>> {
        let tys: Vec<Ty> = match ty {
            Ty::Tuple(ts) => ts.clone(),
            Ty::Struct(name, args) => self
                .tc
                .struct_fields_resolved(name, args)?
                .into_iter()
                .map(|(_, t)| t)
                .collect(),
            _ => return None,
        };
        let (offsets, _, _) = self.tc.aggregate_sa(&tys)?;
        Some(offsets.into_iter().zip(tys).collect())
    }

    /// The discriminant index of a (non-generic) enum's variant by name.
    pub fn variant_index(&self, enum_name: &str, variant: &str) -> Option<usize> {
        self.tc
            .enum_variants_resolved(enum_name, &[])?
            .iter()
            .position(|(n, _)| n == variant)
    }

    /// Full tagged-union geometry of an enum.
    pub fn enum_info(&self, ty: &Ty) -> Option<EnumInfo> {
        let (name, args) = match ty {
            Ty::Enum(n, a) => (n, a),
            _ => return None,
        };
        let info = self.tc.enum_layout_info(name, args)?;
        let resolved = self.tc.enum_variants_resolved(name, args)?;
        let variants = resolved
            .into_iter()
            .zip(info.var_offsets.iter())
            .map(|((_, payload), offs)| {
                payload
                    .into_iter()
                    .zip(offs.iter().cloned())
                    .map(|((_, t), o)| (o, t))
                    .collect()
            })
            .collect();
        Some(EnumInfo {
            tag_size: info.tag_size,
            payload_offset: info.payload_offset,
            variants,
        })
    }
}

/// Compute the by-value layout of every concrete struct and enum in `prog`.
/// Generic declarations are skipped (no single layout before monomorphization).
pub fn dump_layouts(prog: &Program) -> Layouts {
    let mut tc = TypeChecker::new(prog);
    tc.run(prog);
    let mut structs = Vec::new();
    let mut enums = Vec::new();
    let mut skipped = Vec::new();

    for item in &prog.items {
        match item {
            Item::Struct(s) if s.generics.is_empty() => {
                let fields = tc.struct_fields_resolved(&s.name, &[]).unwrap_or_default();
                let tys: Vec<Ty> = fields.iter().map(|(_, t)| t.clone()).collect();
                match tc.aggregate_sa(&tys) {
                    Some((offsets, size, align)) => {
                        let fields = fields
                            .iter()
                            .zip(offsets)
                            .map(|((name, ty), offset)| {
                                let (fs, fa) = tc.size_align(ty).unwrap_or((0, 1));
                                FieldLayout {
                                    name: name.clone(),
                                    ty: display_ty(ty),
                                    offset,
                                    size: fs,
                                    align: fa,
                                }
                            })
                            .collect();
                        let needs_drop = tc.ty_needs_drop(&Ty::Struct(s.name.clone(), Vec::new()));
                        structs.push(StructLayout {
                            name: s.name.clone(),
                            size,
                            align,
                            fields,
                            needs_drop,
                        });
                    }
                    None => skipped.push(format!("struct {} (unsized field)", s.name)),
                }
            }
            Item::Struct(s) => skipped.push(format!("struct {} (generic)", s.name)),
            Item::Enum(e) if e.generics.is_empty() => {
                match (
                    tc.enum_layout_info(&e.name, &[]),
                    tc.enum_variants_resolved(&e.name, &[]),
                ) {
                    (Some(info), Some(variants)) => {
                        let vlayouts = variants
                            .iter()
                            .zip(&info.var_offsets)
                            .map(|((vname, payload), offs)| {
                                let fields = payload
                                    .iter()
                                    .zip(offs)
                                    .enumerate()
                                    .map(|(i, ((fname, ty), off))| {
                                        let (fs, fa) = tc.size_align(ty).unwrap_or((0, 1));
                                        FieldLayout {
                                            name: fname.clone().unwrap_or_else(|| i.to_string()),
                                            ty: display_ty(ty),
                                            offset: info.payload_offset + off,
                                            size: fs,
                                            align: fa,
                                        }
                                    })
                                    .collect();
                                VariantLayout {
                                    name: vname.clone(),
                                    fields,
                                }
                            })
                            .collect();
                        let needs_drop = tc.ty_needs_drop(&Ty::Enum(e.name.clone(), Vec::new()));
                        enums.push(EnumLayout {
                            name: e.name.clone(),
                            size: info.size,
                            align: info.align,
                            tag_size: info.tag_size,
                            payload_offset: info.payload_offset,
                            variants: vlayouts,
                            needs_drop,
                        });
                    }
                    _ => skipped.push(format!("enum {} (unsized payload)", e.name)),
                }
            }
            Item::Enum(e) => skipped.push(format!("enum {} (generic)", e.name)),
            _ => {}
        }
    }

    Layouts {
        structs,
        enums,
        skipped,
        errors: tc.errors,
    }
}
