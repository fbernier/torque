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
use std::mem::MaybeUninit;

use crate::atoms;
use crate::map_order::{order_members, prefix_be, FLATMAP_LIMIT, MIN_ORDERED_MEMBERS};
use crate::nif_util::{make_tuple2, map_from_arrays};
use crate::types::MAX_DEPTH;

const STACK_SIZE: usize = 64;

/// Cap on the retained thread-local value stack (in terms, 8 bytes each ≈ 1 MB),
/// so a one-off huge document doesn't pin a large allocation on a scheduler
/// thread indefinitely. Mirrors the encoder's `BUF_RETAIN_CAP`.
const VALUES_RETAIN_CAP: usize = 1 << 17;

/// Equivalent retention cap for key-order entries.
const KEY_ORDS_RETAIN_CAP: usize = VALUES_RETAIN_CAP / 2;

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
    /// First 8 key bytes, zero-padded. For keys of ≤ 8 bytes this is the whole
    /// content, so prefix + len equality needs no byte compare on a hit.
    prefix: u64,
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
/// On OTP 28+ this matches what `enif_make_sub_binary` does anyway (slices
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

/// Sort metadata captured while an object key still points into the input.
/// Escaped keys use parser scratch space and disable pre-sorting for that object.
/// Metadata stops at `FLATMAP_LIMIT`, where ERTS switches to hash maps.
#[derive(Clone, Copy)]
struct KeyOrd {
    prefix: u64,
    off: u32,
    len: u32,
}

impl KeyOrd {
    /// Placeholder for a key whose bytes cannot be borrowed from the input.
    const UNSORTABLE: KeyOrd = KeyOrd {
        prefix: 0,
        off: 0,
        len: 0,
    };

    /// Full comparison for keys with the same prefix.
    #[cold]
    #[inline(never)]
    fn slow_lt(&self, other: &KeyOrd, base: *const u8) -> bool {
        unsafe {
            std::slice::from_raw_parts(base.add(self.off as usize), self.len as usize)
                < std::slice::from_raw_parts(base.add(other.off as usize), other.len as usize)
        }
    }
}

/// Saved stack positions and ordering state for an open container.
#[derive(Clone, Copy)]
struct Frame {
    values: usize,
    keys: u32,
    unsortable: u32,
    members: u32,
}

struct DecodeBufs {
    values: Vec<ERL_NIF_TERM>,
    frames: Vec<Frame>,
    key_ords: Vec<KeyOrd>,
    keys: KeyCache,
}

thread_local! {
    /// Scratch buffers reused by each scheduler thread.
    static DECODE_BUFS: RefCell<DecodeBufs> = RefCell::new(DecodeBufs {
        values: Vec::with_capacity(64),
        frames: Vec::with_capacity(16),
        key_ords: Vec::with_capacity(32),
        keys: KeyCache::new(),
    });
}

struct InputRef {
    term: ERL_NIF_TERM,
    base: *const u8,
    len: usize,
    /// Bound for a key the decoder wants to borrow: the input length, or 0 for
    /// an input too large to address with `KeyOrd`'s u32 offsets (sonic-rs
    /// rejects >2 GB, so that never happens).
    borrow_limit: usize,
    /// Exclusive upper bound for offsets with eight readable input bytes.
    wide_limit: usize,
}

impl InputRef {
    /// Offset of `s` when its entire span lies within `limit`.
    #[inline]
    fn offset_within(&self, s: &str, limit: usize) -> Option<usize> {
        let offset = (s.as_ptr() as usize).checked_sub(self.base as usize)?;
        let room = limit.checked_sub(offset)?;
        (s.len() <= room).then_some(offset)
    }
}

/// Zero-padded key prefix used by the cache and key ordering.
///
/// # Safety
///
/// `ptr` must cover `len` bytes, and eight bytes when `wide` is true.
#[inline]
unsafe fn key_prefix_le(ptr: *const u8, len: usize, wide: bool) -> u64 {
    if !wide {
        return tail_prefix_le(std::slice::from_raw_parts(ptr, len));
    }
    let eight = u64::from_le_bytes(ptr.cast::<[u8; 8]>().read_unaligned());
    if len < 8 {
        eight & ((1u64 << (len * 8)) - 1)
    } else {
        eight
    }
}

