use crate::atoms;
use crate::native_decode::{
    self, builder_error_term, with_term_builder, Overflow, Source, Strings, TermBuilder,
    HEAP_BINARY_MAX,
};
use crate::nif_util::{make_tuple2, timeslice_percent, REDUCTION_COUNT};
use crate::types::{value_to_term, MAX_DEPTH};
use crate::{ParseBuffer, ParsedDocument};
use rustler::sys::{
    enif_make_double, enif_make_int64, enif_make_list_cell, enif_make_list_from_array,
    enif_make_uint64, ERL_NIF_TERM,
};
use rustler::{schedule, Binary, Encoder, Env, ListIterator, NifResult, ResourceArc, Term};
use sonic_rs::{JsonContainerTrait, JsonValueTrait};

const GET_MANY_STACK: usize = 64;

/// Below this many built terms the enif_consume_timeslice call costs more than
/// the accounting is worth — mirrors the encoder's TIMESLICE_MIN_BYTES guard,
/// so scalar extractions pay only this one branch.
const TIMESLICE_MIN_NODES: usize = 512;
/// Terms built per BEAM reduction. Calibrated for parity with the byte-based
/// accounting used by parse/decode/encode (20 bytes per reduction): typical
/// JSON runs ~5-10 source bytes per built term, so extracting a subtree
/// charges roughly what decoding the same content would. Long strings count
/// as one term, so string-heavy content charges less; both schemes are coarse
/// work proxies, not wall-clock estimates.
const NODES_PER_REDUCTION: usize = 4;

/// Report term-building work from the get family to the scheduler. The
/// pointer lookup itself is sub-microsecond, but building the result term
/// costs in proportion to the extracted subtree, which only the node counter
/// sees.
#[inline]
fn consume_timeslice_nodes(env: Env, nodes: usize) {
    if nodes >= TIMESLICE_MIN_NODES {
        let reds = nodes / NODES_PER_REDUCTION;
        schedule::consume_timeslice(env, ((reds * 100 / REDUCTION_COUNT) as i32).clamp(1, 100));
    }
}

/// Stack-first accumulator for per-path result terms: fills a fixed array,
/// spilling to a heap Vec only past `GET_MANY_STACK` entries — or immediately,
/// with exact capacity, when a larger size is known up front via `with_hint`.
struct TermAcc {
    stack: [ERL_NIF_TERM; GET_MANY_STACK],
    count: usize,
    heap: Option<Vec<ERL_NIF_TERM>>,
}

impl TermAcc {
    #[inline]
    fn new() -> Self {
        Self::with_hint(0)
    }

    #[inline]
    fn with_hint(n: usize) -> Self {
        TermAcc {
            stack: [0; GET_MANY_STACK],
            count: 0,
            heap: if n > GET_MANY_STACK {
                Some(Vec::with_capacity(n))
            } else {
                None
            },
        }
    }

    #[inline]
    fn push(&mut self, term: ERL_NIF_TERM) {
        if self.count < GET_MANY_STACK && self.heap.is_none() {
            self.stack[self.count] = term;
        } else {
            self.heap
                .get_or_insert_with(|| {
                    let mut v = Vec::with_capacity(GET_MANY_STACK * 2);
                    v.extend_from_slice(&self.stack[..self.count]);
                    v
                })
                .push(term);
        }
        self.count += 1;
    }

    #[inline]
    fn into_list<'a>(self, env: Env<'a>) -> Term<'a> {
        let terms = match &self.heap {
            Some(v) => v.as_slice(),
            None => &self.stack[..self.count],
        };
        unsafe {
            Term::new(
                env,
                enif_make_list_from_array(env.as_c_arg(), terms.as_ptr(), self.count as u32),
            )
        }
    }
}

/// Parses an RFC 6901 array index. Numeric tokens with leading zeroes remain
/// object keys so raw and compiled pointers resolve them identically.
#[inline]
fn array_index(token: &str) -> Option<usize> {
    let b = token.as_bytes();
    match b {
        [] => None,
        [b'0'] => Some(0),
        [first, ..] if first.is_ascii_digit() && *first != b'0' => token.parse().ok(),
        _ => None,
    }
}

/// Looks up `key` in an object.
///
/// When `unique_keys` is true, uses sonic-rs's internal index (fast).
/// Otherwise, does a reverse linear scan so that the last value wins,
/// matching the duplicate-key behaviour of both term converters' maps.
#[inline]
fn object_get<'v>(
    value: &'v sonic_rs::Value,
    key: &str,
    unique_keys: bool,
) -> Option<&'v sonic_rs::Value> {
    if unique_keys {
        value.get(key)
    } else {
        value.as_object()?.get_last(&key)
    }
}

