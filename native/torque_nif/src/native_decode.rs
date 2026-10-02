//! Fused single-pass decoder built on sonic-rs's native push-based parser.
//!
//! This implements sonic-rs's `JsonVisitor` directly, building Erlang terms
//! during the SIMD parse — no intermediate `Value` tree, zero-copy
//! sub-binaries for unescaped strings, and one shared term per repeated
//! object key (see `KeyCache`).
//!
//! Terms are assembled with a postfix value stack: scalars push a term; a
//! container-end pops its children, builds the list/map term, and pushes the
//! result. After a successful parse the stack holds exactly the root term.

use rustler::sys::{
    enif_make_double, enif_make_int64, enif_make_list_from_array, enif_make_map_put,
    enif_make_new_map, enif_make_sub_binary, enif_make_uint64, ERL_NIF_TERM,
};
use rustler::{Encoder, Env, NewBinary, Term};
use sonic_rs::JsonVisitor;
use std::cell::RefCell;

use crate::atoms;
use crate::decoder::parse_error_term;
use crate::nif_util::{make_tuple2, map_from_arrays};
use crate::types::MAX_DEPTH;
use crate::ParseBuffer;

/// Cap on each retained thread-local stack, in entries, so a one-off huge
/// document doesn't pin a large allocation on a scheduler thread.
const STACK_RETAIN_CAP: usize = 1 << 17;

/// Empties a stack and caps its capacity. Emptying first matters: `shrink_to`
/// cannot go below the length, and a build can leave any number of roots.
fn release<T>(stack: &mut Vec<T>) {
    stack.clear();
    if stack.capacity() > STACK_RETAIN_CAP {
        stack.shrink_to(STACK_RETAIN_CAP);
    }
}

const KEY_CACHE_SLOTS: usize = 256;
/// Longest key eligible for caching; bounds the byte-compare on lookup.
const KEY_CACHE_MAX_LEN: usize = 64;
/// Miss/hit balance above which the cache is bypassed for the rest of the
/// call (see `KeyCache::debit`).
const KEY_CACHE_BYPASS_AT: i32 = 256;

#[derive(Clone, Copy)]
struct KeyEntry {
    ptr: *const u8,
    term: ERL_NIF_TERM,
    /// First 8 key bytes, zero-padded, and for longer keys the last 8. Up to
    /// 16 bytes they cover the whole key, so a hit needs no byte compare.
    prefix: u64,
    tail: u64,
    len: u32,
    epoch: u32,
}

/// Direct-mapped, per-call memo of object-key terms. Typical JSON repeats the
/// same few keys across every element of an array, and each occurrence used to
/// build a fresh term; a hit reuses the earlier one instead. Entries are keyed
/// by pointers into the input buffer (stable for the whole call) and
/// invalidated between calls by an epoch counter, since terms are only valid
/// within the env of the call that made them.
///
/// Cached keys are built as *copied* heap binaries rather than sub-binaries.
/// On OTP 27+ this matches what `enif_make_sub_binary` does anyway (slices
/// ≤ 64 bytes are copied on-heap); on older OTPs it avoids real sub-binaries
/// that would pin the whole input binary via the decoded map's keys. The copy
/// is once per distinct key per call — amortized by the cache.
struct KeyCache {
    entries: [KeyEntry; KEY_CACHE_SLOTS],
    epoch: u32,
    /// Per-call adaptivity: a miss adds 1, a hit subtracts 8. Documents shaped
    /// as unique-key dictionaries never hit, so once the balance exceeds
    /// `KEY_CACHE_BYPASS_AT` the cache is bypassed for the rest of the call
    /// rather than charging a lookup per key that can never pay off. The
    /// threshold is high enough that a record array whose objects have up to
    /// ~256 distinct keys still warms the cache before tripping it.
    debit: i32,
}