/// Handles keys too close to the input end for a wide read.
#[cold]
#[inline(never)]
fn tail_prefix_le(bytes: &[u8]) -> u64 {
    prefix_be(bytes).swap_bytes()
}

struct TermBuilder<'a, 'b> {
    env: Env<'a>,
    input: InputRef,
    /// Postfix value stack: completed terms plus the open containers' children.
    /// Borrowed from a reused thread-local buffer (see `DECODE_BUFS`).
    values: &'b mut Vec<ERL_NIF_TERM>,
    /// Start of each open container in `values`.
    frames: &'b mut Vec<Frame>,
    /// Sort metadata for open object members.
    key_ords: &'b mut Vec<KeyOrd>,
    keys: &'b mut KeyCache,
    /// Running unsortable-key count, saved and restored with each frame.
    unsortable: u32,
    /// Running object member count, saved and restored with each frame.
    members: u32,
    too_deep: bool,
}

impl<'a, 'b> TermBuilder<'a, 'b> {
    #[inline]
    fn push(&mut self, term: ERL_NIF_TERM) {
        self.values.push(term);
    }

    /// Sub-binary (zero-copy) when the str lives in the input buffer, else copy
    /// (escaped strings are unescaped into the parser's scratch buffer).
    #[inline]
    fn str_term(&self, s: &str) -> ERL_NIF_TERM {
        if let Some(offset) = self.input.offset_within(s, self.input.len) {
            return unsafe {
                enif_make_sub_binary(self.env.as_c_arg(), self.input.term, offset, s.len())
            };
        }
        let mut binary = NewBinary::new(self.env, s.len());
        binary.as_mut_slice().copy_from_slice(s.as_bytes());
        let term: Term = binary.into();
        term.as_c_arg()
    }