/// RFC 6901 permits only `~0` and `~1`; any other `~` is malformed.
#[inline]
fn escapes_valid(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'~' {
            match bytes.get(i + 1) {
                Some(b'0') | Some(b'1') => i += 2,
                _ => return false,
            }
        } else {
            i += 1;
        }
    }
    true
}
#[inline]
fn pointer_lookup<'v>(
    value: &'v sonic_rs::Value,
    path: &str,
    unique_keys: bool,
) -> Option<&'v sonic_rs::Value> {
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        return Some(value);
    }
    if bytes[0] != b'/' {
        return None;
    }
    if bytes.len() == 1 {
        return Some(value);
    }

    let mut current = value;
    for segment in path[1..].split('/') {
        if current.is_array() {
            if let Some(index) = array_index(segment) {
                current = current.get(index)?;
                continue;
            }
        }
        if segment.contains('~') {
            if !escapes_valid(segment) {
                return None;
            }
            if segment.len() > 512 {
                let unescaped = segment.replace("~1", "/").replace("~0", "~");
                current = object_get(current, &unescaped, unique_keys)?;
            } else {
                let bytes = segment.as_bytes();
                let mut tmp = [0u8; 512];
                let mut out_len = 0usize;
                let mut i = 0usize;
                while i < bytes.len() {
                    if bytes[i] == b'~' && i + 1 < bytes.len() {
                        match bytes[i + 1] {
                            b'1' => {
                                tmp[out_len] = b'/';
                                out_len += 1;
                                i += 2;
                            }
                            b'0' => {
                                tmp[out_len] = b'~';
                                out_len += 1;
                                i += 2;
                            }
                            _ => {
                                tmp[out_len] = bytes[i];
                                out_len += 1;
                                i += 1;
                            }
                        }
                    } else {
                        tmp[out_len] = bytes[i];
                        out_len += 1;
                        i += 1;
                    }
                }
                // SAFETY: input is valid UTF-8 &str; substitutions write only ASCII bytes
                let unescaped = unsafe { std::str::from_utf8_unchecked(&tmp[..out_len]) };
                current = object_get(current, unescaped, unique_keys)?;
            }
        } else {
            current = object_get(current, segment, unique_keys)?;
        }
    }
    Some(current)
}

fn do_parse(
    json: &[u8],
    unique_keys: bool,
) -> Result<ResourceArc<ParsedDocument>, sonic_rs::Error> {
    let (value, buffer) = ParseBuffer::parse(json)?;
    Ok(ResourceArc::new(ParsedDocument {
        value,
        unique_keys,
        buffer,
    }))
}

/// Build the `{:error, _}` term for a parse failure. The vendored sonic-rs caps
/// nesting; surface that as `:nesting_too_deep` for parity with get/encode.
/// Other errors keep the sonic-rs message string.
#[inline]
pub(crate) fn parse_error_term<'a>(env: Env<'a>, err: &sonic_rs::Error) -> Term<'a> {
    let err_raw = atoms::error().as_c_arg();
    if err.is_recursion_limit() {
        make_tuple2(env, err_raw, atoms::nesting_too_deep().as_c_arg())
    } else {
        make_tuple2(env, err_raw, format!("{}", err).encode(env).as_c_arg())
    }
}

#[rustler::nif]
fn parse<'a>(env: Env<'a>, json: Binary) -> Term<'a> {
    match do_parse(json.as_slice(), false) {
        Ok(resource) => {
            schedule::consume_timeslice(env, timeslice_percent(json.len()));
            make_tuple2(env, atoms::ok().as_c_arg(), resource.encode(env).as_c_arg())
        }
        Err(e) => parse_error_term(env, &e),
    }
}

#[rustler::nif(schedule = "DirtyCpu")]
fn parse_dirty<'a>(env: Env<'a>, json: Binary) -> Term<'a> {
    match do_parse(json.as_slice(), false) {
        Ok(resource) => make_tuple2(env, atoms::ok().as_c_arg(), resource.encode(env).as_c_arg()),
        Err(e) => parse_error_term(env, &e),
    }
}

#[rustler::nif]
fn parse_opts<'a>(env: Env<'a>, json: Binary, unique_keys: bool) -> Term<'a> {
    match do_parse(json.as_slice(), unique_keys) {
        Ok(resource) => {
            schedule::consume_timeslice(env, timeslice_percent(json.len()));
            make_tuple2(env, atoms::ok().as_c_arg(), resource.encode(env).as_c_arg())
        }
        Err(e) => parse_error_term(env, &e),
    }
}