impl KeyCache {
    fn new() -> Self {
        KeyCache {
            entries: [KeyEntry {
                ptr: std::ptr::null(),
                term: 0,
                prefix: 0,
                tail: 0,
                len: 0,
                epoch: 0,
            }; KEY_CACHE_SLOTS],
            epoch: 0,
            debit: 0,
        }
    }

    /// Invalidate all entries for a new decode call. On the (rare) epoch
    /// wraparound, hard-clear so stale entries can't alias the new epoch.
    #[inline]
    fn next_epoch(&mut self) {
        self.debit = 0;
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            for e in self.entries.iter_mut() {
                e.epoch = 0;
            }
            self.epoch = 1;
        }
    }
}

/// Largest map ERTS stores as a flatmap (`MAP_SMALL_MAP_LIMIT`).
const FLATMAP_LIMIT: usize = 32;
/// Below this, ERTS's sort is cheaper than checking the order ourselves.
const MIN_ORDERED_MEMBERS: usize = 4;

/// Sort key for an object member. `ptr` is null when the key bytes live in
/// the parser's scratch buffer (escaped keys), which later strings overwrite.
#[derive(Clone, Copy)]
struct KeyOrd {
    prefix: u64,
    ptr: *const u8,
    len: usize,
}

impl KeyOrd {
    /// Erlang orders binaries by bytes, then length, like `[u8]`'s `Ord`.
    #[inline]
    fn lt(&self, other: &KeyOrd) -> bool {
        if self.prefix != other.prefix {
            return self.prefix < other.prefix;
        }
        // SAFETY: both point into the input binary, which outlives the call.
        unsafe {
            std::slice::from_raw_parts(self.ptr, self.len)
                < std::slice::from_raw_parts(other.ptr, other.len)
        }
    }
}

#[derive(Clone, Copy)]
struct Frame {
    values: usize,
    ords: usize,
}

struct DecodeBufs {
    values: Vec<ERL_NIF_TERM>,
    frames: Vec<Frame>,
    key_terms: Vec<ERL_NIF_TERM>,
    ords: Vec<KeyOrd>,
    keys: KeyCache,
}

thread_local! {
    /// Reused across decode calls on each scheduler thread, avoiding two heap
    /// allocations (the value and frame stacks) per call — the dominant
    /// per-call cost for small payloads. NIFs run to completion without
    /// preemption and decode never re-enters this NIF, so the borrow is never
    /// nested.
    static DECODE_BUFS: RefCell<DecodeBufs> = RefCell::new(DecodeBufs {
        values: Vec::with_capacity(64),
        frames: Vec::with_capacity(16),
        key_terms: Vec::with_capacity(64),
        ords: Vec::with_capacity(64),
        keys: KeyCache::new(),
    });
}

/// Where the strings a builder may borrow live.
pub(crate) enum Source<'r> {
    /// The binary term `term`, whose bytes are `bytes`.
    Binary { term: ERL_NIF_TERM, bytes: &'r [u8] },
    /// A parsed document's buffer, made a binary term of the caller's
    /// environment only once a string is taken from it, so gets of numbers
    /// and literals skip the copy.
    Document(&'r ParseBuffer),
}

struct InputRef<'b> {
    /// 0 until a `Source::Document` buffer is first referenced.
    term: ERL_NIF_TERM,
    base: *const u8,
    len: usize,
    document: Option<&'b ParseBuffer>,
}

impl InputRef<'_> {
    /// Offset of `s` when its entire span lies inside the input. Integer
    /// arithmetic on purpose: `s` may point into the parser's scratch buffer,
    /// a different allocation, where `offset_from` would be undefined.
    #[inline]
    fn offset_within(&self, s: &str) -> Option<usize> {
        let offset = (s.as_ptr() as usize).checked_sub(self.base as usize)?;
        let room = self.len.checked_sub(offset)?;
        (s.len() <= room).then_some(offset)
    }

    #[cold]
    fn make_term(&mut self, env: Env) -> ERL_NIF_TERM {
        if let Some(document) = self.document {
            self.term = document.term_in(env);
        }
        self.term
    }
}