    /// Builds an object-key term and records its sort order when possible.
    /// Escaped keys bypass the cache and disable pre-sorting for their object.
    #[inline]
    fn key_term(&mut self, s: &str) -> ERL_NIF_TERM {
        let ptr = s.as_ptr();
        let len = s.len();
        let mut prefix = 0u64;
        let mut offset = 0usize;
        let mut borrowed = false;
        if let Some(at) = self.input.offset_within(s, self.input.borrow_limit) {
            offset = at;
            borrowed = true;
            // SAFETY: the key is in the input, and `wide_limit` proves whether
            // eight bytes are readable from this offset.
            prefix = unsafe { key_prefix_le(ptr, len, offset < self.input.wide_limit) };
        }

        // ERTS hash maps do not benefit from term-order metadata.
        self.members += 1;
        if self.members <= FLATMAP_LIMIT as u32 {
            if borrowed {
                self.key_ords.push(KeyOrd {
                    prefix: prefix.swap_bytes(),
                    off: offset as u32,
                    len: len as u32,
                });
            } else {
                self.unsortable += 1;
                self.key_ords.push(KeyOrd::UNSORTABLE);
            }
        }

        if !borrowed || len == 0 || len > KEY_CACHE_MAX_LEN || self.keys.debit > KEY_CACHE_BYPASS_AT
        {
            return self.str_term(s);
        }
        let h = (prefix ^ (len as u64)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let entry = &mut self.keys.entries[(h >> 56) as usize & (KEY_CACHE_SLOTS - 1)];
        if entry.epoch == self.keys.epoch
            && entry.prefix == prefix
            && entry.len == len as u32
            && (len <= 8
                || unsafe {
                    std::slice::from_raw_parts(entry.ptr.add(8), len - 8)
                        == std::slice::from_raw_parts(ptr.add(8), len - 8)
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
            len: len as u32,
            epoch: self.keys.epoch,
        };
        term
    }
}

/// De-interleaves object children and pre-sorts eligible flatmap keys.
#[inline]
fn build_map(env: Env, kv: &[ERL_NIF_TERM], ords: &[KeyOrd], base: *const u8) -> ERL_NIF_TERM {
    let pairs = kv.len() / 2;
    if pairs > STACK_SIZE {
        let mut keys = Vec::with_capacity(pairs);
        let mut vals = Vec::with_capacity(pairs);
        for i in 0..pairs {
            keys.push(kv[2 * i]);
            vals.push(kv[2 * i + 1]);
        }
        return make_map(env, &keys, &vals, kv);
    }

    let mut keys: [MaybeUninit<ERL_NIF_TERM>; STACK_SIZE] = [MaybeUninit::uninit(); STACK_SIZE];
    let mut vals: [MaybeUninit<ERL_NIF_TERM>; STACK_SIZE] = [MaybeUninit::uninit(); STACK_SIZE];
    let mut prefixes: [MaybeUninit<u64>; FLATMAP_LIMIT] = [MaybeUninit::uninit(); FLATMAP_LIMIT];
    let permuted =
        ords.len() == pairs && (MIN_ORDERED_MEMBERS..=FLATMAP_LIMIT).contains(&pairs) && {
            for (slot, o) in prefixes[..pairs].iter_mut().zip(ords) {
                slot.write(o.prefix);
            }
            // SAFETY: prefixes[..pairs] was initialized above.
            let prefixes = unsafe { std::slice::from_raw_parts(prefixes.as_ptr().cast(), pairs) };
            order_members(
                prefixes,
                |a, b| ords[a].slow_lt(&ords[b], base),
                |perm| {
                    for (i, &member) in perm.iter().enumerate() {
                        let s = member as usize;
                        keys[i].write(kv[2 * s]);
                        vals[i].write(kv[2 * s + 1]);
                    }
                },
            )
        };
    if !permuted {
        for i in 0..pairs {
            keys[i].write(kv[2 * i]);
            vals[i].write(kv[2 * i + 1]);
        }
    }

    // SAFETY: keys[..pairs] and vals[..pairs] were initialized.
    unsafe {
        make_map(
            env,
            std::slice::from_raw_parts(keys.as_ptr() as *const ERL_NIF_TERM, pairs),
            std::slice::from_raw_parts(vals.as_ptr() as *const ERL_NIF_TERM, pairs),
            kv,
        )
    }
}

/// Builds an ERTS map, inserting in source order if array construction rejects it.
#[inline]
fn make_map(
    env: Env,
    keys: &[ERL_NIF_TERM],
    vals: &[ERL_NIF_TERM],
    kv: &[ERL_NIF_TERM],
) -> ERL_NIF_TERM {
    unsafe {
        let mut map: ERL_NIF_TERM = 0;
        if map_from_arrays(env, keys.as_ptr(), vals.as_ptr(), keys.len(), &mut map) {
            map
        } else {
            map_from_source(env, kv)
        }
    }
}

/// Source-order insertion preserves last-value-wins for duplicate keys.
#[cold]
#[inline(never)]
fn map_from_source(env: Env, kv: &[ERL_NIF_TERM]) -> ERL_NIF_TERM {
    unsafe {
        let mut map = enif_make_new_map(env.as_c_arg());
        for member in kv.as_chunks::<2>().0 {
            let mut new_map: ERL_NIF_TERM = 0;
            enif_make_map_put(env.as_c_arg(), map, member[0], member[1], &mut new_map);
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

    /// Integer literal beyond i64/u64 range: build an exact Erlang bignum from
    /// the raw digits instead of degrading to a lossy f64.
    #[inline]
    fn visit_overflow_int(&mut self, raw: &str, as_f64: f64) -> bool {
        let t = bignum_term(self.env, raw)
            .unwrap_or_else(|| unsafe { enif_make_double(self.env.as_c_arg(), as_f64) });
        self.push(t);
        true
    }

    #[inline]
    fn visit_str(&mut self, value: &str) -> bool {
        let t = self.str_term(value);
        self.push(t);
        true
    }

    /// `key_term` also records the key's sort order (see its docs).
    #[inline]
    fn visit_key(&mut self, key: &str) -> bool {
        let t = self.key_term(key);
        self.push(t);
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
            keys: self.key_ords.len() as u32,
            unsortable: self.unsortable,
            members: self.members,
        });
        self.members = 0;
        true
    }

    #[inline]
    fn visit_array_end(&mut self, _len: usize) -> bool {
        let frame = match self.frames.pop() {
            Some(f) => f,
            None => return false,
        };
        let start = frame.values;
        let count = (self.values.len() - start) as u32;
        let list = unsafe {
            enif_make_list_from_array(self.env.as_c_arg(), self.values[start..].as_ptr(), count)
        };
        // Arrays hold no keys of their own, but an object nested inside one
        // may have bumped `unsortable`; rewind so enclosing objects are judged
        // only on their own keys.
        self.unsortable = frame.unsortable;
        self.members = frame.members;
        self.key_ords.truncate(frame.keys as usize);
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
            keys: self.key_ords.len() as u32,
            unsortable: self.unsortable,
            members: self.members,
        });
        self.members = 0;
        true
    }