#[rustler::nif(schedule = "DirtyCpu")]
fn parse_opts_dirty<'a>(env: Env<'a>, json: Binary, unique_keys: bool) -> Term<'a> {
    match do_parse(json.as_slice(), unique_keys) {
        Ok(resource) => make_tuple2(env, atoms::ok().as_c_arg(), resource.encode(env).as_c_arg()),
        Err(e) => parse_error_term(env, &e),
    }
}

/// A selection's share of the borrow budget, by the `Value` it reached.
type SelectionCost = (*const sonic_rs::Value, usize);

/// Turns a parsed document's values into terms: directly for a small
/// document, through the term builder for one with a parse buffer.
enum DocTerms<'x, 'a, 'b> {
    Direct(Env<'a>),
    Built {
        builder: &'x mut TermBuilder<'a, 'b>,
        /// Selections that added to the budget, for `repeated_cost`; `None`
        /// once the budget has been decided.
        costs: Option<&'x mut Vec<SelectionCost>>,
    },
}

impl DocTerms<'_, '_, '_> {
    /// The term for `value`, or `None` when it nests deeper than `MAX_DEPTH`.
    #[inline]
    fn value_term(&mut self, value: &sonic_rs::Value, nodes: &mut usize) -> Option<ERL_NIF_TERM> {
        match self {
            DocTerms::Direct(env) => {
                value_to_term(*env, value, MAX_DEPTH, nodes).map(|t| t.as_c_arg())
            }
            DocTerms::Built { builder, .. } => builder.value_term(value, nodes),
        }
    }

    /// `value_term` for one of several selections, recording what it added
    /// to the budget so a value several paths reach counts once.
    #[inline]
    fn selection_term(
        &mut self,
        value: &sonic_rs::Value,
        nodes: &mut usize,
    ) -> Option<ERL_NIF_TERM> {
        let DocTerms::Built {
            builder,
            costs: Some(costs),
        } = self
        else {
            return self.value_term(value, nodes);
        };
        let (borrowed, built) = (builder.borrowed(), *nodes);
        let term = builder.value_term(value, nodes)?;
        let cost = builder.borrowed() - borrowed + (*nodes - built) * BYTES_PER_NODE;
        if cost > 0 {
            // One allocation for a typical path list rather than three.
            if costs.capacity() == 0 {
                costs.reserve(16);
            }
            costs.push((value, cost));
        }
        Some(term)
    }
}

/// Budget counted again for selections reaching a value an earlier one
/// reached. Such a repeat builds an equal term over the same bytes, so it
/// pins nothing more: counted, four paths to a container pinned a document
/// one path left detached.
#[cold]
fn repeated_cost(costs: &mut [SelectionCost]) -> usize {
    costs.sort_unstable_by_key(|&(value, _)| value);
    costs
        .windows(2)
        .filter(|pair| pair[0].0 == pair[1].0)
        .map(|pair| pair[1].1)
        .sum()
}

/// Builds results from a parsed document with `build`. A buffered document's
/// strings are borrowed unless `borrow_input` (a container counted by its
/// nodes, a value several paths reach once) says no; then `build` reruns
/// with copies, bounded by the results being small next to the document. A
/// selection inside another one counts both times, an overlap the caller
/// chose. Members a duplicate key dropped were counted too, but the input
/// chose those, so then nothing borrows.
#[inline]
fn build_doc_results<'a, R>(
    env: Env<'a>,
    doc: &ParsedDocument,
    nodes: &mut usize,
    mut build: impl for<'x, 'b> FnMut(&mut DocTerms<'x, 'a, 'b>, &mut usize) -> R,
) -> R {
    let Some(buffer) = &doc.buffer else {
        return build(&mut DocTerms::Direct(env), nodes);
    };
    let len = buffer.bytes().len();
    let source = Source::Document(buffer);
    with_term_builder(env, source, Strings::Borrow, Overflow::Float, |builder| {
        let mut costs = Vec::new();
        let result = build(
            &mut DocTerms::Built {
                builder: &mut *builder,
                costs: Some(&mut costs),
            },
            nodes,
        );
        let borrowed = builder.borrowed();
        let counted = borrowed + *nodes * BYTES_PER_NODE;
        // A buffered document is over `BORROW_ANY_INPUT`, so with the counts
        // unusable there is nothing left to justify borrowing. Repeats only
        // lower the count, so they are looked for only when it would borrow.
        if borrowed == 0
            || (!builder.dropped_members()
                && borrow_input(len, || counted)
                && borrow_input(len, || counted - repeated_cost(&mut costs)))
        {
            return result;
        }
        builder.set_strings(Strings::Copy);
        build(
            &mut DocTerms::Built {
                builder,
                costs: None,
            },
            nodes,
        )
    })
}