/// How the builder spells a string that lies inside the input binary.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Strings {
    /// A sub-binary of the input, which may keep the input alive.
    Borrow,
    /// A fresh binary.
    Copy,
}

/// How the builder spells an integer literal outside the i64/u64 range.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Overflow {
    /// An exact bignum, which `decode/1` returns.
    Bignum,
    /// The nearest float, as a sonic-rs `Value` holds it. Extraction and get
    /// mix both builders depending on which other pointers were asked for,
    /// so they must agree.
    Float,
}

/// Longest slice ERTS copies onto the heap for a sub-binary, since OTP 27.
/// Before that, a sub-binary of any length references its source.
pub(crate) const HEAP_BINARY_MAX: usize = 64;

/// A fresh binary holding `s`.
#[inline]
pub(crate) fn copied_binary(env: Env, s: &str) -> ERL_NIF_TERM {
    let mut binary = NewBinary::new(env, s.len());
    binary.as_mut_slice().copy_from_slice(s.as_bytes());
    let term: Term = binary.into();
    term.as_c_arg()
}

pub(crate) struct TermBuilder<'a, 'b> {
    env: Env<'a>,
    input: InputRef<'b>,
    strings: Strings,
    overflow: Overflow,
    /// Bytes of strings borrowed from the input. ERTS (since OTP 27) copies
    /// the short ones onto the heap, but they still count: the budget weighs
    /// what the results hold against the input they may keep alive.
    borrowed: usize,
    /// An object built so far lost members to a later duplicate key, so
    /// `borrowed` counts strings no result holds.
    dropped_members: bool,
    /// Postfix value stack: completed terms plus the open containers' children.
    /// Borrowed from a reused thread-local buffer (see `DECODE_BUFS`).
    values: &'b mut Vec<ERL_NIF_TERM>,
    /// Where each currently-open container's children begin in `values`,
    /// and its members' keys in `key_terms` and `ords`.
    frames: &'b mut Vec<Frame>,
    /// Open objects' key terms, kept apart from `values` so an object's keys
    /// and values reach `enif_make_map_from_arrays` without being copied.
    key_terms: &'b mut Vec<ERL_NIF_TERM>,
    ords: &'b mut Vec<KeyOrd>,
    keys: &'b mut KeyCache,
    too_deep: bool,
}

impl<'a, 'b> TermBuilder<'a, 'b> {
    /// Terms of the values parsed so far, in order. Each parse driven through
    /// the builder leaves exactly one.
    #[inline]
    pub(crate) fn roots(&self) -> &[ERL_NIF_TERM] {
        debug_assert!(self.frames.is_empty());
        self.values
    }

    /// Bytes of input strings borrowed so far.
    #[inline]
    pub(crate) fn borrowed(&self) -> usize {
        self.borrowed
    }

    /// Whether a duplicate key discarded built members, leaving `borrowed`
    /// and any span or node count of the containers above what survives.
    #[inline]
    pub(crate) fn dropped_members(&self) -> bool {
        self.dropped_members
    }

    #[inline]
    pub(crate) fn set_strings(&mut self, strings: Strings) {
        self.strings = strings;
    }

    #[inline]
    fn push(&mut self, term: ERL_NIF_TERM) {
        self.values.push(term);
    }

    /// Sub-binary (zero-copy) when the str lives in the input buffer and the
    /// builder borrows, else copy (escaped strings are unescaped into the
    /// parser's scratch buffer).
    #[inline]
    pub(crate) fn str_term(&mut self, s: &str) -> ERL_NIF_TERM {
        if self.strings == Strings::Borrow {
            if let Some(offset) = self.input.offset_within(s) {
                self.borrowed += s.len();
                let term = match self.input.term {
                    0 => self.input.make_term(self.env),
                    term => term,
                };
                return unsafe { enif_make_sub_binary(self.env.as_c_arg(), term, offset, s.len()) };
            }
        }
        copied_binary(self.env, s)
    }