    #[inline]
    fn visit_object_end(&mut self, _len: usize) -> bool {
        let frame = match self.frames.pop() {
            Some(f) => f,
            None => return false,
        };
        let start = frame.values;
        // An escaped key can't be borrowed for comparison, so give up on
        // reordering the object that contains it and let ERTS order that one.
        // Only this object's own keys count: `unsortable` is a running total,
        // so it is rewound on close, or a single escaped key deep in the tree
        // would disqualify every object enclosing it — whose own key metadata
        // is still perfectly good.
        let ords: &[KeyOrd] = if self.unsortable == frame.unsortable {
            &self.key_ords[frame.keys as usize..]
        } else {
            &[]
        };
        let map = build_map(self.env, &self.values[start..], ords, self.input.base);
        self.unsortable = frame.unsortable;
        self.members = frame.members;
        self.key_ords.truncate(frame.keys as usize);
        self.values.truncate(start);
        self.values.push(map);
        true
    }
}

pub fn decode_to_term<'a>(env: Env<'a>, input_term: ERL_NIF_TERM, bytes: &[u8]) -> Term<'a> {
    DECODE_BUFS.with(|cell| {
        let mut bufs = cell.borrow_mut();
        let DecodeBufs {
            values,
            frames,
            key_ords,
            keys,
        } = &mut *bufs;
        // Clear state left by an unwind; successful calls already leave it empty.
        values.clear();
        frames.clear();
        key_ords.clear();
        keys.next_epoch();
        let mut builder = TermBuilder {
            env,
            input: InputRef {
                term: input_term,
                base: bytes.as_ptr(),
                len: bytes.len(),
                borrow_limit: if bytes.len() <= u32::MAX as usize {
                    bytes.len()
                } else {
                    0
                },
                wide_limit: bytes.len().saturating_sub(7),
            },
            values,
            frames,
            key_ords,
            keys,
            unsortable: 0,
            members: 0,
            too_deep: false,
        };

        let result = match sonic_rs::parse_into_visitor(bytes, &mut builder) {
            Ok(()) => match builder.values.first() {
                Some(&root) => make_tuple2(env, atoms::ok().as_c_arg(), root),
                None => make_tuple2(
                    env,
                    atoms::error().as_c_arg(),
                    "empty document".encode(env).as_c_arg(),
                ),
            },
            Err(e) => {
                if builder.too_deep {
                    make_tuple2(
                        env,
                        atoms::error().as_c_arg(),
                        atoms::nesting_too_deep().as_c_arg(),
                    )
                } else {
                    make_tuple2(
                        env,
                        atoms::error().as_c_arg(),
                        format!("{}", e).encode(env).as_c_arg(),
                    )
                }
            }
        };

        // Release exceptional growth before returning the buffers to thread-local storage.
        builder.values.clear();
        builder.frames.clear();
        builder.key_ords.clear();
        if builder.values.capacity() > VALUES_RETAIN_CAP {
            builder.values.shrink_to(VALUES_RETAIN_CAP);
        }
        if builder.key_ords.capacity() > KEY_ORDS_RETAIN_CAP {
            builder.key_ords.shrink_to(KEY_ORDS_RETAIN_CAP);
        }
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input_of(doc: &[u8]) -> InputRef {
        InputRef {
            term: 0,
            base: doc.as_ptr(),
            len: doc.len(),
            borrow_limit: doc.len(),
            wide_limit: doc.len().saturating_sub(7),
        }
    }

    #[test]
    fn a_span_is_inside_the_input_only_when_all_of_it_is() {
        let doc = b"{\"ab\":1}";
        let input = input_of(doc);
        let at =
            |off: usize, len: usize| std::str::from_utf8(&doc[off..off + len]).expect("fixture");

        for off in 0..doc.len() {
            for len in 0..=(doc.len() - off) {
                assert_eq!(
                    input.offset_within(at(off, len), input.borrow_limit),
                    Some(off),
                    "{off}/{len}"
                );
            }
        }

        assert_eq!(
            input.offset_within(at(doc.len(), 0), input.borrow_limit),
            Some(doc.len())
        );

        let scratch = String::from("{\"ab\":1}");
        assert_eq!(
            input.offset_within(scratch.as_str(), input.borrow_limit),
            None
        );
    }

    #[test]
    fn spans_outside_the_input_are_rejected_from_both_sides() {
        let backing = b"HEAD{\"ab\":1}TAIL";
        let input = InputRef {
            term: 0,
            base: backing[4..].as_ptr(),
            len: 8,
            borrow_limit: 8,
            wide_limit: 1,
        };
        let at = |r: std::ops::Range<usize>| std::str::from_utf8(&backing[r]).unwrap();

        assert_eq!(
            input.offset_within(at(4..12), 8),
            Some(0),
            "exactly the input"
        );
        assert_eq!(
            input.offset_within(at(12..12), 8),
            Some(8),
            "empty at the end"
        );
        assert_eq!(input.offset_within(at(0..4), 8), None, "entirely before");
        assert_eq!(
            input.offset_within(at(3..5), 8),
            None,
            "straddles the start"
        );
        assert_eq!(input.offset_within(at(4..13), 8), None, "runs past the end");
        assert_eq!(input.offset_within(at(12..16), 8), None, "entirely after");
        assert_eq!(input.offset_within(at(4..5), 0), None, "zero limit");
    }

    /// Covers the unaligned fast path and tail fallback at every input offset.
    #[test]
    fn the_wide_load_agrees_with_a_byte_wise_prefix_everywhere() {
        // Keys of every length that matters, and bytes that matter: multi-byte
        // UTF-8, an embedded NUL, and the high bytes that would sign-extend if
        // anything treated them as signed.
        let mut doc = Vec::new();
        for chunk in [
            b"a".as_slice(),
            b"ab",
            "\u{00e9}\u{4e2d}\u{1f600}".as_bytes(),
            b"abcdefg",
            b"abcdefgh",
            b"abcdefghi",
            b"k\0v",
            &[0xff; 9],
            &[0x80, 0x00, 0x7f, 0xff],
            &[b'x'; 64],
        ] {
            doc.extend_from_slice(chunk);
        }
        let wide_limit = doc.len().saturating_sub(7);

        for offset in 0..doc.len() {
            for len in 0..=(doc.len() - offset).min(64) {
                let key = &doc[offset..offset + len];
                let wide = offset < wide_limit;
                // SAFETY: offset + len is inside doc, and `wide` is exactly the
                // test the decoder makes before reading eight bytes.
                let got = unsafe { key_prefix_le(doc[offset..].as_ptr(), len, wide) };
                assert_eq!(
                    got,
                    prefix_be(key).swap_bytes(),
                    "offset {offset} len {len} wide {wide}"
                );
            }
        }
    }
}