#[rustler::nif]
fn get<'a>(env: Env<'a>, doc: ResourceArc<ParsedDocument>, path: &str) -> Term<'a> {
    let ok_raw = atoms::ok().as_c_arg();
    let err_raw = atoms::error().as_c_arg();
    let nsf_raw = atoms::no_such_field().as_c_arg();
    let ntd_raw = atoms::nesting_too_deep().as_c_arg();
    let mut nodes = 0usize;
    let result = match pointer_lookup(&doc.value, path, doc.unique_keys) {
        Some(value) => {
            match build_doc_results(env, &doc, &mut nodes, |b, n| b.value_term(value, n)) {
                Some(term) => make_tuple2(env, ok_raw, term),
                None => make_tuple2(env, err_raw, ntd_raw),
            }
        }
        None => make_tuple2(env, err_raw, nsf_raw),
    };
    consume_timeslice_nodes(env, nodes);
    result
}

/// Cached raw atoms for the per-path result tuples in `get_many`.
struct ResultAtoms {
    ok: ERL_NIF_TERM,
    err: ERL_NIF_TERM,
    nsf: ERL_NIF_TERM,
    ntd: ERL_NIF_TERM,
}

#[inline]
fn get_one_result(
    env: Env,
    terms: &mut DocTerms,
    doc: &ParsedDocument,
    path: &str,
    atoms: &ResultAtoms,
    nodes: &mut usize,
) -> ERL_NIF_TERM {
    match pointer_lookup(&doc.value, path, doc.unique_keys) {
        Some(value) => match terms.selection_term(value, nodes) {
            Some(term) => make_tuple2(env, atoms.ok, term).as_c_arg(),
            None => make_tuple2(env, atoms.err, atoms.ntd).as_c_arg(),
        },
        None => make_tuple2(env, atoms.err, atoms.nsf).as_c_arg(),
    }
}

#[rustler::nif]
fn get_many<'a>(
    env: Env<'a>,
    doc: ResourceArc<ParsedDocument>,
    paths: Term<'a>,
) -> NifResult<Term<'a>> {
    let result_atoms = ResultAtoms {
        ok: atoms::ok().as_c_arg(),
        err: atoms::error().as_c_arg(),
        nsf: atoms::no_such_field().as_c_arg(),
        ntd: atoms::nesting_too_deep().as_c_arg(),
    };
    let mut nodes = 0usize;
    let result = build_doc_results(env, &doc, &mut nodes, |terms, nodes| {
        let mut acc = TermAcc::new();
        for path_term in paths.into_list_iterator()? {
            // Non-binary (or non-UTF-8) path entries are caller bugs: badarg.
            let path: &str = path_term.decode()?;
            acc.push(get_one_result(env, terms, &doc, path, &result_atoms, nodes));
        }
        Ok(acc.into_list(env))
    });
    consume_timeslice_nodes(env, nodes);
    result
}

#[rustler::nif]
fn array_length<'a>(env: Env<'a>, doc: ResourceArc<ParsedDocument>, path: &str) -> Term<'a> {
    match pointer_lookup(&doc.value, path, doc.unique_keys) {
        Some(value) if value.is_array() => {
            let len = value.as_array().unwrap().len();
            unsafe {
                Term::new(
                    env,
                    rustler::sys::enif_make_uint64(env.as_c_arg(), len as u64),
                )
            }
        }
        _ => atoms::nil().to_term(env),
    }
}

#[rustler::nif]
fn decode<'a>(env: Env<'a>, json: Binary<'a>) -> Term<'a> {
    let input_term = json.encode(env).as_c_arg();
    let result = native_decode::decode_to_term(env, input_term, json.as_slice());
    schedule::consume_timeslice(env, timeslice_percent(json.len()));
    result
}

#[rustler::nif(schedule = "DirtyCpu")]
fn decode_dirty<'a>(env: Env<'a>, json: Binary<'a>) -> Term<'a> {
    let input_term = json.encode(env).as_c_arg();
    native_decode::decode_to_term(env, input_term, json.as_slice())
}

// --- Pre-compiled pointers + fused parse/extract ---
//
// The common parse-once-extract-once workload uses a *fixed* set of JSON
// Pointer paths known at startup. Compiling those paths once (segment split,
// `~`-unescape, index-vs-key classification) lets the per-request call skip all
// per-path string work — roughly halving extraction time — and fusing the parse
// and the extraction into one NIF call avoids materializing a document handle.
use crate::{CompiledPaths, PathSeg};