    /// Term for an object key, memoized in the per-call key cache.
    ///
    /// Only borrowed keys qualify: an escaped key's bytes live in the parser's
    /// scratch buffer, which later strings overwrite, so its pointer can't be
    /// used as a cache identity. Those (rare) keys fall back to `str_term`.
    /// `wide` says the key starts at least 8 bytes before the end of the input,
    /// so `prefix` came from one unaligned load; a key inside the final 8 bytes
    /// of the document (impossible in valid JSON, which needs at least `":x}`
    /// after it) just falls back.
    #[inline(always)]
    fn key_term(&mut self, s: &str, wide: bool, prefix: u64) -> ERL_NIF_TERM {
        let ptr = s.as_ptr();
        let len = s.len();
        if !wide || self.keys.debit > KEY_CACHE_BYPASS_AT || len == 0 || len > KEY_CACHE_MAX_LEN {
            return self.str_term(s);
        }
        // Schema keys often share their first 8 bytes and length
        // (`profile_link_color`, `profile_text_color`), so mix in the last 8
        // too or they evict each other from the same slot.
        let tail = if len > 8 {
            unsafe { (ptr.add(len - 8) as *const u64).read_unaligned() }
        } else {
            0
        };
        let h = (prefix ^ (len as u64) ^ tail.rotate_left(29)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let entry = &mut self.keys.entries[(h >> 56) as usize & (KEY_CACHE_SLOTS - 1)];
        if entry.epoch == self.keys.epoch
            && entry.prefix == prefix
            && entry.tail == tail
            && entry.len == len as u32
            && (len <= 16
                || unsafe {
                    std::slice::from_raw_parts(entry.ptr.add(8), len - 16)
                        == std::slice::from_raw_parts(ptr.add(8), len - 16)
                })
        {
            self.keys.debit -= 8;
            return entry.term;
        }
        self.keys.debit += 1;
        // len <= KEY_CACHE_MAX_LEN (64), so this is an on-heap binary, not refc.
        let mut binary = NewBinary::new(self.env, len);
        binary.as_mut_slice().copy_from_slice(s.as_bytes());
        let term = Term::from(binary).as_c_arg();
        *entry = KeyEntry {
            ptr,
            term,
            prefix,
            tail,
            len: len as u32,
            epoch: self.keys.epoch,
        };
        term
    }

    /// Builds the term for a parsed document's `value` through the same hooks
    /// a parse drives, sharing the key cache and presorted map members. `None`
    /// past `MAX_DEPTH`, leaving the builder ready for the next value. `nodes`
    /// counts container children for timeslice accounting.
    pub(crate) fn value_term(
        &mut self,
        value: &sonic_rs::Value,
        nodes: &mut usize,
    ) -> Option<ERL_NIF_TERM> {
        let base = self.values.len();
        if self.replay(value, nodes) {
            return self.values.pop();
        }
        self.values.truncate(base);
        self.frames.clear();
        self.key_terms.clear();
        self.ords.clear();
        self.too_deep = false;
        None
    }

    /// The term for a scalar `value`; `None` for a container.
    #[inline]
    fn scalar_term(&mut self, value: &sonic_rs::Value) -> Option<ERL_NIF_TERM> {
        use sonic_rs::{JsonType, JsonValueTrait};
        let env = self.env.as_c_arg();
        Some(match value.get_type() {
            JsonType::Null => atoms::nil().as_c_arg(),
            JsonType::Boolean if value.is_true() => atoms::r#true().as_c_arg(),
            JsonType::Boolean => atoms::r#false().as_c_arg(),
            JsonType::Number => match (value.as_i64(), value.as_u64()) {
                (Some(n), _) => unsafe { enif_make_int64(env, n) },
                (None, Some(n)) => unsafe { enif_make_uint64(env, n) },
                // A number that is neither integer type is a float.
                (None, None) => unsafe {
                    enif_make_double(env, value.as_f64().unwrap_or_default())
                },
            },
            JsonType::String => self.str_term(value.as_str().unwrap_or_default()),
            JsonType::Array | JsonType::Object => return None,
        })
    }

    fn replay(&mut self, value: &sonic_rs::Value, nodes: &mut usize) -> bool {
        use sonic_rs::{JsonContainerTrait, JsonType, JsonValueTrait};
        if let Some(term) = self.scalar_term(value) {
            self.push(term);
            return true;
        }
        if value.get_type() == JsonType::Array {
            let Some(array) = value.as_array() else {
                return false;
            };
            *nodes += array.len();
            if !self.visit_array_start(array.len()) {
                return false;
            }
            for child in array.iter() {
                if !self.replay(child, nodes) {
                    return false;
                }
            }
            return self.visit_array_end(array.len());
        }
        let Some(object) = value.as_object() else {
            return false;
        };
        *nodes += 2 * object.len();
        if !self.visit_object_start(object.len()) {
            return false;
        }
        for (key, child) in object.iter() {
            if !self.visit_key(key) || !self.replay(child, nodes) {
                return false;
            }
        }
        self.visit_object_end(object.len())
    }
}

/// Stable insertion sort of member indices into Erlang term order, or `None`
/// when the members are already ordered or a key can't be compared here.
///
/// ERTS sorts flatmap keys itself (`erts_validate_and_sort_flatmap`) with an
/// insertion sort over generic term comparisons, O(n²) unless the keys arrive
/// in term order. JSON producers other than the BEAM emit schema order, so
/// sorting on raw key bytes first leaves ERTS a linear validation pass.
#[inline]
fn member_order(ords: &[KeyOrd]) -> Option<[u8; FLATMAP_LIMIT]> {
    // Sorted neighbours often share 8 bytes (`in_reply_to_status_id`,
    // `in_reply_to_status_id_str`); compare those in full rather than
    // sorting to find nothing moved.
    let ordered = ords.windows(2).all(|w| {
        w[0].prefix < w[1].prefix
            || (w[0].prefix == w[1].prefix
                && !w[0].ptr.is_null()
                && !w[1].ptr.is_null()
                && w[0].lt(&w[1]))
    });
    if ordered || ords.iter().any(|o| o.ptr.is_null()) {
        return None;
    }
    let mut perm = [0u8; FLATMAP_LIMIT];
    let mut moved = false;
    for i in 0..ords.len() {
        let mut j = i;
        while j > 0 && ords[i].lt(&ords[perm[j - 1] as usize]) {
            perm[j] = perm[j - 1];
            j -= 1;
        }
        moved |= j != i;
        perm[j] = i as u8;
    }
    moved.then_some(perm)
}

/// Build a map term from an object's keys and values, ordering its members
/// first when ERTS would otherwise sort them itself. `None` when a key
/// repeats.
#[inline]
fn build_map(
    env: Env,
    keys: &[ERL_NIF_TERM],
    vals: &[ERL_NIF_TERM],
    ords: &[KeyOrd],
) -> Option<ERL_NIF_TERM> {
    let pairs = keys.len();
    if (MIN_ORDERED_MEMBERS..=FLATMAP_LIMIT).contains(&pairs) {
        if let Some(perm) = member_order(ords) {
            let mut k = [0 as ERL_NIF_TERM; FLATMAP_LIMIT];
            let mut v = [0 as ERL_NIF_TERM; FLATMAP_LIMIT];
            for (slot, &i) in perm[..pairs].iter().enumerate() {
                k[slot] = keys[i as usize];
                v[slot] = vals[i as usize];
            }
            return make_map(env, &k[..pairs], &v[..pairs]);
        }
    }
    make_map(env, keys, vals)
}

#[inline]
fn make_map(env: Env, keys: &[ERL_NIF_TERM], vals: &[ERL_NIF_TERM]) -> Option<ERL_NIF_TERM> {
    let mut map: ERL_NIF_TERM = 0;
    // SAFETY: `keys` and `vals` are live slices of `keys.len()` terms.
    unsafe { map_from_arrays(env, keys.as_ptr(), vals.as_ptr(), keys.len(), &mut map) }
        .then_some(map)
}

/// Map of an object whose keys repeat: last value wins, in source order
/// (matches `object_get` lookups).
#[cold]
#[inline(never)]
fn map_last_wins(env: Env, keys: &[ERL_NIF_TERM], vals: &[ERL_NIF_TERM]) -> ERL_NIF_TERM {
    unsafe {
        let mut map = enif_make_new_map(env.as_c_arg());
        for (&key, &val) in keys.iter().zip(vals) {
            let mut new_map: ERL_NIF_TERM = 0;
            enif_make_map_put(env.as_c_arg(), map, key, val, &mut new_map);
            map = new_map;
        }
        map
    }
}

// Erlang External Term Format tags for arbitrary-precision integers.
const ETF_VERSION: u8 = 131;
const SMALL_BIG_EXT: u8 = 110;
/// Magnitude bytes for SMALL_BIG_EXT fit in a one-byte length, so 255 base-256
/// bytes (~614 decimal digits) is the stack fast path; larger falls back.
const MAG_CAP: usize = 255;

/// Build an exact Erlang bignum term from a decimal integer token.
///
/// Converts the digits to a little-endian base-256 magnitude on the stack and
/// hands ERTS the SMALL_BIG_EXT bytes directly (`binary_to_term_trusted`, no
/// SAFE scan) — no `num-bigint` allocation, no `to_bytes_le` pass, no heap
/// buffer. Tokens beyond `MAG_CAP` bytes defer to `num-bigint` so correctness
/// stays unbounded. Returns `None` only if the digits don't parse.
#[inline]
fn bignum_term(env: Env, raw: &str) -> Option<ERL_NIF_TERM> {
    let (neg, digits) = match raw.as_bytes().split_first() {
        Some((b'-', rest)) => (1u8, rest),
        _ => (0u8, raw.as_bytes()),
    };
    if digits.is_empty() {
        return None;
    }

    let mut mag = [0u8; MAG_CAP];
    let mut len = 0usize;
    for &d in digits {
        let mut carry = match d {
            b'0'..=b'9' => (d - b'0') as u32,
            _ => return None,
        };
        for limb in mag[..len].iter_mut() {
            let v = *limb as u32 * 10 + carry;
            *limb = v as u8;
            carry = v >> 8;
        }
        while carry > 0 {
            if len >= MAG_CAP {
                return bignum_term_large(env, raw);
            }
            mag[len] = carry as u8;
            carry >>= 8;
            len += 1;
        }
    }

    // ETF: [131, SMALL_BIG_EXT, len, sign, <len LE magnitude bytes>]
    let mut etf = [0u8; 4 + MAG_CAP];
    etf[0] = ETF_VERSION;
    etf[1] = SMALL_BIG_EXT;
    etf[2] = len as u8;
    etf[3] = neg;
    etf[4..4 + len].copy_from_slice(&mag[..len]);

    // SAFETY: self-constructed SMALL_BIG_EXT — no atoms/resources, so the
    // trusted (unsafe, no-SAFE-scan) decode cannot create anything unsafe.
    unsafe { env.binary_to_term_trusted(&etf[..4 + len]) }.map(|(t, _)| t.as_c_arg())
}

/// Cold path for integers too large for the stack buffer (~600+ digits).
#[cold]
#[inline(never)]
fn bignum_term_large(env: Env, raw: &str) -> Option<ERL_NIF_TERM> {
    rustler::BigInt::parse_bytes(raw.as_bytes(), 10).map(|big| big.encode(env).as_c_arg())
}

impl<'de, 'a, 'b> JsonVisitor<'de> for TermBuilder<'a, 'b> {
    #[inline]
    fn visit_dom_start(&mut self) -> bool {
        true
    }

    #[inline]
    fn visit_dom_end(&mut self) -> bool {
        true
    }

    #[inline]
    fn visit_null(&mut self) -> bool {
        self.push(atoms::nil().as_c_arg());
        true
    }

    #[inline]
    fn visit_bool(&mut self, val: bool) -> bool {
        self.push(if val {
            atoms::r#true().as_c_arg()
        } else {
            atoms::r#false().as_c_arg()
        });
        true
    }

    #[inline]
    fn visit_i64(&mut self, val: i64) -> bool {
        let t = unsafe { enif_make_int64(self.env.as_c_arg(), val) };
        self.push(t);
        true
    }

    #[inline]
    fn visit_u64(&mut self, val: u64) -> bool {
        let t = unsafe { enif_make_uint64(self.env.as_c_arg(), val) };
        self.push(t);
        true
    }

    #[inline]
    fn visit_f64(&mut self, val: f64) -> bool {
        let t = unsafe { enif_make_double(self.env.as_c_arg(), val) };
        self.push(t);
        true
    }

    /// Integer literal beyond i64/u64 range: an exact Erlang bignum from the
    /// raw digits under `Overflow::Bignum`, else the float a `Value` holds.
    #[inline]
    fn visit_overflow_int(&mut self, raw: &str, as_f64: f64) -> bool {
        let exact = match self.overflow {
            Overflow::Bignum => bignum_term(self.env, raw),
            Overflow::Float => None,
        };
        let t = exact.unwrap_or_else(|| unsafe { enif_make_double(self.env.as_c_arg(), as_f64) });
        self.push(t);
        true
    }