/// Pre-split a single JSON Pointer into segments. A numeric segment is stored as
/// `Num`, keeping both the parsed index and the literal key so the lookup can
/// pick the right interpretation per node (array index vs. object key) —
/// matching the runtime behaviour of `pointer_lookup`.
fn compile_one(path: &str) -> NifResult<Vec<PathSeg>> {
    let mut segs = Vec::new();
    // Torque treats both empty and slash-only pointers as the document root.
    if path.is_empty() || path == "/" {
        return Ok(segs);
    }
    // Reject non-pointers before slicing. A multibyte leading character could
    // otherwise panic inside the NIF.
    let rest = match path.strip_prefix('/') {
        Some(rest) => rest,
        None => return Err(rustler::Error::BadArg),
    };
    for segment in rest.split('/') {
        let key = if segment.contains('~') {
            if !escapes_valid(segment) {
                return Err(rustler::Error::BadArg);
            }
            segment.replace("~1", "/").replace("~0", "~")
        } else {
            segment.to_string()
        };
        match array_index(segment) {
            Some(idx) => segs.push(PathSeg::Num { idx, key }),
            None => segs.push(PathSeg::Key(key)),
        }
    }
    Ok(segs)
}

#[rustler::nif]
fn compile_paths<'a>(
    env: Env<'a>,
    paths: ListIterator<'a>,
    unique_keys: bool,
    validate: bool,
) -> NifResult<Term<'a>> {
    let mut out = Vec::new();
    let mut plan = sonic_rs::extract::ExtractPlan::new();
    for pt in paths {
        // Non-binary (or non-UTF-8) entries are caller bugs: badarg. Silently
        // compiling them (e.g. as "") would return the whole document.
        let p: &str = pt.decode()?;
        let segs = compile_one(p)?;
        // The plan borrows segments and copies only new keys.
        plan.add_path(plan_segs(&segs));
        out.push(segs);
    }
    plan.finish();
    Ok(ResourceArc::new(CompiledPaths {
        paths: out,
        plan,
        unique_keys,
        validate,
    })
    .encode(env))
}

/// Borrows compiled path segments for plan construction.
fn plan_segs(
    segs: &[PathSeg],
) -> impl ExactSizeIterator<Item = sonic_rs::extract::Seg<'_>> + use<'_> {
    use sonic_rs::extract::Seg;
    segs.iter().map(|s| match s {
        PathSeg::Key(k) => Seg::Key(k),
        PathSeg::Num { idx, key } => Seg::Index { idx: *idx, key },
    })
}

/// Inputs at or under this are borrowed from whatever is taken out of them.
/// A binary this size is a single allocation whose cost is about what the refc
/// bookkeeping for one extracted string is, so refusing to pin it buys nothing
/// and costs a copy. The request-shaped documents this path is tuned on - a
/// 1.9 KB OpenRTB bid request - sit well inside it.
const BORROW_ANY_INPUT: usize = 4096;

/// Otherwise, extracted strings must cover this fraction of the input.
const BORROW_INPUT_FRACTION: usize = 4;

/// Whether extracted values should point into the caller's binary.
///
/// A sub-binary keeps the whole input alive for as long as any string cut from
/// it survives. That is the trade `decode/1` makes and worth making there,
/// since a decoded document holds most of its input anyway. One-shot
/// extraction is the opposite shape - it exists to answer a few paths and drop
/// the document - and a 100-byte user agent taken from a 400 KB feed pinned
/// all 400 KB of it, which `process_info(:binary)` reports and a bidder
/// running a million of these a second pays for.
///
/// The answer is per call rather than per string, so a result list is either
/// all borrowed or all copied. It weighs what the results hold against the
/// input they would keep alive, so every string counts, short ones that ERTS
/// copies onto the heap included, and a selected container counts for its
/// whole span.
///
/// `borrowed_len` is called only for an input large enough for the answer to
/// depend on it, so the request-shaped documents this path is tuned on never
/// walk their results twice.
#[inline]
fn borrow_input(input_len: usize, borrowed_len: impl FnOnce() -> usize) -> bool {
    input_len <= BORROW_ANY_INPUT
        || borrowed_len().saturating_mul(BORROW_INPUT_FRACTION) >= input_len
}

/// Source bytes per built term, for charging the terms of a selected
/// container by its span. See `NODES_PER_REDUCTION`.
const BYTES_PER_NODE: usize = 8;