    #[inline]
    fn visit_str(&mut self, value: &str) -> bool {
        let t = self.str_term(value);
        self.push(t);
        true
    }

    // Always, like `visit_array_end` and `key_term`: `replay` is a second
    // caller, and outlining the three cost decode/1 4%.
    #[inline(always)]
    fn visit_key(&mut self, key: &str) -> bool {
        let len = key.len();
        let offset = self.input.offset_within(key);
        let wide = matches!(offset, Some(o) if self.input.len - o >= 8);
        // First 8 key bytes, little-endian and zero-padded.
        let prefix = if wide {
            let w = unsafe { (key.as_ptr() as *const u64).read_unaligned() };
            if len < 8 {
                w & ((1u64 << (len * 8)) - 1)
            } else {
                w
            }
        } else {
            let mut b = [0u8; 8];
            let n = len.min(8);
            b[..n].copy_from_slice(&key.as_bytes()[..n]);
            u64::from_le_bytes(b)
        };
        self.ords.push(KeyOrd {
            prefix: prefix.swap_bytes(),
            ptr: if offset.is_some() {
                key.as_ptr()
            } else {
                std::ptr::null()
            },
            len,
        });
        let t = self.key_term(key, wide, prefix);
        self.key_terms.push(t);
        true
    }

    #[inline]
    fn visit_array_start(&mut self, _hint: usize) -> bool {
        if self.frames.len() >= MAX_DEPTH as usize {
            self.too_deep = true;
            return false;
        }
        self.frames.push(Frame {
            values: self.values.len(),
            ords: self.ords.len(),
        });
        true
    }