/// Re-parses the selected containers with copied strings, returning each one's
/// term by root index. Containers are built before the batch knows whether it
/// may borrow. The answer is no when their spans total under a quarter of the
/// input, or when a duplicate key dropped members from one; selected
/// containers never nest, so this parse covers at most the input again. A
/// repeated path shares its first occurrence's root, so it is skipped.
#[cold]
fn rebuild_copied(
    bytes: &[u8],
    plan: &sonic_rs::extract::ExtractPlan,
    values: &[Option<sonic_rs::extract::Extracted>],
    builder: &mut TermBuilder,
) -> Vec<ERL_NIF_TERM> {
    use sonic_rs::extract::Extracted;

    let mut terms = builder.roots().to_vec();
    builder.set_strings(Strings::Copy);
    for (slot, v) in values.iter().enumerate() {
        let Some(Extracted::Visited { root, start, end }) = v else {
            continue;
        };
        if plan.is_repeat(slot) {
            continue;
        }
        // These bytes already parsed once, so this cannot fail; if it somehow
        // did, the borrowed term is still a correct value.
        if sonic_rs::parse_into_visitor(&bytes[*start..*end], builder).is_ok() {
            if let Some(&term) = builder.roots().last() {
                terms[*root] = term;
            }
        }
    }
    terms
}

/// Parses once and returns the result term with the number of bytes reached.
/// Selected values are built directly; other regions are parsed or skipped
/// according to the compiled validation policy.
#[inline]
fn do_parse_get_many_nil<'a>(
    env: Env<'a>,
    input_term: ERL_NIF_TERM,
    bytes: &[u8],
    compiled: &CompiledPaths,
    nodes: &mut usize,
) -> Term<'a> {
    use sonic_rs::extract::{Extracted, Keys, Validate};

    let validate = if compiled.validate {
        Validate::Yes
    } else {
        Validate::No
    };
    let keys = if compiled.unique_keys {
        Keys::Unique
    } else {
        Keys::Repeatable
    };

    let source = Source::Binary {
        term: input_term,
        bytes,
    };
    with_term_builder(env, source, Strings::Borrow, Overflow::Float, |builder| {
        let values =
            match sonic_rs::extract::extract_with(bytes, &compiled.plan, validate, keys, builder) {
                Ok(values) => values,
                Err(e) => return builder_error_term(env, builder, &e),
            };
        let nil_raw = atoms::nil().as_c_arg();
        // A repeated path's result is its first occurrence's term, so it adds
        // neither borrowed bytes nor work. A container that lost members to a
        // duplicate key spans bytes no result holds, so after one, containers
        // add nothing either.
        let spans_count = !builder.dropped_members();
        let borrow = borrow_input(bytes.len(), || {
            values
                .iter()
                .enumerate()
                .filter(|&(slot, _)| !compiled.plan.is_repeat(slot))
                .map(|(_, v)| match v {
                    Some(Extracted::Str(s)) => s.len(),
                    Some(Extracted::Visited { start, end, .. }) if spans_count => end - start,
                    _ => 0,
                })
                .sum()
        });
        let rebuilt = (!borrow && builder.borrowed() > 0)
            .then(|| rebuild_copied(bytes, &compiled.plan, &values, builder));
        builder.set_strings(if borrow {
            Strings::Borrow
        } else {
            Strings::Copy
        });
        // Consed from the back, so the list needs no staging buffer.
        let mut list = unsafe { enif_make_list_from_array(env.as_c_arg(), std::ptr::null(), 0) };
        for (slot, v) in values.iter().enumerate().rev() {
            let t = match v {
                // ERTS copies a slice this short onto the heap anyway, so
                // building the heap binary here skips its sub-binary bookkeeping.
                Some(Extracted::Str(s)) if s.len() <= HEAP_BINARY_MAX => {
                    native_decode::copied_binary(env, s)
                }
                // A string the parser never had to unescape is still in the
                // caller's binary, so the term can point at it rather than
                // pay a copy into a `Value` and another out of it - as long
                // as keeping the input behind it is worth that.
                Some(Extracted::Str(s)) => builder.str_term(s),
                Some(Extracted::Visited { root, start, end }) => {
                    if !compiled.plan.is_repeat(slot) {
                        *nodes += (end - start) / BYTES_PER_NODE;
                    }
                    match &rebuilt {
                        Some(terms) => terms[*root],
                        None => builder.roots()[*root],
                    }
                }
                Some(Extracted::U64(n)) => unsafe { enif_make_uint64(env.as_c_arg(), *n) },
                Some(Extracted::I64(n)) => unsafe { enif_make_int64(env.as_c_arg(), *n) },
                Some(Extracted::F64(n)) => unsafe { enif_make_double(env.as_c_arg(), *n) },
                Some(Extracted::Bool(true)) => atoms::r#true().as_c_arg(),
                Some(Extracted::Bool(false)) => atoms::r#false().as_c_arg(),
                Some(Extracted::Value(v)) => value_to_term(env, v, MAX_DEPTH, nodes)
                    .map(|t| t.as_c_arg())
                    .unwrap_or(nil_raw),
                Some(Extracted::Null) | None => nil_raw,
            };
            list = unsafe { enif_make_list_cell(env.as_c_arg(), t, list) };
        }
        make_tuple2(env, atoms::ok().as_c_arg(), list)
    })
}

#[rustler::nif]
fn parse_get_many_nil<'a>(
    env: Env<'a>,
    json: Binary<'a>,
    compiled: ResourceArc<CompiledPaths>,
) -> Term<'a> {
    let mut nodes = 0usize;
    let input_term = json.to_term(env).as_c_arg();
    let result = do_parse_get_many_nil(env, input_term, json.as_slice(), &compiled, &mut nodes);
    // Timeslice fractions accumulate: bytes cover the parse, nodes the extraction.
    schedule::consume_timeslice(env, timeslice_percent(json.len()));
    consume_timeslice_nodes(env, nodes);
    result
}

#[rustler::nif(schedule = "DirtyCpu")]
fn parse_get_many_nil_dirty<'a>(
    env: Env<'a>,
    json: Binary<'a>,
    compiled: ResourceArc<CompiledPaths>,
) -> Term<'a> {
    let mut nodes = 0usize;
    let input_term = json.to_term(env).as_c_arg();
    do_parse_get_many_nil(env, input_term, json.as_slice(), &compiled, &mut nodes)
}

#[inline]
fn pointer_lookup_compiled<'v>(
    value: &'v sonic_rs::Value,
    segs: &[PathSeg],
    unique_keys: bool,
) -> Option<&'v sonic_rs::Value> {
    let mut current = value;
    for seg in segs {
        current = match seg {
            PathSeg::Key(k) => object_get(current, k, unique_keys)?,
            PathSeg::Num { idx, key } => {
                if current.is_array() {
                    current.get(*idx)?
                } else {
                    object_get(current, key, unique_keys)?
                }
            }
        };
    }
    Some(current)
}

#[rustler::nif]
fn get_many_nil_compiled<'a>(
    env: Env<'a>,
    doc: ResourceArc<ParsedDocument>,
    compiled: ResourceArc<CompiledPaths>,
) -> Term<'a> {
    let nil_raw = atoms::nil().as_c_arg();
    let mut nodes = 0usize;
    let result = build_doc_results(env, &doc, &mut nodes, |terms, nodes| {
        let mut acc = TermAcc::with_hint(compiled.paths.len());
        for segs in compiled.paths.iter() {
            // Missing fields and depth-exceeded values both read as nil.
            let r = pointer_lookup_compiled(&doc.value, segs, compiled.unique_keys)
                .and_then(|v| terms.selection_term(v, nodes))
                .unwrap_or(nil_raw);
            acc.push(r);
        }
        acc.into_list(env)
    });
    consume_timeslice_nodes(env, nodes);
    result
}

#[rustler::nif]
fn get_many_nil<'a>(
    env: Env<'a>,
    doc: ResourceArc<ParsedDocument>,
    paths: Term<'a>,
) -> NifResult<Term<'a>> {
    let nil_raw = atoms::nil().as_c_arg();
    let mut nodes = 0usize;
    let result = build_doc_results(env, &doc, &mut nodes, |terms, nodes| {
        let mut acc = TermAcc::new();
        for path_term in paths.into_list_iterator()? {
            // Non-binary (or non-UTF-8) path entries are caller bugs: badarg.
            let path: &str = path_term.decode()?;
            let r = pointer_lookup(&doc.value, path, doc.unique_keys)
                .and_then(|value| terms.selection_term(value, nodes))
                .unwrap_or(nil_raw);
            acc.push(r);
        }
        Ok(acc.into_list(env))
    });
    consume_timeslice_nodes(env, nodes);
    result
}

#[cfg(test)]
mod extract_regressions {
    use sonic_rs::extract::{extract, ExtractPlan, Extracted, Keys, Seg, Validate};
    use sonic_rs::{JsonValueTrait, Value};

    fn owned(value: Option<Extracted<'_>>) -> Value {
        match value {
            Some(Extracted::Value(value)) => value,
            other => panic!("expected an owned value, got {other:?}"),
        }
    }

    fn numbers(json: &str, plan: &ExtractPlan) -> Vec<Option<u64>> {
        extract(json, plan, Validate::Yes, Keys::Repeatable)
            .unwrap()
            .into_iter()
            .map(|value| value.map(|value| owned(Some(value)).as_u64().unwrap()))
            .collect()
    }