    #[inline(always)]
    fn visit_array_end(&mut self, _len: usize) -> bool {
        let start = match self.frames.pop() {
            Some(f) => f.values,
            None => return false,
        };
        let count = (self.values.len() - start) as u32;
        let list = unsafe {
            enif_make_list_from_array(self.env.as_c_arg(), self.values[start..].as_ptr(), count)
        };
        self.values.truncate(start);
        self.values.push(list);
        true
    }

    #[inline]
    fn visit_object_start(&mut self, _hint: usize) -> bool {
        if self.frames.len() >= MAX_DEPTH as usize {
            self.too_deep = true;
            return false;
        }
        self.frames.push(Frame {
            values: self.values.len(),
            ords: self.ords.len(),
        });
        true
    }

    #[inline]
    fn visit_object_end(&mut self, _len: usize) -> bool {
        let frame = match self.frames.pop() {
            Some(f) => f,
            None => return false,
        };
        let start = frame.values;
        let keys = &self.key_terms[frame.ords..];
        let vals = &self.values[start..];
        let map = match build_map(self.env, keys, vals, &self.ords[frame.ords..]) {
            Some(map) => map,
            None => {
                self.dropped_members = true;
                map_last_wins(self.env, keys, vals)
            }
        };
        self.key_terms.truncate(frame.ords);
        self.ords.truncate(frame.ords);
        self.values.truncate(start);
        self.values.push(map);
        true
    }
}

/// Runs `f` with a term builder whose strings may borrow from `source`, on
/// this scheduler thread's reused stacks and key cache.
pub(crate) fn with_term_builder<'a, R>(
    env: Env<'a>,
    source: Source,
    strings: Strings,
    overflow: Overflow,
    f: impl for<'b> FnOnce(&mut TermBuilder<'a, 'b>) -> R,
) -> R {
    let input = match source {
        Source::Binary { term, bytes } => InputRef {
            term,
            base: bytes.as_ptr(),
            len: bytes.len(),
            document: None,
        },
        Source::Document(document) => InputRef {
            term: 0,
            base: document.bytes().as_ptr(),
            len: document.bytes().len(),
            document: Some(document),
        },
    };
    DECODE_BUFS.with(|cell| {
        let mut bufs = cell.borrow_mut();
        let DecodeBufs {
            values,
            frames,
            key_terms,
            ords,
            keys,
        } = &mut *bufs;
        // Emptied on entry as well as on exit: where panics unwind (debug
        // builds; release aborts), rustler catches one out of `f`, skipping
        // the release below and leaving the next build a dead env's terms.
        values.clear();
        frames.clear();
        key_terms.clear();
        ords.clear();
        keys.next_epoch();
        let mut builder = TermBuilder {
            env,
            input,
            strings,
            overflow,
            borrowed: 0,
            dropped_members: false,
            values,
            frames,
            key_terms,
            ords,
            keys,
            too_deep: false,
        };
        let result = f(&mut builder);
        // The terms live in the environment; the handles here are dead now.
        release(builder.values);
        release(builder.frames);
        release(builder.key_terms);
        release(builder.ords);
        result
    })
}

/// Build the `{:error, _}` term for a failed parse through `builder`.
pub(crate) fn builder_error_term<'a>(
    env: Env<'a>,
    builder: &TermBuilder,
    err: &sonic_rs::Error,
) -> Term<'a> {
    if builder.too_deep {
        make_tuple2(
            env,
            atoms::error().as_c_arg(),
            atoms::nesting_too_deep().as_c_arg(),
        )
    } else {
        parse_error_term(env, err)
    }
}

pub fn decode_to_term<'a>(env: Env<'a>, input_term: ERL_NIF_TERM, bytes: &[u8]) -> Term<'a> {
    let source = Source::Binary {
        term: input_term,
        bytes,
    };
    with_term_builder(env, source, Strings::Borrow, Overflow::Bignum, |builder| {
        match sonic_rs::parse_into_visitor(bytes, builder) {
            Ok(()) => match builder.values.first() {
                Some(&root) => make_tuple2(env, atoms::ok().as_c_arg(), root),
                None => make_tuple2(
                    env,
                    atoms::error().as_c_arg(),
                    "empty document".encode(env).as_c_arg(),
                ),
            },
            Err(e) => builder_error_term(env, builder, &e),
        }
    })
}