    // These arena lifetime tests also run under Miri, including root selection,
    // selected subtrees sharing an arena, and error unwinding after allocation.
    #[test]
    fn selected_container_root_outlives_extraction() {
        let mut plan = ExtractPlan::new();
        plan.add_path(std::iter::empty());
        for validate in [Validate::Yes, Validate::No] {
            let mut values = extract("[1]", &plan, validate, Keys::Repeatable).unwrap();
            let root = owned(values.pop().unwrap());
            let child = root.get(0).unwrap().clone();
            let clone = root.clone();
            drop(values);
            drop(root);
            assert_eq!(clone.get(0).unwrap().as_u64(), Some(1));
            drop(clone);
            assert_eq!(child.as_u64(), Some(1));
        }
    }

    #[test]
    fn selected_nested_containers_survive_independent_drops() {
        let mut plan = ExtractPlan::new();
        plan.add_path([Seg::Key("a")].into_iter());
        plan.add_path([Seg::Key("a"), Seg::Index { idx: 0, key: "0" }].into_iter());
        plan.add_path(
            [
                Seg::Key("a"),
                Seg::Index { idx: 1, key: "1" },
                Seg::Key("b"),
            ]
            .into_iter(),
        );
        plan.add_path([Seg::Key("a")].into_iter());
        for validate in [Validate::Yes, Validate::No] {
            let mut values = extract(
                r#"{"a":[[1],{"b":[2]}],"other":0}"#,
                &plan,
                validate,
                Keys::Repeatable,
            )
            .unwrap();
            let duplicate = owned(values.pop().unwrap());
            let nested = owned(values.pop().unwrap());
            let first = owned(values.pop().unwrap());
            drop(values);
            assert_eq!(duplicate.get(0).unwrap().get(0).unwrap().as_u64(), Some(1));
            drop(duplicate);
            assert_eq!(nested.get(0).unwrap().as_u64(), Some(2));
            drop(nested);
            assert_eq!(first.get(0).unwrap().as_u64(), Some(1));
        }
    }

    #[test]
    fn selected_container_errors_unwind_arenas() {
        let mut root = ExtractPlan::new();
        root.add_path(std::iter::empty());
        let mut nested = ExtractPlan::new();
        nested.add_path([Seg::Key("a")].into_iter());
        nested.add_path([Seg::Key("broken")].into_iter());
        for validate in [Validate::Yes, Validate::No] {
            for json in ["[1,", r#"{"a":[1],"b":[2,}"#] {
                assert!(extract(json, &root, validate, Keys::Repeatable).is_err());
            }
            assert!(extract(
                r#"{"a":[[1],{"b":[2]}],"broken":[2,}"#,
                &nested,
                validate,
                Keys::Repeatable,
            )
            .is_err());
        }
        assert!(extract("[1] x", &root, Validate::Yes, Keys::Repeatable).is_err());
    }

    #[test]
    fn numeric_plans_work_before_finish_and_after_more_paths() {
        let width = 40;
        let mut plan = ExtractPlan::new();
        // A key-only edge upgraded to numeric must share both result slots.
        plan.add_path([Seg::Key("4")].into_iter());
        for idx in (0..width).rev() {
            let key = idx.to_string();
            plan.add_path([Seg::Index { idx, key: &key }].into_iter());
        }
        plan.add_path([Seg::Index { idx: 4, key: "4" }].into_iter());
        let missing = usize::MAX.to_string();
        plan.add_path(
            [Seg::Index {
                idx: usize::MAX,
                key: &missing,
            }]
            .into_iter(),
        );
        let json = format!(
            "[{}]",
            (0..=width)
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut expected = vec![Some(4)];
        expected.extend((0..width as u64).rev().map(Some));
        expected.extend([Some(4), None]);
        assert_eq!(numbers(&json, &plan), expected);
        plan.finish();
        assert_eq!(numbers(&json, &plan), expected);

        // Inserting below the largest existing index invalidates sorted order.
        let key = width.to_string();
        plan.add_path(
            [Seg::Index {
                idx: width,
                key: &key,
            }]
            .into_iter(),
        );
        expected.push(Some(width as u64));
        assert_eq!(numbers(&json, &plan), expected);
        plan.finish();
        assert_eq!(numbers(&json, &plan), expected);

        let object = format!(
            "{{{}}}",
            (0..=width)
                .map(|i| format!(r#""{i}":{}"#, i + 100))
                .collect::<Vec<_>>()
                .join(",")
        );
        let expected: Vec<_> = expected.into_iter().map(|n| n.map(|n| n + 100)).collect();
        assert_eq!(numbers(&object, &plan), expected);
    }
}
