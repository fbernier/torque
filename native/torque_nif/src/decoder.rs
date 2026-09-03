use ahash::AHashMap;

use crate::atoms;
use crate::native_decode;
use crate::nif_util::{
    make_tuple2, make_tuple3, map_from_arrays, timeslice_percent, MapEntries, BYTES_PER_REDUCTION,
    REDUCTION_COUNT,
};
use crate::types::{value_to_term, MAX_DEPTH};
use crate::ParsedDocument;
use rustler::sys::{enif_make_list_from_array, enif_make_sub_binary, ERL_NIF_TERM};
use rustler::{
    schedule, Binary, Encoder, Env, ListIterator, NewBinary, NifResult, ResourceArc, Term,
};
use sonic_rs::JsonValueTrait;

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

/// Members a lookup may scan per reduction node charged.
const MEMBERS_PER_NODE: usize = 32;

/// Members indexed per reduction node charged.
const INDEXED_MEMBERS_PER_NODE: usize = 2;

/// Bytes of key material per reduction node charged.
const KEY_BYTES_PER_NODE: usize = 5;

/// Raw work counters for one NIF call.
///
/// Lookup cost is counted in the units the work is done in — comparisons,
/// key bytes, indexed members — and converted to built-term units once, when
/// the call reports. Converting per lookup discarded every remainder: a batch
/// of 2047 scans over 31-member objects charged nothing at all for its 63k
/// comparisons, because each one rounded to zero on its own.
#[derive(Default)]
pub struct Work {
    /// Terms built and results emitted. Already exact, never rounded.
    pub nodes: usize,
    /// Key comparisons made by linear scans.
    compared: usize,
    /// Members hashed into an index.
    indexed_members: usize,
    /// Key and pointer bytes read: compared, hashed, or split.
    bytes: usize,
}

impl Work {
    /// Built-term units for everything counted so far.
    #[inline]
    fn nodes(&self) -> usize {
        self.nodes
            + self.compared / MEMBERS_PER_NODE
            + self.indexed_members / INDEXED_MEMBERS_PER_NODE
            + self.bytes / KEY_BYTES_PER_NODE
    }

    /// Charges `members` key comparisons that read `bytes` of key.
    #[inline]
    fn scanned(&mut self, members: usize, bytes: usize) {
        self.compared += members;
        self.bytes += bytes;
    }

    /// Charges indexing `members` whose keys hash `bytes`.
    #[inline]
    fn indexed(&mut self, members: usize, bytes: usize) {
        self.indexed_members += members;
        self.bytes += bytes;
    }

    /// Charges reading `bytes` of a lookup key or a JSON Pointer.
    #[inline]
    fn key_bytes(&mut self, bytes: usize) {
        self.bytes += bytes;
    }
}

/// Report term-building work from the get family to the scheduler. The
/// pointer lookup itself is sub-microsecond, but `value_to_term` cost is
/// proportional to the extracted subtree, which only the node counter sees.
#[inline]
fn consume_timeslice_nodes(env: Env, nodes: usize) {
    if nodes >= TIMESLICE_MIN_NODES {
        let reds = nodes / NODES_PER_REDUCTION;
        schedule::consume_timeslice(env, ((reds * 100 / REDUCTION_COUNT) as i32).clamp(1, 100));
    }
}

/// Smallest input worth reporting to the scheduler.
const TIMESLICE_MIN_BYTES: usize = 512;

/// Charges byte-proportional work to a normal scheduler.
#[inline]
fn consume_timeslice_bytes(env: Env, bytes: usize) {
    if bytes >= TIMESLICE_MIN_BYTES {
        schedule::consume_timeslice(env, timeslice_percent(bytes));
    }
}

/// Combines byte and term work before rounding the timeslice percentage.
#[inline]
fn mixed_timeslice_percent(bytes: usize, nodes: usize) -> Option<i32> {
    if bytes < TIMESLICE_MIN_BYTES && nodes < TIMESLICE_MIN_NODES {
        return None;
    }
    let reds = bytes / BYTES_PER_REDUCTION + nodes / NODES_PER_REDUCTION;
    Some(((reds * 100 / REDUCTION_COUNT) as i32).clamp(1, 100))
}

/// Charges combined byte and term work with one scheduler update.
#[inline]
fn consume_timeslice_mixed(env: Env, bytes: usize, nodes: usize) {
    if let Some(percent) = mixed_timeslice_percent(bytes, nodes) {
        schedule::consume_timeslice(env, percent);
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

    /// Result terms emitted, including scalars and missing values.
    #[inline]
    fn len(&self) -> usize {
        self.count
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

/// Looks up an object key with the document's duplicate-key policy.
///
/// Parsed objects expose a pair slice, so unique-key lookups scan forward and
/// ordinary lookups scan backward for the last value. Rust-built objects cannot
/// contain duplicates and use the regular `Value` lookup.
#[inline]
fn object_get<'v, I: ObjectIndex<'v>>(
    value: &'v sonic_rs::Value,
    key: &str,
    memo: &mut I,
    work: &mut Work,
) -> Option<&'v sonic_rs::Value> {
    let pairs = match value.as_pair_slice() {
        Some(pairs) => pairs,
        None => return value.get(key),
    };
    if pairs.len() < WIDE_OBJECT_MEMBERS {
        let scanned = ObjectMemo::scan(pairs, key, memo.unique_keys());
        scanned.charge(work);
        return scanned.hit;
    }
    memo.wide_lookup(pairs, key, work)
}

/// Objects at or above this width may earn a memoized index.
const WIDE_OBJECT_MEMBERS: usize = 128;

/// Lookup policy for wide objects.
pub trait ObjectIndex<'v> {
    /// Whether the caller promises object keys are unique.
    fn unique_keys(&self) -> bool;

    /// Looks up `key` and adds scan or index work to `work`.
    fn wide_lookup(
        &mut self,
        pairs: &'v [(sonic_rs::Value, sonic_rs::Value)],
        key: &str,
        work: &mut Work,
    ) -> Option<&'v sonic_rs::Value>;
}

/// Single-path policy: scan directly and retain no index between lookups.
pub struct NoIndex {
    unique_keys: bool,
}

impl NoIndex {
    #[inline]
    pub fn new(unique_keys: bool) -> Self {
        NoIndex { unique_keys }
    }
}

impl<'v> ObjectIndex<'v> for NoIndex {
    #[inline(always)]
    fn unique_keys(&self) -> bool {
        self.unique_keys
    }

    #[inline(always)]
    fn wide_lookup(
        &mut self,
        pairs: &'v [(sonic_rs::Value, sonic_rs::Value)],
        key: &str,
        work: &mut Work,
    ) -> Option<&'v sonic_rs::Value> {
        let scanned = ObjectMemo::scan(pairs, key, self.unique_keys);
        scanned.charge(work);
        scanned.hit
    }
}

/// Batch policy: allocate the memo only after reaching a wide object.
pub struct LazyMemo<'v> {
    unique_keys: bool,
    memo: Option<Box<ObjectMemo<'v>>>,
}

impl LazyMemo<'_> {
    #[inline]
    pub fn new(unique_keys: bool) -> Self {
        LazyMemo {
            unique_keys,
            memo: None,
        }
    }
}

impl<'v> ObjectIndex<'v> for LazyMemo<'v> {
    #[inline(always)]
    fn unique_keys(&self) -> bool {
        self.unique_keys
    }

    #[inline]
    fn wide_lookup(
        &mut self,
        pairs: &'v [(sonic_rs::Value, sonic_rs::Value)],
        key: &str,
        work: &mut Work,
    ) -> Option<&'v sonic_rs::Value> {
        let unique_keys = self.unique_keys;
        self.memo
            .get_or_insert_with(|| Box::new(ObjectMemo::new(unique_keys)))
            .lookup_wide(pairs, key, work)
    }
}

/// Per-batch cache of indexes for repeatedly visited wide objects.
///
/// Paths commonly alternate between a few objects in a chain. Two-way sets
/// retain those indexes, while probation prevents cold objects from evicting
/// them before paying the same scan cost.
pub struct ObjectMemo<'v> {
    slots: [MemoSlot<'v>; MEMO_SETS * MEMO_WAYS],
    /// New objects accumulate scan credit here before taking a resident way.
    candidates: [Candidate; MEMO_SETS * MEMO_WAYS],
    clock: u32,
    unique_keys: bool,
}

const MEMO_SETS: usize = 4;
const MEMO_WAYS: usize = 2;

/// Result and work observed during one linear scan.
struct Scanned<'v> {
    hit: Option<&'v sonic_rs::Value>,
    compared: usize,
    compared_bytes: usize,
}

/// Above this total key size, scans must also earn the bytes an index hashes.
const INDEX_KEY_BYTES: usize = 1024 * 1024;

/// Individual keys at this length use byte-counting comparisons.
const LONG_KEY_BYTES: usize = 256;

/// Whole scans above this potential byte count also track compared bytes.
const SCAN_BYTES_ABOVE: usize = 4 * 1024 * 1024;

impl Scanned<'_> {
    /// Charges this scan's comparisons and the key bytes they read.
    #[inline]
    fn charge(&self, work: &mut Work) {
        work.scanned(self.compared, self.compared_bytes);
    }
}

/// Whether observed scan work has paid for hashing `key_bytes` into an index.
/// Member credit is enough for ordinary objects; large key sets must also earn
/// their hashing cost in bytes actually compared.
#[inline]
fn affordable(key_bytes: u64, byte_credit: u64) -> bool {
    key_bytes <= INDEX_KEY_BYTES as u64 || byte_credit >= key_bytes
}

/// What an object has paid towards its index, and what it is being asked for.
/// Shared by the residents in the ways and the newcomers on probation, because
/// both are answering the same question: is this object's index worth building
/// yet, and of two that do not have one, which is further from it.
#[derive(Clone, Copy)]
struct Earning {
    /// Members scanned on this object's behalf, frozen once the index exists.
    credit: u64,
    /// Key bytes actually compared while earning the index.
    byte_credit: u64,
    /// Total key bytes, profiled once the member target is reached.
    key_bytes: Option<u64>,
    /// Member comparisons required before considering an index.
    target: u64,
}

impl Earning {
    #[inline]
    fn new(target: u64) -> Self {
        Earning {
            credit: 0,
            byte_credit: 0,
            key_bytes: None,
            target,
        }
    }

    /// Whether both the member and key-byte costs have been earned.
    #[inline]
    fn ready(&self) -> bool {
        self.credit >= self.target
            && match self.key_bytes {
                Some(bytes) => affordable(bytes, self.byte_credit),
                None => false,
            }
    }

    /// Progress as clamped member and byte fractions.
    #[inline]
    fn progress(&self) -> ((u128, u128), (u128, u128)) {
        let target = self.target.max(1);
        let members = (self.credit.min(target) as u128, target as u128);
        let bytes = match self.key_bytes {
            Some(k) => {
                let k = k.max(1);
                (self.byte_credit.min(k) as u128, k as u128)
            }
            None => (0, 1),
        };
        (members, bytes)
    }

    /// Whether this object is further from an index than `other`.
    #[inline]
    fn less_deserving(&self, other: &Self) -> bool {
        match (self.ready(), other.ready()) {
            (false, true) => true,
            (true, false) => false,
            _ => {
                let ((an, ad), (abn, abd)) = self.progress();
                let ((bn, bd), (bbn, bbd)) = other.progress();
                match (an * bd).cmp(&(bn * ad)) {
                    std::cmp::Ordering::Less => true,
                    std::cmp::Ordering::Greater => false,
                    std::cmp::Ordering::Equal => abn * bbd < bbn * abd,
                }
            }
        }
    }
}

/// An object waiting to earn admission to a full set.
#[derive(Clone, Copy)]
struct Candidate {
    base: *const u8,
    earning: Earning,
}

struct MemoSlot<'v> {
    /// Address of the object's pair slice; null means unused.
    base: *const u8,
    /// Logical time of the last lookup.
    used: u32,
    /// Work earned toward an index.
    earning: Earning,
    /// Winning pair index for each key once built.
    index: Option<AHashMap<&'v str, u32>>,
}

impl<'v> MemoSlot<'v> {
    #[inline]
    fn empty() -> Self {
        MemoSlot {
            base: std::ptr::null(),
            used: 0,
            earning: Earning::new(0),
            index: None,
        }
    }
}

/// Visits required before indexing an object of this width.
///
/// Index construction scales worse with width than a streaming scan. Keep
/// these steps aligned with the wide-object member sweep.
#[inline]
fn index_after_visits(members: usize) -> u32 {
    match members {
        0..256 => 4,
        256..1024 => 8,
        1024..4096 => 12,
        4096..16_384 => 24,
        _ => 28,
    }
}

impl<'v> ObjectMemo<'v> {
    #[inline]
    pub fn new(unique_keys: bool) -> Self {
        ObjectMemo {
            unique_keys,
            slots: std::array::from_fn(|_| MemoSlot::empty()),
            candidates: [Candidate {
                base: std::ptr::null(),
                earning: Earning::new(0),
            }; MEMO_SETS * MEMO_WAYS],
            clock: 0,
        }
    }

    /// Slot range selected from an object's pair-slice address.
    #[inline]
    fn set_of(base: *const u8) -> std::ops::Range<usize> {
        const PHI: u64 = 0x9E37_79B9_7F4A_7C15;
        let mixed = (base as u64 >> 5).wrapping_mul(PHI);
        let set = (mixed >> (64 - MEMO_SETS.trailing_zeros())) as usize;
        set * MEMO_WAYS..(set + 1) * MEMO_WAYS
    }

    #[inline(never)]
    fn lookup_wide(
        &mut self,
        pairs: &'v [(sonic_rs::Value, sonic_rs::Value)],
        key: &str,
        work: &mut Work,
    ) -> Option<&'v sonic_rs::Value> {
        let unique_keys = self.unique_keys;
        let base = pairs.as_ptr() as *const u8;
        // Credit comparisons made by this object, not lookups or total batch
        // size. Scans short-circuit, and a batch may spread its paths across
        // several objects.
        let target = Self::scan_budget(pairs.len());
        self.clock = self.clock.wrapping_add(1);
        let now = self.clock;
        let set = Self::set_of(base);
        let ways = &mut self.slots[set.clone()];

        if let Some(slot) = ways.iter_mut().find(|s| s.base == base) {
            slot.used = now;
            if slot.index.is_none() {
                if slot.earning.ready() {
                    let (index, hashed) = Self::build_index(pairs, unique_keys);
                    work.indexed(pairs.len(), hashed);
                    slot.index = Some(index);
                } else {
                    // Keep scanning until both member and key-byte costs are earned.
                    return Self::scan_and_price(pairs, key, unique_keys, &mut slot.earning, work);
                }
            }
            work.key_bytes(key.len());
            return slot
                .index
                .as_ref()
                .unwrap_or_else(|| unreachable!("built above"))
                .get(key)
                .map(|i| &pairs[*i as usize].1);
        }

        // An empty way admits the object immediately.
        if let Some(free) = ways.iter_mut().find(|s| s.base.is_null()) {
            free.base = base;
            free.used = now;
            free.earning = Earning::new(target);
            return Self::scan_and_price(pairs, key, unique_keys, &mut free.earning, work);
        }

        // A full set puts newcomers on probation before allowing eviction.
        let candidates = &mut self.candidates[set.clone()];
        let slot = match candidates.iter().position(|c| c.base == base) {
            Some(i) => i,
            None => candidates
                .iter()
                .position(|c| c.base.is_null())
                .unwrap_or_else(|| Self::least_deserving(candidates)),
        };
        if candidates[slot].base != base {
            candidates[slot] = Candidate {
                base,
                earning: Earning::new(target),
            };
        }
        if !candidates[slot].earning.ready() {
            return Self::scan_and_price(
                pairs,
                key,
                unique_keys,
                &mut candidates[slot].earning,
                work,
            );
        }

        let (index, hashed) = Self::build_index(pairs, unique_keys);
        work.indexed(pairs.len(), hashed);
        work.key_bytes(key.len());
        candidates[slot] = Candidate {
            base: std::ptr::null(),
            earning: Earning::new(0),
        };

        // Prefer the least advanced unindexed way; otherwise evict the LRU index.
        let ways = &mut self.slots[set];
        let victim = match ways
            .iter()
            .enumerate()
            .filter(|(_, s)| s.index.is_none())
            .reduce(|worst, s| {
                if s.1.earning.less_deserving(&worst.1.earning) {
                    s
                } else {
                    worst
                }
            })
            .map(|(i, _)| i)
        {
            Some(i) => i,
            None => ways
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| s.used)
                .map(|(i, _)| i)
                .unwrap_or_else(|| unreachable!("MEMO_WAYS is not zero")),
        };
        let victim = &mut ways[victim];
        victim.base = base;
        victim.used = now;
        victim.earning = Earning::new(target);
        victim.earning.credit = target;
        victim.index = Some(index);
        victim
            .index
            .as_ref()
            .unwrap_or_else(|| unreachable!("just built"))
            .get(key)
            .map(|i| &pairs[*i as usize].1)
    }

    /// Scans, records work, and profiles key size when the member target is met.
    #[inline]
    fn scan_and_price(
        pairs: &'v [(sonic_rs::Value, sonic_rs::Value)],
        key: &str,
        unique_keys: bool,
        earning: &mut Earning,
        work: &mut Work,
    ) -> Option<&'v sonic_rs::Value> {
        let over_cap = earning
            .key_bytes
            .is_some_and(|b| b > INDEX_KEY_BYTES as u64);
        let scanned = if over_cap {
            Self::scan_long(pairs, key, unique_keys)
        } else {
            Self::scan(pairs, key, unique_keys)
        };
        scanned.charge(work);
        earning.credit = earning.credit.saturating_add(scanned.compared as u64);
        earning.byte_credit = earning
            .byte_credit
            .saturating_add(scanned.compared_bytes as u64);
        if earning.credit >= earning.target && earning.key_bytes.is_none() {
            earning.key_bytes = Some(Self::profile_key_bytes(pairs, work));
        }
        scanned.hit
    }

    /// Total bytes in the object's keys.
    #[cold]
    #[inline(never)]
    fn profile_key_bytes(pairs: &'v [(sonic_rs::Value, sonic_rs::Value)], work: &mut Work) -> u64 {
        work.scanned(pairs.len(), 0);
        pairs
            .iter()
            .map(|(k, _)| k.as_node_str().map_or(0, str::len) as u64)
            .sum()
    }

    /// Candidate furthest from earning an index.
    fn least_deserving(candidates: &[Candidate]) -> usize {
        let mut worst = 0;
        for i in 1..candidates.len() {
            if candidates[i]
                .earning
                .less_deserving(&candidates[worst].earning)
            {
                worst = i;
            }
        }
        worst
    }

    /// Member comparisons required to amortize index construction.
    #[inline]
    fn scan_budget(members: usize) -> u64 {
        members as u64 * (index_after_visits(members) - 1) as u64
    }

    /// Builds the index and returns the number of key bytes hashed.
    fn build_index(
        pairs: &'v [(sonic_rs::Value, sonic_rs::Value)],
        unique_keys: bool,
    ) -> (AHashMap<&'v str, u32>, usize) {
        debug_assert!(pairs.len() <= u32::MAX as usize);
        let mut index = AHashMap::with_capacity(pairs.len());
        let mut bytes = 0usize;
        for (i, (k, _)) in pairs.iter().enumerate() {
            if let Some(name) = k.as_node_str() {
                bytes += name.len();
                if unique_keys {
                    index.entry(name).or_insert(i as u32);
                } else {
                    index.insert(name, i as u32);
                }
            }
        }
        (index, bytes)
    }

    /// Linear scan that stops at the winning match.
    #[inline]
    fn scan(
        pairs: &'v [(sonic_rs::Value, sonic_rs::Value)],
        key: &str,
        unique_keys: bool,
    ) -> Scanned<'v> {
        if key.len() > LONG_KEY_BYTES || pairs.len().saturating_mul(key.len()) > SCAN_BYTES_ABOVE {
            return Self::scan_long(pairs, key, unique_keys);
        }
        let len = pairs.len();
        let hit = if unique_keys {
            pairs
                .iter()
                .position(|(k, _)| k.as_node_str() == Some(key))
                .map(|i| (i, i + 1))
        } else {
            pairs
                .iter()
                .rposition(|(k, _)| k.as_node_str() == Some(key))
                .map(|i| (i, len - i))
        };
        match hit {
            Some((i, compared)) => Scanned {
                hit: Some(&pairs[i].1),
                compared,
                compared_bytes: 0,
            },
            None => Scanned {
                hit: None,
                compared: len,
                compared_bytes: 0,
            },
        }
    }

    /// Linear scan that also counts bytes reached by key comparisons.
    #[cold]
    #[inline(never)]
    fn scan_long(
        pairs: &'v [(sonic_rs::Value, sonic_rs::Value)],
        key: &str,
        unique_keys: bool,
    ) -> Scanned<'v> {
        let mut compared = 0usize;
        let mut compared_bytes = 0usize;
        let mut hit = None;
        if unique_keys {
            for (k, v) in pairs.iter() {
                compared += 1;
                if let Some(name) = k.as_node_str() {
                    if name.len() == key.len()
                        && Self::eq_counting(name.as_bytes(), key.as_bytes(), &mut compared_bytes)
                    {
                        hit = Some(v);
                        break;
                    }
                }
            }
        } else {
            for (k, v) in pairs.iter().rev() {
                compared += 1;
                if let Some(name) = k.as_node_str() {
                    if name.len() == key.len()
                        && Self::eq_counting(name.as_bytes(), key.as_bytes(), &mut compared_bytes)
                    {
                        hit = Some(v);
                        break;
                    }
                }
            }
        }
        Scanned {
            hit,
            compared,
            compared_bytes,
        }
    }

    /// Equal-length key comparison with byte accounting.
    #[inline]
    fn eq_counting(a: &[u8], b: &[u8], bytes: &mut usize) -> bool {
        let mut at = 0usize;
        let mut chunk = 64usize;
        while at < a.len() {
            // `at + chunk` rather than the sum: a key can be most of the
            // address space on a 32-bit target, and the sum can wrap where the
            // remaining length cannot.
            let end = at + chunk.min(a.len() - at);
            *bytes += end - at;
            if a[at..end] != b[at..end] {
                return false;
            }
            at = end;
            chunk = (chunk * 2).min(4096);
        }
        true
    }
}

/// Short pointers use byte splitting to avoid searcher setup. Long pointers
/// use `str::split`, which vectorizes delimiter search.
const SEARCHER_SPLIT_BYTES: usize = 64;

/// Follows one pointer segment.
#[inline]
fn descend<'v, I: ObjectIndex<'v>>(
    current: &'v sonic_rs::Value,
    segment: &str,
    memo: &mut I,
    work: &mut Work,
) -> Option<&'v sonic_rs::Value> {
    if current.is_array() {
        if let Some(index) = array_index(segment) {
            return current.get(index);
        }
    }
    // `~` is ASCII, so a byte scan is sufficient.
    if !segment.as_bytes().contains(&b'~') {
        return object_get(current, segment, memo, work);
    }
    if segment.len() > 512 {
        let unescaped = segment.replace("~1", "/").replace("~0", "~");
        return object_get(current, &unescaped, memo, work);
    }
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
    // SAFETY: the input is valid UTF-8 and substitutions emit only ASCII.
    let unescaped = unsafe { std::str::from_utf8_unchecked(&tmp[..out_len]) };
    object_get(current, unescaped, memo, work)
}

#[inline]
fn pointer_lookup<'v, I: ObjectIndex<'v>>(
    value: &'v sonic_rs::Value,
    path: &str,
    memo: &mut I,
    work: &mut Work,
) -> Option<&'v sonic_rs::Value> {
    work.key_bytes(path.len());
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
    if bytes.len() >= SEARCHER_SPLIT_BYTES {
        // Use the vectorized standard searcher for long pointers.
        for segment in path[1..].split('/') {
            current = descend(current, segment, memo, work)?;
        }
    } else {
        // ASCII delimiters preserve UTF-8 boundaries.
        for segment in bytes[1..].split(|&b| b == b'/') {
            // SAFETY: splitting valid UTF-8 on ASCII preserves valid segments.
            let segment = unsafe { std::str::from_utf8_unchecked(segment) };
            current = descend(current, segment, memo, work)?;
        }
    }
    Some(current)
}

fn do_parse(
    bytes: &[u8],
    unique_keys: bool,
) -> Result<ResourceArc<ParsedDocument>, sonic_rs::Error> {
    sonic_rs::from_slice::<sonic_rs::Value>(bytes)
        .map(|value| ResourceArc::new(ParsedDocument { value, unique_keys }))
}

/// Estimated work for rejected input. Every byte is validated as UTF-8, while
/// parsing and error construction reach only part of the document. Validation
/// is therefore charged at a fraction of full parsing work.
const UTF8_SCAN_RATIO: usize = 32;

/// Combines the full-input UTF-8 scan with syntax work through `reached`.
/// A rejection at the end is charged like a successful parse, without billing
/// the two passes twice.
#[inline]
pub(crate) fn timeslice_bytes(reached: usize, len: usize) -> usize {
    let reached = reached.min(len);
    let validated = len / UTF8_SCAN_RATIO;
    validated + reached.saturating_sub(reached / UTF8_SCAN_RATIO)
}

/// Uses the reported error offset as the syntax-work boundary. Sonic-rs may
/// prefer an invalid UTF-8 position over an earlier syntax fault, but building
/// the error also walks to that position, so it remains the useful work proxy.
#[inline]
pub(crate) fn bytes_scanned(err: &sonic_rs::Error, len: usize) -> usize {
    timeslice_bytes(err.offset().saturating_add(1), len)
}

/// Builds the public parse error. Recursion-limit failures use the stable
/// `:nesting_too_deep` atom; other sonic-rs errors retain their message.
#[inline]
fn parse_error_term<'a>(env: Env<'a>, err: &sonic_rs::Error) -> Term<'a> {
    let err_raw = atoms::error().as_c_arg();
    if err.is_recursion_limit() {
        make_tuple2(env, err_raw, atoms::nesting_too_deep().as_c_arg())
    } else {
        make_tuple2(env, err_raw, err.to_string().encode(env).as_c_arg())
    }
}

#[rustler::nif]
fn parse<'a>(env: Env<'a>, json: Binary) -> Term<'a> {
    let (result, scanned) = match do_parse(json.as_slice(), false) {
        Ok(resource) => (
            make_tuple2(env, atoms::ok().as_c_arg(), resource.encode(env).as_c_arg()),
            json.len(),
        ),
        Err(e) => (parse_error_term(env, &e), bytes_scanned(&e, json.len())),
    };
    consume_timeslice_bytes(env, scanned);
    result
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
    let (result, scanned) = match do_parse(json.as_slice(), unique_keys) {
        Ok(resource) => (
            make_tuple2(env, atoms::ok().as_c_arg(), resource.encode(env).as_c_arg()),
            json.len(),
        ),
        Err(e) => (parse_error_term(env, &e), bytes_scanned(&e, json.len())),
    };
    consume_timeslice_bytes(env, scanned);
    result
}

#[rustler::nif(schedule = "DirtyCpu")]
fn parse_opts_dirty<'a>(env: Env<'a>, json: Binary, unique_keys: bool) -> Term<'a> {
    match do_parse(json.as_slice(), unique_keys) {
        Ok(resource) => make_tuple2(env, atoms::ok().as_c_arg(), resource.encode(env).as_c_arg()),
        Err(e) => parse_error_term(env, &e),
    }
}

#[inline]
fn do_get<'a>(env: Env<'a>, doc: &ParsedDocument, path: &str, work: &mut Work) -> Term<'a> {
    let ok_raw = atoms::ok().as_c_arg();
    let err_raw = atoms::error().as_c_arg();
    let nsf_raw = atoms::no_such_field().as_c_arg();
    let ntd_raw = atoms::nesting_too_deep().as_c_arg();
    match pointer_lookup(&doc.value, path, &mut NoIndex::new(doc.unique_keys), work) {
        Some(value) => match value_to_term(env, value, MAX_DEPTH, &mut work.nodes) {
            Some(term) => make_tuple2(env, ok_raw, term.as_c_arg()),
            None => make_tuple2(env, err_raw, ntd_raw),
        },
        None => make_tuple2(env, err_raw, nsf_raw),
    }
}

#[rustler::nif]
fn get<'a>(env: Env<'a>, doc: ResourceArc<ParsedDocument>, path: &str) -> Term<'a> {
    let mut work = Work::default();
    let result = do_get(env, &doc, path, &mut work);
    consume_timeslice_nodes(env, work.nodes());
    result
}

#[rustler::nif(schedule = "DirtyCpu")]
fn get_dirty<'a>(env: Env<'a>, doc: ResourceArc<ParsedDocument>, path: &str) -> Term<'a> {
    let mut work = Work::default();
    do_get(env, &doc, path, &mut work)
}

/// Cached raw atoms for the per-path result tuples in `get_many`.
struct ResultAtoms {
    ok: ERL_NIF_TERM,
    err: ERL_NIF_TERM,
    nsf: ERL_NIF_TERM,
    ntd: ERL_NIF_TERM,
}

#[inline]
fn get_one_result<'v, I: ObjectIndex<'v>>(
    env: Env,
    doc: &'v ParsedDocument,
    path: &str,
    atoms: &ResultAtoms,
    work: &mut Work,
    memo: &mut I,
) -> ERL_NIF_TERM {
    match pointer_lookup(&doc.value, path, memo, work) {
        Some(value) => match value_to_term(env, value, MAX_DEPTH, &mut work.nodes) {
            Some(term) => make_tuple2(env, atoms.ok, term.as_c_arg()).as_c_arg(),
            None => make_tuple2(env, atoms.err, atoms.ntd).as_c_arg(),
        },
        None => make_tuple2(env, atoms.err, atoms.nsf).as_c_arg(),
    }
}

/// Look every path up and build the result list. Shared by the normal and
/// dirty NIFs: which scheduler runs it is the caller's decision, made from the
/// path count, and the work is the same either way.
#[inline]
fn do_get_many<'a>(
    env: Env<'a>,
    doc: &ParsedDocument,
    paths: ListIterator<'a>,
    work: &mut Work,
) -> NifResult<Term<'a>> {
    let result_atoms = ResultAtoms {
        ok: atoms::ok().as_c_arg(),
        err: atoms::error().as_c_arg(),
        nsf: atoms::no_such_field().as_c_arg(),
        ntd: atoms::nesting_too_deep().as_c_arg(),
    };
    let mut acc = TermAcc::new();
    let mut memo = LazyMemo::new(doc.unique_keys);

    for path_term in paths {
        // Non-binary (or non-UTF-8) path entries are caller bugs: badarg.
        let path: &str = path_term.decode()?;
        acc.push(get_one_result(
            env,
            doc,
            path,
            &result_atoms,
            work,
            &mut memo,
        ));
        // Charge every emitted result; `value_to_term` counts only container children.
        work.nodes += 1;
    }
    Ok(acc.into_list(env))
}

#[rustler::nif]
fn get_many<'a>(
    env: Env<'a>,
    doc: ResourceArc<ParsedDocument>,
    paths: ListIterator<'a>,
) -> NifResult<Term<'a>> {
    let mut work = Work::default();
    // Preserve work completed before an invalid path returns `badarg`.
    let result = do_get_many(env, &doc, paths, &mut work);
    consume_timeslice_nodes(env, work.nodes());
    result
}

#[rustler::nif(schedule = "DirtyCpu")]
fn get_many_dirty<'a>(
    env: Env<'a>,
    doc: ResourceArc<ParsedDocument>,
    paths: ListIterator<'a>,
) -> NifResult<Term<'a>> {
    let mut work = Work::default();
    do_get_many(env, &doc, paths, &mut work)
}

/// Implements `get_many_defaults/2` without an intermediate result list. Keys
/// and fallback terms come directly from the input map; missing, null, or
/// over-nested values retain their fallback.
#[inline]
fn do_get_many_defaults<'a>(
    env: Env<'a>,
    doc: &ParsedDocument,
    defaults: Term<'a>,
    work: &mut Work,
) -> NifResult<Term<'a>> {
    let count = defaults.map_size()?;
    let nil_raw = atoms::nil().as_c_arg();
    let entries = MapEntries::new(env, defaults).ok_or(rustler::Error::BadArg)?;

    let mut keys: Vec<ERL_NIF_TERM> = Vec::with_capacity(count);
    let mut vals: Vec<ERL_NIF_TERM> = Vec::with_capacity(count);
    let mut memo = LazyMemo::new(doc.unique_keys);

    for (key, default) in entries {
        // A non-binary (or non-UTF-8) key is a caller bug, the same one the
        // path list version reports.
        let path: &str = key.decode()?;
        let found = pointer_lookup(&doc.value, path, &mut memo, work)
            .and_then(|value| value_to_term(env, value, MAX_DEPTH, &mut work.nodes))
            .map(|term| term.as_c_arg())
            .filter(|term| *term != nil_raw);
        keys.push(key.as_c_arg());
        vals.push(found.unwrap_or_else(|| default.as_c_arg()));
        // One result per key, whether it came from the document or from the
        // caller's default, counted as it is made so a later bad key does not
        // erase the ones before it.
        work.nodes += 1;
    }
    let mut map: ERL_NIF_TERM = 0;
    // SAFETY: keys and vals hold `count` initialised terms each, and the keys
    // came from a map, so they are already unique.
    let built = unsafe { map_from_arrays(env, keys.as_ptr(), vals.as_ptr(), count, &mut map) };
    if built {
        Ok(unsafe { Term::new(env, map) })
    } else {
        Err(rustler::Error::BadArg)
    }
}

#[rustler::nif]
fn get_many_defaults<'a>(
    env: Env<'a>,
    doc: ResourceArc<ParsedDocument>,
    defaults: Term<'a>,
) -> NifResult<Term<'a>> {
    let mut work = Work::default();
    // Report work even when a later invalid path returns `badarg`.
    let result = do_get_many_defaults(env, &doc, defaults, &mut work);
    consume_timeslice_nodes(env, work.nodes());
    result
}

#[rustler::nif(schedule = "DirtyCpu")]
fn get_many_defaults_dirty<'a>(
    env: Env<'a>,
    doc: ResourceArc<ParsedDocument>,
    defaults: Term<'a>,
) -> NifResult<Term<'a>> {
    let mut work = Work::default();
    do_get_many_defaults(env, &doc, defaults, &mut work)
}

#[inline]
fn do_array_length<'a>(
    env: Env<'a>,
    doc: &ParsedDocument,
    path: &str,
    work: &mut Work,
) -> Term<'a> {
    // Include path and wide-object scan work, which the scalar result cannot represent.
    let len = pointer_lookup(&doc.value, path, &mut NoIndex::new(doc.unique_keys), work)
        .and_then(|value| value.as_value_slice())
        .map(|values| values.len());
    match len {
        Some(len) => unsafe {
            Term::new(
                env,
                rustler::sys::enif_make_uint64(env.as_c_arg(), len as u64),
            )
        },
        None => atoms::nil().to_term(env),
    }
}

#[rustler::nif]
fn array_length<'a>(env: Env<'a>, doc: ResourceArc<ParsedDocument>, path: &str) -> Term<'a> {
    let mut work = Work::default();
    let result = do_array_length(env, &doc, path, &mut work);
    consume_timeslice_nodes(env, work.nodes());
    result
}

#[rustler::nif(schedule = "DirtyCpu")]
fn array_length_dirty<'a>(env: Env<'a>, doc: ResourceArc<ParsedDocument>, path: &str) -> Term<'a> {
    let mut work = Work::default();
    do_array_length(env, &doc, path, &mut work)
}

#[rustler::nif]
fn decode<'a>(env: Env<'a>, json: Binary<'a>) -> Term<'a> {
    let input_term = json.encode(env).as_c_arg();
    let (result, scanned) = native_decode::decode_to_term(env, input_term, json.as_slice());
    consume_timeslice_bytes(env, scanned);
    result
}

#[rustler::nif(schedule = "DirtyCpu")]
fn decode_dirty<'a>(env: Env<'a>, json: Binary<'a>) -> Term<'a> {
    let input_term = json.encode(env).as_c_arg();
    native_decode::decode_to_term(env, input_term, json.as_slice()).0
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

/// Compiles JSON Pointers into reusable path segments and an extraction plan.
/// Returns the handle with the two quantities scheduler dispatch needs: the
/// path count and the total pointer bytes, so a compiled handle answers the
/// same dispatch question as the raw list it was built from.
#[rustler::nif]
fn compile_paths<'a>(
    env: Env<'a>,
    paths: ListIterator<'a>,
    unique_keys: bool,
    validate: bool,
) -> NifResult<Term<'a>> {
    do_compile_paths(env, paths, unique_keys, validate)
}

#[rustler::nif(schedule = "DirtyCpu")]
fn compile_paths_dirty<'a>(
    env: Env<'a>,
    paths: ListIterator<'a>,
    unique_keys: bool,
    validate: bool,
) -> NifResult<Term<'a>> {
    do_compile_paths(env, paths, unique_keys, validate)
}

#[inline]
fn do_compile_paths<'a>(
    env: Env<'a>,
    paths: ListIterator<'a>,
    unique_keys: bool,
    validate: bool,
) -> NifResult<Term<'a>> {
    let mut out = Vec::new();
    let mut bytes = 0usize;
    let mut plan = sonic_rs::extract::ExtractPlan::new();
    for pt in paths {
        // Non-binary (or non-UTF-8) entries are caller bugs: badarg. Silently
        // compiling them (e.g. as "") would return the whole document.
        let p: &str = pt.decode()?;
        // The raw-list walk sums pointer bytes, so sum the same thing here
        // rather than compiled segment bytes, which differ by separators and
        // by whatever `~` unescaping removed.
        bytes += p.len();
        let segs = compile_one(p)?;
        // The plan borrows segments and copies only new keys.
        plan.add_path(plan_segs(&segs));
        out.push(segs);
    }
    plan.finish();
    let count = out.len();
    let handle = ResourceArc::new(CompiledPaths {
        paths: out,
        plan,
        unique_keys,
        validate,
    })
    .encode(env);
    Ok(make_tuple3(
        env,
        handle.as_c_arg(),
        count.encode(env).as_c_arg(),
        bytes.encode(env).as_c_arg(),
    ))
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

/// Extract all compiled paths from an already-traversed `value` into a result
/// list term, substituting nil for missing fields and depth-exceeded values.
#[inline]
fn extract_compiled<'a>(
    env: Env<'a>,
    value: &sonic_rs::Value,
    compiled: &CompiledPaths,
    work: &mut Work,
) -> Term<'a> {
    let nil_raw = atoms::nil().as_c_arg();
    let mut acc = TermAcc::with_hint(compiled.paths.len());
    let mut memo = LazyMemo::new(compiled.unique_keys);
    for segs in compiled.paths.iter() {
        let r = match pointer_lookup_compiled(value, segs, &mut memo, work) {
            Some(v) => value_to_term(env, v, MAX_DEPTH, &mut work.nodes)
                .map(|t| t.as_c_arg())
                .unwrap_or(nil_raw),
            None => nil_raw,
        };
        acc.push(r);
    }
    work.nodes += acc.len();
    acc.into_list(env)
}

/// Extracts compiled paths with the tagged results returned by `get_many/2`.
#[inline]
fn extract_compiled_results<'a>(
    env: Env<'a>,
    value: &sonic_rs::Value,
    compiled: &CompiledPaths,
    work: &mut Work,
) -> Term<'a> {
    let result_atoms = ResultAtoms {
        ok: atoms::ok().as_c_arg(),
        err: atoms::error().as_c_arg(),
        nsf: atoms::no_such_field().as_c_arg(),
        ntd: atoms::nesting_too_deep().as_c_arg(),
    };
    let mut acc = TermAcc::with_hint(compiled.paths.len());
    let mut memo = LazyMemo::new(compiled.unique_keys);
    for segs in compiled.paths.iter() {
        let result = match pointer_lookup_compiled(value, segs, &mut memo, work) {
            Some(v) => match value_to_term(env, v, MAX_DEPTH, &mut work.nodes) {
                Some(term) => make_tuple2(env, result_atoms.ok, term.as_c_arg()).as_c_arg(),
                None => make_tuple2(env, result_atoms.err, result_atoms.ntd).as_c_arg(),
            },
            None => make_tuple2(env, result_atoms.err, result_atoms.nsf).as_c_arg(),
        };
        acc.push(result);
    }
    work.nodes += acc.len();
    acc.into_list(env)
}

/// Builds a string term, borrowing from `input` when requested and safe.
#[inline]
fn extracted_str_term(
    env: Env,
    input_term: ERL_NIF_TERM,
    input: &[u8],
    s: &str,
    borrow: bool,
) -> ERL_NIF_TERM {
    if borrow {
        if let Some(offset) = (s.as_ptr() as usize).checked_sub(input.as_ptr() as usize) {
            if let Some(room) = input.len().checked_sub(offset) {
                if s.len() <= room {
                    return unsafe {
                        enif_make_sub_binary(env.as_c_arg(), input_term, offset, s.len())
                    };
                }
            }
        }
    }
    let mut binary = NewBinary::new(env, s.len());
    binary.as_mut_slice().copy_from_slice(s.as_bytes());
    let term: Term = binary.into();
    term.as_c_arg()
}

/// Small inputs are always eligible for borrowed string results.
const BORROW_ANY_INPUT: usize = 4096;

/// Otherwise, extracted strings must cover this fraction of the input.
const BORROW_INPUT_FRACTION: usize = 4;

/// Borrows only when retaining the input is cheaper than copying the results.
#[inline]
fn borrow_input(alloc_len: usize, borrowed_len: impl FnOnce() -> usize) -> bool {
    alloc_len <= BORROW_ANY_INPUT
        || borrowed_len().saturating_mul(BORROW_INPUT_FRACTION) >= alloc_len
}

/// Parses once and returns the result term with the number of bytes reached.
/// Selected values are built directly; other regions are parsed or skipped
/// according to the compiled validation policy.
#[inline]
fn do_parse_get_many_nil<'a>(
    env: Env<'a>,
    input_term: ERL_NIF_TERM,
    bytes: &[u8],
    alloc_len: usize,
    compiled: &CompiledPaths,
    work: &mut Work,
) -> (Term<'a>, usize) {
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

    match sonic_rs::extract::extract(bytes, &compiled.plan, validate, keys) {
        Ok(values) => {
            let nil_raw = atoms::nil().as_c_arg();
            // Decided once for the batch, so a result list is either all
            // borrowed or all copied.
            let borrow = borrow_input(alloc_len, || {
                values
                    .iter()
                    .map(|v| match v {
                        Some(Extracted::Str(s)) => s.len(),
                        _ => 0,
                    })
                    .sum()
            });
            let mut acc = TermAcc::with_hint(values.len());
            for v in values.iter() {
                let t = match v {
                    // A string the parser never had to unescape is still in the
                    // caller's binary, so the term can point at it rather than
                    // pay a copy into a `Value` and another out of it - as long
                    // as keeping the input behind it is worth that.
                    Some(Extracted::Str(s)) => {
                        extracted_str_term(env, input_term, bytes, s, borrow)
                    }
                    Some(Extracted::Value(v)) => value_to_term(env, v, MAX_DEPTH, &mut work.nodes)
                        .map(|t| t.as_c_arg())
                        .unwrap_or(nil_raw),
                    None => nil_raw,
                };
                acc.push(t);
            }
            work.nodes += acc.len();
            (
                make_tuple2(env, atoms::ok().as_c_arg(), acc.into_list(env).as_c_arg()),
                bytes.len(),
            )
        }
        Err(e) => (parse_error_term(env, &e), bytes_scanned(&e, bytes.len())),
    }
}

#[rustler::nif]
fn parse_get_many_nil<'a>(
    env: Env<'a>,
    json: Binary<'a>,
    compiled: ResourceArc<CompiledPaths>,
    alloc_len: usize,
) -> Term<'a> {
    let mut work = Work::default();
    let input_term = json.to_term(env).as_c_arg();
    let (result, scanned) = do_parse_get_many_nil(
        env,
        input_term,
        json.as_slice(),
        alloc_len,
        &compiled,
        &mut work,
    );
    // Bytes cover the parse, nodes the extraction; one hint covers both.
    consume_timeslice_mixed(env, scanned, work.nodes());
    result
}

#[rustler::nif(schedule = "DirtyCpu")]
fn parse_get_many_nil_dirty<'a>(
    env: Env<'a>,
    json: Binary<'a>,
    compiled: ResourceArc<CompiledPaths>,
    alloc_len: usize,
) -> Term<'a> {
    let mut work = Work::default();
    let input_term = json.to_term(env).as_c_arg();
    do_parse_get_many_nil(
        env,
        input_term,
        json.as_slice(),
        alloc_len,
        &compiled,
        &mut work,
    )
    .0
}

#[inline]
fn pointer_lookup_compiled<'v, I: ObjectIndex<'v>>(
    value: &'v sonic_rs::Value,
    segs: &[PathSeg],
    memo: &mut I,
    work: &mut Work,
) -> Option<&'v sonic_rs::Value> {
    let mut current = value;
    for seg in segs {
        current = match seg {
            PathSeg::Key(k) => object_get(current, k, memo, work)?,
            PathSeg::Num { idx, key } => {
                if current.is_array() {
                    current.get(*idx)?
                } else {
                    object_get(current, key, memo, work)?
                }
            }
        };
    }
    Some(current)
}

#[rustler::nif]
fn get_many_compiled<'a>(
    env: Env<'a>,
    doc: ResourceArc<ParsedDocument>,
    compiled: ResourceArc<CompiledPaths>,
) -> Term<'a> {
    let mut work = Work::default();
    let result = extract_compiled_results(env, &doc.value, &compiled, &mut work);
    consume_timeslice_nodes(env, work.nodes());
    result
}

#[rustler::nif(schedule = "DirtyCpu")]
fn get_many_compiled_dirty<'a>(
    env: Env<'a>,
    doc: ResourceArc<ParsedDocument>,
    compiled: ResourceArc<CompiledPaths>,
) -> Term<'a> {
    let mut work = Work::default();
    extract_compiled_results(env, &doc.value, &compiled, &mut work)
}

#[rustler::nif]
fn get_many_nil_compiled<'a>(
    env: Env<'a>,
    doc: ResourceArc<ParsedDocument>,
    compiled: ResourceArc<CompiledPaths>,
) -> Term<'a> {
    let mut work = Work::default();
    let result = extract_compiled(env, &doc.value, &compiled, &mut work);
    consume_timeslice_nodes(env, work.nodes());
    result
}

#[rustler::nif(schedule = "DirtyCpu")]
fn get_many_nil_compiled_dirty<'a>(
    env: Env<'a>,
    doc: ResourceArc<ParsedDocument>,
    compiled: ResourceArc<CompiledPaths>,
) -> Term<'a> {
    let mut work = Work::default();
    extract_compiled(env, &doc.value, &compiled, &mut work)
}

#[inline]
fn do_get_many_nil<'a>(
    env: Env<'a>,
    doc: &ParsedDocument,
    paths: ListIterator<'a>,
    work: &mut Work,
) -> NifResult<Term<'a>> {
    let nil_raw = atoms::nil().as_c_arg();
    let mut acc = TermAcc::new();
    let mut memo = LazyMemo::new(doc.unique_keys);

    for path_term in paths {
        // Non-binary (or non-UTF-8) path entries are caller bugs: badarg.
        let path: &str = path_term.decode()?;
        let r = match pointer_lookup(&doc.value, path, &mut memo, work) {
            Some(value) => match value_to_term(env, value, MAX_DEPTH, &mut work.nodes) {
                Some(term) => term.as_c_arg(),
                None => nil_raw,
            },
            None => nil_raw,
        };
        acc.push(r);
        // Per result, so a batch that ends in `badarg` still reports what it
        // built before that.
        work.nodes += 1;
    }

    Ok(acc.into_list(env))
}

#[rustler::nif]
fn get_many_nil<'a>(
    env: Env<'a>,
    doc: ResourceArc<ParsedDocument>,
    paths: ListIterator<'a>,
) -> NifResult<Term<'a>> {
    let mut work = Work::default();
    // Charged before the `?`: a bad path part way through a batch does not
    // undo the lookups already made, and reporting them is what keeps a long
    // batch that ends in `badarg` from being free.
    let result = do_get_many_nil(env, &doc, paths, &mut work);
    consume_timeslice_nodes(env, work.nodes());
    result
}

#[rustler::nif(schedule = "DirtyCpu")]
fn get_many_nil_dirty<'a>(
    env: Env<'a>,
    doc: ResourceArc<ParsedDocument>,
    paths: ListIterator<'a>,
) -> NifResult<Term<'a>> {
    let mut work = Work::default();
    do_get_many_nil(env, &doc, paths, &mut work)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// Parsed wide object with predictable `kN` keys.
    fn wide_doc(members: usize) -> sonic_rs::Value {
        let json = format!(
            "{{{}}}",
            (0..members)
                .map(|i| format!("\"k{i}\":{i}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        sonic_rs::from_str(&json).expect("valid document")
    }

    /// Node units for `members` key comparisons, in the terms the timeslice
    /// charge is expressed in.
    fn scan_work(members: usize) -> usize {
        members / MEMBERS_PER_NODE
    }

    /// Node units this scan alone would charge.
    fn scanned_work(scanned: &Scanned<'_>) -> usize {
        let mut work = Work::default();
        scanned.charge(&mut work);
        work.nodes()
    }

    fn lookup<'v>(memo: &mut ObjectMemo<'v>, doc: &'v sonic_rs::Value, key: &str) -> (bool, usize) {
        let mut work = Work::default();
        let pairs = doc.as_pair_slice().expect("object");
        let hit = memo.lookup_wide(pairs, key, &mut work);
        (hit.is_some(), work.nodes())
    }

    #[test]
    fn a_wide_object_reports_the_work_it_does() {
        let doc = wide_doc(4_000);
        let mut memo = ObjectMemo::new(false);

        let (found, miss) = lookup(&mut memo, &doc, "nope");
        assert!(!found);
        assert!(miss >= scan_work(4_000), "a full scan charged {miss}");

        let (found, edge) = lookup(&mut memo, &doc, "k3999");
        assert!(found);
        assert!(edge * 8 < miss, "a one-comparison lookup charged {edge}");
    }

    #[test]
    fn the_index_is_built_once_the_object_has_earned_it() {
        let members = 4_000;
        let doc = wide_doc(members);
        let mut memo = ObjectMemo::new(false);
        let threshold = index_after_visits(members) as usize;

        let (_, scan) = lookup(&mut memo, &doc, "nope");
        for visit in 2..threshold - 1 {
            let (found, work) = lookup(&mut memo, &doc, "nope");
            assert!(!found);
            assert_eq!(work, scan, "visit {visit} did not scan");
        }

        let (_, priced) = lookup(&mut memo, &doc, "nope");
        assert_eq!(
            priced,
            scan + scan_work(members),
            "the crossing scan priced"
        );

        let (_, building) = lookup(&mut memo, &doc, "nope");
        assert!(
            building > priced,
            "the threshold visit did not build an index"
        );

        let (found, hashed) = lookup(&mut memo, &doc, "k0");
        assert!(found);
        assert!(hashed < scan, "an indexed lookup charged {hashed}");
    }

    #[test]
    fn the_way_that_goes_is_the_one_furthest_from_an_index() {
        let narrow = wide_doc(WIDE_OBJECT_MEMBERS);
        let wide = colliding_with(base_of(&narrow), 4_096, 1)
            .pop()
            .expect("a wide object in the same set");
        let newcomer = colliding_with(base_of(&narrow), 256, 1)
            .pop()
            .expect("a third object in the same set");
        let mut memo = ObjectMemo::new(false);

        for _ in 0..index_after_visits(WIDE_OBJECT_MEMBERS) - 1 {
            lookup(&mut memo, &narrow, "nope");
        }
        lookup(&mut memo, &wide, "nope");
        let ways = ObjectMemo::set_of(base_of(&narrow));
        assert!(
            memo.slots[ways.clone()]
                .iter()
                .all(|s| s.index.is_none() && !s.base.is_null()),
            "the two residents did not take the ways unindexed"
        );
        assert!(
            memo.slots[ways.clone()]
                .iter()
                .find(|s| s.base == base_of(&wide))
                .expect("the wide resident")
                .earning
                .credit
                > memo.slots[ways.clone()]
                    .iter()
                    .find(|s| s.base == base_of(&narrow))
                    .expect("the narrow resident")
                    .earning
                    .credit,
            "the fixture does not have the wide object holding more raw credit"
        );

        for _ in 0..index_after_visits(256) + 1 {
            lookup(&mut memo, &newcomer, "nope");
        }
        assert!(
            memo.slots[ways.clone()]
                .iter()
                .any(|s| s.base == base_of(&newcomer)),
            "the newcomer never got in"
        );
        assert!(
            memo.slots[ways].iter().any(|s| s.base == base_of(&narrow)),
            "the resident one lookup from its index gave up its way"
        );
    }

    fn colliding(members: usize, wanted: usize) -> Vec<sonic_rs::Value> {
        let mut by_set: HashMap<usize, Vec<sonic_rs::Value>> = HashMap::new();
        for _ in 0..512 {
            let doc = wide_doc(members);
            let set =
                ObjectMemo::set_of(doc.as_pair_slice().expect("object").as_ptr() as *const u8);
            let bucket = by_set.entry(set.start).or_default();
            bucket.push(doc);
            if bucket.len() == wanted {
                return by_set.remove(&set.start).expect("just filled");
            }
        }
        panic!("no {wanted} objects shared a set in 512 tries");
    }

    #[test]
    fn an_edge_hit_earns_one_comparison_of_credit() {
        let members = 4_000;
        let doc = wide_doc(members);
        let last = format!("k{}", members - 1);
        let mut memo = ObjectMemo::new(false);

        for _ in 0..index_after_visits(members) * 4 {
            let (found, work) = lookup(&mut memo, &doc, &last);
            assert!(found);
            assert!(work <= 1, "a one-comparison lookup charged {work}");
        }
        assert!(
            memo.slots.iter().all(|s| s.index.is_none()),
            "edge hits built an index they had not paid for"
        );

        for _ in 0..index_after_visits(members) {
            lookup(&mut memo, &doc, "nope");
        }
        assert!(
            memo.slots.iter().any(|s| s.index.is_some()),
            "full scans did not earn an index"
        );
    }

    #[test]
    fn a_hot_newcomer_is_admitted_once_it_has_earned_it() {
        let members = 4_000;
        let docs = colliding(members, MEMO_WAYS + 1);
        let threshold = index_after_visits(members) as usize;
        let mut memo = ObjectMemo::new(false);

        for doc in docs.iter().take(MEMO_WAYS) {
            for _ in 0..threshold {
                lookup(&mut memo, doc, "nope");
            }
        }
        assert_eq!(
            memo.slots.iter().filter(|s| s.index.is_some()).count(),
            MEMO_WAYS,
            "the incumbents did not earn indexes"
        );

        let newcomer = docs.last().expect("three objects");
        let base = newcomer.as_pair_slice().expect("object").as_ptr() as *const u8;
        for _ in 0..threshold - 1 {
            lookup(&mut memo, newcomer, "nope");
        }
        assert!(
            !memo.slots.iter().any(|s| s.base == base),
            "a newcomer took a way before paying for it"
        );

        lookup(&mut memo, newcomer, "nope");
        assert!(
            memo.slots
                .iter()
                .any(|s| s.base == base && s.index.is_some()),
            "a newcomer that paid the threshold was not let in"
        );
        assert_eq!(
            memo.slots.iter().filter(|s| s.index.is_some()).count(),
            MEMO_WAYS,
            "admission grew the set"
        );
    }

    #[test]
    fn cold_colliders_converge_instead_of_taking_each_others_ways() {
        let members = 4_000;
        let docs = colliding(members, MEMO_WAYS + 1);
        let mut memo = ObjectMemo::new(false);
        let rounds = (ObjectMemo::scan_budget(members) / members as u64) as usize * 3;

        for _ in 0..rounds {
            for doc in docs.iter() {
                lookup(&mut memo, doc, "nope");
            }
        }

        assert_eq!(
            memo.slots.iter().filter(|s| s.index.is_some()).count(),
            MEMO_WAYS,
            "a set of two ways did not settle on two objects"
        );
    }

    #[test]
    fn two_newcomers_alternating_both_earn_their_way_in() {
        let members = 4_000;
        let docs = colliding(members, MEMO_WAYS * 2);
        let mut memo = ObjectMemo::new(false);
        let budget = ObjectMemo::scan_budget(members);
        let scans = (budget / members as u64) as usize + 1;

        for doc in docs.iter().take(MEMO_WAYS).rev() {
            for _ in 0..scans {
                lookup(&mut memo, doc, "nope");
            }
        }
        let incumbents: Vec<*const u8> = docs.iter().take(MEMO_WAYS).map(base_of).collect();

        let newcomers: Vec<*const u8> = docs.iter().skip(MEMO_WAYS).map(base_of).collect();
        for _ in 0..scans + 1 {
            for doc in docs.iter().skip(MEMO_WAYS) {
                lookup(&mut memo, doc, "nope");
            }
        }

        for base in newcomers {
            assert!(
                memo.slots
                    .iter()
                    .any(|s| s.base == base && s.index.is_some()),
                "an alternating newcomer never earned its way in"
            );
        }
        assert!(
            incumbents
                .iter()
                .all(|base| memo.slots.iter().all(|s| s.base != *base)),
            "the finished incumbents kept their ways"
        );
    }

    #[test]
    fn the_way_that_goes_is_the_one_used_longest_ago() {
        let members = 4_000;
        let docs = colliding(members, MEMO_WAYS + 1);
        let mut memo = ObjectMemo::new(false);
        let scans = (ObjectMemo::scan_budget(members) / members as u64) as usize + 1;

        for doc in docs.iter().take(MEMO_WAYS) {
            for _ in 0..scans {
                lookup(&mut memo, doc, "nope");
            }
        }
        lookup(&mut memo, &docs[MEMO_WAYS - 1], "k0");
        let stale = base_of(&docs[0]);
        let fresh = base_of(&docs[MEMO_WAYS - 1]);

        for _ in 0..scans {
            lookup(&mut memo, &docs[MEMO_WAYS], "nope");
        }

        assert!(
            memo.slots.iter().all(|s| s.base != stale),
            "the way used longest ago survived"
        );
        assert!(
            memo.slots
                .iter()
                .any(|s| s.base == fresh && s.index.is_some()),
            "the way used most recently was taken instead"
        );
    }

    fn base_of(doc: &sonic_rs::Value) -> *const u8 {
        doc.as_pair_slice().expect("object").as_ptr() as *const u8
    }

    #[test]
    fn interleaved_newcomers_do_not_rebuild_indexes() {
        let members = 4_000;
        let docs = colliding(members, MEMO_WAYS * 2);
        let threshold = index_after_visits(members) as usize;
        let mut memo = ObjectMemo::new(false);

        for doc in docs.iter().take(MEMO_WAYS) {
            for _ in 0..threshold {
                lookup(&mut memo, doc, "nope");
            }
        }
        let before: Vec<(*const u8, bool)> = memo
            .slots
            .iter()
            .map(|s| (s.base, s.index.is_some()))
            .collect();

        // Keep every newcomer below its own admission threshold.
        for _ in 0..threshold - 1 {
            for doc in docs.iter() {
                lookup(&mut memo, doc, "nope");
            }
        }

        let after: Vec<(*const u8, bool)> = memo
            .slots
            .iter()
            .map(|s| (s.base, s.index.is_some()))
            .collect();
        assert_eq!(
            before, after,
            "cold interleaving moved an index rather than scanning"
        );
    }

    /// Long-key fixture whose match sits at either end of the scan order.
    fn long_key_doc(members: usize, key_len: usize, match_last: bool) -> (sonic_rs::Value, String) {
        let prefix = "p".repeat(key_len - 4);
        let long = format!("{prefix}9999");
        let others: Vec<String> = (0..members - 1).map(|i| format!("\"s{i}\":{i}")).collect();
        let long_member = format!("\"{long}\":1");
        let members = if match_last {
            others
                .into_iter()
                .chain(std::iter::once(long_member))
                .collect::<Vec<_>>()
        } else {
            std::iter::once(long_member)
                .chain(others)
                .collect::<Vec<_>>()
        };
        let json = format!("{{{}}}", members.join(","));
        (
            sonic_rs::from_str(&json).expect("valid document"),
            format!("/{long}"),
        )
    }

    #[test]
    fn long_key_scans_follow_direction_and_charge_compared_bytes() {
        let key_len = 1024 * 1024;

        for (unique_keys, key_at_end) in [(false, true), (true, false)] {
            let (doc, path) = long_key_doc(WIDE_OBJECT_MEMBERS - 1, key_len, key_at_end);
            let pairs = doc.as_pair_slice().expect("object");
            let key = path.strip_prefix('/').expect("a pointer");
            let scanned = ObjectMemo::scan(pairs, key, unique_keys);

            assert!(scanned.hit.is_some(), "the long key must match");
            assert_eq!(scanned.compared, 1, "the scan started at the wrong end");
            assert_eq!(scanned.compared_bytes, key_len);
            assert!(scanned_work(&scanned) >= key_len / KEY_BYTES_PER_NODE);
        }

        let long = "d".repeat(LONG_KEY_BYTES * 2);
        let json = format!("{{\"{long}\":1,\"x\":0,\"{long}\":2}}");
        let doc: sonic_rs::Value = sonic_rs::from_str(&json).expect("valid document");
        let path = format!("/{long}");

        let mut work = Work::default();
        let last = pointer_lookup(&doc, &path, &mut NoIndex::new(false), &mut work);
        assert_eq!(last.and_then(|v| v.as_i64()), Some(2));

        let mut work = Work::default();
        let first = pointer_lookup(&doc, &path, &mut NoIndex::new(true), &mut work);
        assert_eq!(first.and_then(|v| v.as_i64()), Some(1));
    }

    #[test]
    fn the_byte_counting_scan_starts_at_the_documented_length() {
        let doc = wide_doc(WIDE_OBJECT_MEMBERS);
        let pairs = doc.as_pair_slice().expect("object");

        let short = "n".repeat(LONG_KEY_BYTES);
        let long = "n".repeat(LONG_KEY_BYTES + 1);
        assert_eq!(ObjectMemo::scan(pairs, &short, false).compared_bytes, 0);
        assert_eq!(ObjectMemo::scan(pairs, &long, false).compared_bytes, 0);

        let json = format!("{{\"{short}\":1,\"{long}\":2}}");
        let sized: sonic_rs::Value = sonic_rs::from_str(&json).expect("valid document");
        let sized = sized.as_pair_slice().expect("object");
        assert_eq!(ObjectMemo::scan(sized, &short, false).compared_bytes, 0);
        assert_eq!(
            ObjectMemo::scan(sized, &long, false).compared_bytes,
            long.len(),
            "the long scan counts the comparison it made"
        );
    }

    fn colliding_with(base: *const u8, members: usize, wanted: usize) -> Vec<sonic_rs::Value> {
        let want = ObjectMemo::set_of(base).start;
        let mut pool: Vec<sonic_rs::Value> = Vec::new();
        let mut hits = 0;
        for _ in 0..512 {
            let doc = wide_doc(members);
            if ObjectMemo::set_of(base_of(&doc)).start == want {
                hits += 1;
            }
            pool.push(doc);
            if hits == wanted {
                break;
            }
        }
        assert_eq!(hits, wanted, "no {wanted} objects landed in that set");
        pool.into_iter()
            .filter(|d| ObjectMemo::set_of(base_of(d)).start == want)
            .collect()
    }

    #[test]
    fn a_scan_that_rejects_at_the_first_byte_does_not_buy_an_index() {
        let key_len = 8192;
        let members = 256;
        let prefix = "c".repeat(key_len - 7);
        let json = format!(
            "{{{}}}",
            (0..members)
                .map(|i| format!("\"a{prefix}{i:06}\":{i}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(
            members * key_len > INDEX_KEY_BYTES,
            "fixture is under the cap"
        );
        let doc: sonic_rs::Value = sonic_rs::from_str(&json).expect("valid document");
        let scans = index_after_visits(members) * 3;

        let mut memo = ObjectMemo::new(false);
        for _ in 0..scans {
            lookup(&mut memo, &doc, &format!("z{prefix}999999"));
        }
        assert!(
            memo.slots.iter().all(|s| s.index.is_none()),
            "scans that read a chunk per key bought an index over {} MiB of them",
            members * key_len / (1024 * 1024)
        );

        let mut memo = ObjectMemo::new(false);
        for _ in 0..scans {
            lookup(&mut memo, &doc, &format!("a{prefix}999999"));
        }
        assert!(
            memo.slots.iter().any(|s| s.index.is_some()),
            "scans that read every key never bought an index"
        );
    }

    #[test]
    fn a_scan_counts_the_bytes_its_comparisons_reached() {
        let key_len = LONG_KEY_BYTES + 44;
        let members = 64;
        let prefix = "c".repeat(key_len - 7);
        let json = format!(
            "{{{}}}",
            (0..members)
                .map(|i| format!("\"a{prefix}{i:06}\":{i}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let doc: sonic_rs::Value = sonic_rs::from_str(&json).expect("valid document");
        let pairs = doc.as_pair_slice().expect("object");

        let rejected = ObjectMemo::scan_long(pairs, &format!("z{prefix}999999"), false);
        assert!(rejected.hit.is_none());
        assert_eq!(rejected.compared, members, "every member was looked at");
        assert_eq!(
            rejected.compared_bytes,
            members * 64,
            "a mismatch at byte 0 is one chunk per member, no more and no less"
        );

        let same_prefix = |shared: usize| {
            let key = format!("a{prefix}{:06}", members - 1);
            let mut needle = key.into_bytes();
            needle[shared] = b'~';
            ObjectMemo::scan_long(pairs, &String::from_utf8(needle).expect("ascii"), false)
        };
        assert_eq!(same_prefix(0).compared_bytes, members * 64);
        assert_eq!(same_prefix(63).compared_bytes, members * 64);
        assert_eq!(same_prefix(64).compared_bytes, members * 192);
        assert_eq!(same_prefix(191).compared_bytes, members * 192);
        assert_eq!(same_prefix(192).compared_bytes, members * key_len);

        let hit = ObjectMemo::scan_long(pairs, &format!("a{prefix}{:06}", members - 1), false);
        assert!(hit.hit.is_some());
        assert_eq!(hit.compared, 1);
        assert_eq!(hit.compared_bytes, key_len, "the whole key was compared");
    }

    #[test]
    fn a_scan_wide_enough_to_be_bytes_counts_them() {
        let key_len = LONG_KEY_BYTES;
        let narrow = 1_024;
        let wide = SCAN_BYTES_ABOVE / key_len + 1;
        let doc = |members: usize| {
            let prefix = "c".repeat(key_len - 8);
            let json = format!(
                "{{{}}}",
                (0..members)
                    .map(|i| format!("\"{prefix}{i:08}\":{i}"))
                    .collect::<Vec<_>>()
                    .join(",")
            );
            sonic_rs::from_str::<sonic_rs::Value>(&json).expect("valid document")
        };
        let needle = format!("{}99999999", "c".repeat(key_len - 8));

        let small = doc(narrow);
        let pairs = small.as_pair_slice().expect("object");
        assert!(pairs.len() * key_len <= SCAN_BYTES_ABOVE);
        assert_eq!(
            ObjectMemo::scan(pairs, &needle, false).compared_bytes,
            0,
            "a scan this size is not worth counting"
        );

        let big = doc(wide);
        let pairs = big.as_pair_slice().expect("object");
        assert!(pairs.len() * key_len > SCAN_BYTES_ABOVE);
        let scanned = ObjectMemo::scan(pairs, &needle, false);
        assert_eq!(
            scanned.compared_bytes,
            wide * key_len,
            "a scan of megabytes reported them as member work alone"
        );
        assert!(
            scanned_work(&scanned) > scan_work(wide),
            "and charged for them"
        );
    }

    #[test]
    fn a_candidate_that_has_earned_its_index_survives_a_newcomer() {
        let key_len = 8192;
        let members = 256;
        let prefix = "c".repeat(key_len - 7);
        let json = format!(
            "{{{}}}",
            (0..members)
                .map(|i| format!("\"a{prefix}{i:06}\":{i}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let earner: sonic_rs::Value = sonic_rs::from_str(&json).expect("valid document");
        let base = base_of(&earner);
        let needle = format!("z{prefix}999999");
        let mut memo = ObjectMemo::new(false);

        let others = colliding_with(base, 256, MEMO_WAYS + 2);
        for doc in others.iter().take(MEMO_WAYS) {
            for _ in 0..index_after_visits(256) {
                lookup(&mut memo, doc, "nope");
            }
        }

        let mut earned = false;
        for _ in 0..index_after_visits(members) * 40 {
            lookup(&mut memo, &earner, &needle);
            if memo
                .candidates
                .iter()
                .any(|c| c.base == base && c.earning.ready() && c.earning.key_bytes.is_some())
            {
                earned = true;
                break;
            }
        }
        assert!(earned, "the candidate never got close to its index");

        lookup(&mut memo, &others[MEMO_WAYS], "nope");
        assert!(
            memo.candidates.iter().filter(|c| !c.base.is_null()).count() >= MEMO_WAYS,
            "the candidate table is not full, so nothing has to be evicted"
        );

        lookup(&mut memo, &others[MEMO_WAYS + 1], "nope");
        assert!(
            memo.candidates
                .iter()
                .any(|c| c.base == base && c.earning.ready() && c.earning.key_bytes.is_some()),
            "a newcomer with no credit evicted a candidate that had earned its index"
        );

        lookup(&mut memo, &earner, &needle);
        assert!(
            memo.slots
                .iter()
                .any(|s| s.base == base && s.index.is_some()),
            "the candidate that had earned its index did not build it"
        );
    }

    #[test]
    fn the_candidate_that_gives_up_its_slot_is_the_one_furthest_from_its_index() {
        let candidate = |credit, target, byte_credit, key_bytes| Candidate {
            base: std::ptr::dangling(),
            earning: Earning {
                credit,
                byte_credit,
                key_bytes,
                target,
            },
        };

        let cap = INDEX_KEY_BYTES as u64;
        let ready = candidate(100, 100, 0, Some(cap));
        let started = candidate(50, 100, 0, Some(cap));
        assert!(ready.earning.ready() && !started.earning.ready());
        assert_eq!(ObjectMemo::least_deserving(&[ready, started]), 1);
        assert_eq!(ObjectMemo::least_deserving(&[started, ready]), 0);

        let wide = candidate(2_000, 100_000, 0, Some(cap * 2));
        let narrow = candidate(200, 256, 0, Some(cap * 2));
        assert_eq!(ObjectMemo::least_deserving(&[wide, narrow]), 0);
        assert_eq!(ObjectMemo::least_deserving(&[narrow, wide]), 1);

        let paid = candidate(50, 100, cap, Some(cap * 2));
        let unpaid = candidate(50, 100, 0, Some(cap * 2));
        assert_eq!(ObjectMemo::least_deserving(&[paid, unpaid]), 1);
        assert_eq!(ObjectMemo::least_deserving(&[unpaid, paid]), 0);

        let scanned_over = candidate(100_000, 384, 0, Some(cap * 2));
        let nearly_paid = candidate(384, 384, cap * 2 - 1, Some(cap * 2));
        assert!(!scanned_over.earning.ready() && !nearly_paid.earning.ready());
        assert_eq!(
            ObjectMemo::least_deserving(&[scanned_over, nearly_paid]),
            0,
            "member credit already paid outranked the bytes still owed"
        );
        assert_eq!(ObjectMemo::least_deserving(&[nearly_paid, scanned_over]), 1);

        let over_read = candidate(50, 100, cap * 6, Some(cap * 2));
        let read_once = candidate(50, 100, cap * 2, Some(cap * 2));
        assert_eq!(ObjectMemo::least_deserving(&[over_read, read_once]), 0);
        assert_eq!(ObjectMemo::least_deserving(&[read_once, over_read]), 0);
    }

    #[test]
    fn an_oversized_candidate_remembers_what_it_costs() {
        let key_len = 16 * 1024;
        let members = 128;
        let prefix = "q".repeat(key_len);
        let json = format!(
            "{{{}}}",
            (0..members)
                .map(|i| format!("\"{prefix}{i}\":{i}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let big: sonic_rs::Value = sonic_rs::from_str(&json).expect("valid document");
        let base = base_of(&big);
        let mut memo = ObjectMemo::new(false);

        let residents = colliding_with(base, 256, MEMO_WAYS);
        for doc in residents.iter() {
            for _ in 0..index_after_visits(256) {
                lookup(&mut memo, doc, "nope");
            }
        }
        assert!(
            memo.slots[ObjectMemo::set_of(base)]
                .iter()
                .all(|s| s.index.is_some()),
            "the residents did not take the ways"
        );

        let budget = ObjectMemo::scan_budget(members) / members as u64 + 4;
        for _ in 0..budget * 3 {
            lookup(&mut memo, &big, "z");
        }

        assert!(
            memo.slots
                .iter()
                .all(|s| s.base != base || s.index.is_none()),
            "an object of {} MiB of keys was indexed for length-rejecting scans",
            members * key_len / (1024 * 1024)
        );
        let profiled = memo
            .candidates
            .iter()
            .find(|c| c.base == base)
            .expect("the object that scanned most is not even a candidate")
            .earning
            .key_bytes
            .expect("the candidate forgot what it had profiled");
        let actual: u64 = big
            .as_pair_slice()
            .expect("object")
            .iter()
            .map(|(k, _)| k.as_node_str().map_or(0, str::len) as u64)
            .sum();
        assert_eq!(profiled, actual, "profiled its keys");

        let newcomers = colliding_with(base, 256, MEMO_WAYS + 2);
        for newcomer in newcomers.iter().skip(MEMO_WAYS) {
            lookup(&mut memo, newcomer, "nope");
        }
        let candidate = memo
            .candidates
            .iter()
            .find(|c| c.base == base)
            .expect("a newcomer with no credit took the earning candidate's slot");
        assert_eq!(
            candidate.earning.key_bytes,
            Some(profiled),
            "the profile was re-taken rather than remembered"
        );
    }

    #[test]
    fn an_oversized_object_is_indexed_once_its_scans_have_read_the_bytes() {
        let key_len = LONG_KEY_BYTES + 44;
        let members = 4_096;
        let prefix = "c".repeat(key_len - 6);
        let json = format!(
            "{{{}}}",
            (0..members)
                .map(|i| format!("\"{prefix}{i:06}\":{i}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(
            members * key_len > INDEX_KEY_BYTES,
            "fixture is under the cap"
        );
        let doc: sonic_rs::Value = sonic_rs::from_str(&json).expect("valid document");
        let needle = format!("{prefix}999999");
        let mut memo = ObjectMemo::new(false);

        for _ in 0..index_after_visits(members) + 2 {
            lookup(&mut memo, &doc, &needle);
        }

        assert!(
            memo.slots.iter().any(|s| s.index.is_some()),
            "scans that read the object never bought an index"
        );
    }

    #[test]
    fn an_object_of_large_keys_is_scanned_rather_than_indexed() {
        let key_len = 16 * 1024;
        let members = 128;
        let prefix = "q".repeat(key_len);
        let json = format!(
            "{{{}}}",
            (0..members)
                .map(|i| format!("\"{prefix}{i}\":{i}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(
            members * key_len > INDEX_KEY_BYTES,
            "fixture is under the cap"
        );
        let doc: sonic_rs::Value = sonic_rs::from_str(&json).expect("valid document");
        let mut memo = ObjectMemo::new(false);

        let budget = ObjectMemo::scan_budget(members) / members as u64 + 4;
        for _ in 0..budget {
            lookup(&mut memo, &doc, "z");
        }

        assert!(
            memo.slots.iter().all(|s| s.index.is_none()),
            "hashing {} MiB of keys was worth it after all",
            members * key_len / (1024 * 1024)
        );
    }

    #[test]
    fn a_narrow_object_with_large_keys_is_charged_for_its_bytes() {
        let key_len = 4 * 1024;
        let prefix = "p".repeat(key_len - 4);
        let json = format!(
            "{{{}}}",
            (0..WIDE_OBJECT_MEMBERS - 1)
                .map(|i| format!("\"{prefix}{i:04}\":{i}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let doc: sonic_rs::Value = sonic_rs::from_str(&json).expect("valid document");
        let path = format!("/{prefix}9999");

        let mut work = Work::default();
        let hit = pointer_lookup(&doc, &path, &mut NoIndex::new(false), &mut work);
        assert!(hit.is_none(), "the miss is the expensive case");
        let nodes = work.nodes();
        assert!(
            nodes >= TIMESLICE_MIN_NODES,
            "a {} MB scan reported {nodes} nodes",
            (WIDE_OBJECT_MEMBERS - 1) * key_len / (1024 * 1024)
        );

        let small = wide_doc(WIDE_OBJECT_MEMBERS - 1);
        let mut small_work = Work::default();
        pointer_lookup(&small, "/k5", &mut NoIndex::new(false), &mut small_work);
        assert!(
            small_work.nodes() < TIMESLICE_MIN_NODES,
            "charged {} nodes",
            small_work.nodes()
        );
    }

    /// Work used to be converted to node units at every site that produced it,
    /// so each lookup discarded its remainder. Scans of 31 members rounded to
    /// nothing however many times a batch made them, and a batch of 2047 of
    /// them reported only the results it emitted. Counting raw and converting
    /// once is what makes those comparisons reach the reporting floor.
    #[test]
    fn narrow_scans_accumulate_across_a_batch() {
        let doc = wide_doc(MEMBERS_PER_NODE - 1);
        // Short enough that the pointer's own bytes also round away.
        let miss = "/no";
        assert!(miss.len() < KEY_BYTES_PER_NODE);

        let mut one = Work::default();
        pointer_lookup(&doc, miss, &mut NoIndex::new(false), &mut one);
        assert_eq!(one.nodes(), 0, "one narrow scan is below the floor");

        let mut batch = Work::default();
        for _ in 0..TIMESLICE_MIN_NODES {
            pointer_lookup(&doc, miss, &mut NoIndex::new(false), &mut batch);
        }
        assert!(
            batch.nodes() >= TIMESLICE_MIN_NODES,
            "{TIMESLICE_MIN_NODES} narrow scans reported {} nodes",
            batch.nodes()
        );
    }

    /// Pins the width-dependent threshold table derived from the member sweep.
    #[test]
    fn the_index_threshold_follows_the_measured_break_even() {
        for w in [128usize, 512, 2_000, 8_000, 32_000, 100_000] {
            assert!(index_after_visits(w) >= 4, "{w}: indexes on sight");
        }
        assert!(index_after_visits(128) < index_after_visits(2_000));
        assert!(index_after_visits(2_000) < index_after_visits(8_000));
        assert!(index_after_visits(8_000) >= 22);
        assert!(index_after_visits(100_000) >= 27);

        for (members, expected) in [
            (WIDE_OBJECT_MEMBERS, 4),
            (255, 4),
            (256, 8),
            (1_023, 8),
            (1_024, 12),
            (4_095, 12),
            (4_096, 24),
            (16_383, 24),
            (16_384, 28),
            (100_000, 28),
        ] {
            assert_eq!(index_after_visits(members), expected, "{members} members");
        }
    }

    /// Rejected-input work must increase with parser progress and remain bounded
    /// by the validation and full-parse endpoints.
    #[test]
    fn rejected_input_is_charged_between_one_pass_and_two() {
        // The validation floor applies even before syntax parsing advances.
        assert_eq!(timeslice_bytes(1, 0), 0);
        assert_eq!(timeslice_bytes(0, 32), 1);
        assert!(timeslice_bytes(1, 19_001) >= TIMESLICE_MIN_BYTES);

        for &len in &[0usize, 1, 511, 512, 1599, 1600, 20_000, 1 << 20] {
            let validated = timeslice_bytes(0, len);
            assert!(validated <= len, "{len}: validating cost more than parsing");
            assert_eq!(
                timeslice_bytes(len, len),
                len,
                "{len}: reaching the end must cost what parsing it does"
            );

            let mut last = validated;
            for reached in (0..=len).step_by(1 + len / 64) {
                let charged = timeslice_bytes(reached, len);
                assert!(charged >= last, "{len}/{reached}: went backwards");
                assert!(charged <= len, "{len}/{reached}: over-charged");
                last = charged;
            }
        }
    }

    /// The size predicate is portable to targets where the `u32` boundary is
    /// not representable as a larger `usize` value.
    #[test]
    fn the_size_bound_sits_at_what_a_u32_offset_can_address() {
        assert!(!sonic_rs::json_too_large(0));
        assert!(!sonic_rs::json_too_large(u32::MAX as usize - 1));
        assert!(!sonic_rs::json_too_large(u32::MAX as usize));

        // The upper boundary exists only on wider pointer targets.
        #[cfg(target_pointer_width = "64")]
        {
            assert!(sonic_rs::json_too_large(u32::MAX as usize + 1));
            assert!(sonic_rs::json_too_large(usize::MAX));
        }
        #[cfg(target_pointer_width = "32")]
        assert!(!sonic_rs::json_too_large(usize::MAX));
    }

    /// Distinguishes the vendored parser's recursion limit from syntax errors.
    #[test]
    fn the_recursion_limit_is_told_apart_from_a_syntax_fault() {
        // Use a larger test thread stack so debug frames reach the parser limit.
        let deep = "[".repeat(130) + &"]".repeat(130);
        let parsed = std::thread::Builder::new()
            .stack_size(16 << 20)
            .spawn(move || {
                let err = sonic_rs::from_slice::<sonic_rs::Value>(deep.as_bytes()).unwrap_err();
                (err.is_recursion_limit(), format!("{}", err))
            })
            .expect("spawn")
            .join()
            .expect("the parser should stop at the cap, not overflow");
        assert!(parsed.0, "{}", parsed.1);

        for ordinary in ["!", "{\"a\":}", "[1,", "\"unterminated"] {
            let err = sonic_rs::from_slice::<sonic_rs::Value>(ordinary.as_bytes()).unwrap_err();
            assert!(!err.is_recursion_limit(), "{ordinary}: {err}");
        }
    }

    /// Fused extraction combines byte and term work before rounding. This test
    /// compares that rule with the old split accounting at both floors.
    #[test]
    fn a_mixed_charge_rounds_once_rather_than_twice() {
        let split = |bytes: usize, nodes: usize| {
            let b = if bytes >= TIMESLICE_MIN_BYTES {
                timeslice_percent(bytes)
            } else {
                0
            };
            let n = if nodes >= TIMESLICE_MIN_NODES {
                ((nodes / NODES_PER_REDUCTION * 100 / REDUCTION_COUNT) as i32).clamp(1, 100)
            } else {
                0
            };
            b + n
        };
        let combined = |bytes, nodes| mixed_timeslice_percent(bytes, nodes).unwrap_or(0);

        // Separate clamping over-reports this pair.
        assert_eq!((split(512, 512), combined(512, 512)), (4, 3));
        // Separate truncation under-reports this pair.
        assert_eq!((split(1599, 512), combined(1599, 512)), (4, 5));
        // The next byte removes the rounding difference.
        assert_eq!((split(1600, 512), combined(1600, 512)), (5, 5));
        // Below both floors there is nothing to report.
        assert_eq!(mixed_timeslice_percent(511, 511), None);
        // Work below one floor still contributes when the other is reportable.
        assert!(combined(511, 512) >= 1);
        assert!(combined(512, 511) >= 1);

        for bytes in [0usize, 511, 512, 1599, 1600, 20_000, 1 << 20] {
            for nodes in [0usize, 511, 512, 20_000, 1 << 20] {
                let got = mixed_timeslice_percent(bytes, nodes).unwrap_or(0);
                assert!((0..=100).contains(&got), "{bytes}/{nodes}: {got}");
                let only_bytes = combined(bytes, 0);
                assert!(
                    got >= only_bytes,
                    "{bytes}/{nodes}: nodes reduced the charge"
                );
            }
        }
    }

    /// Malformed inputs whose DOM and visitor parsers report different offsets.
    fn malformed_documents() -> [(&'static str, Vec<u8>); 3] {
        let filler = 4_000;
        [
            (
                "rejected on byte 0, bad byte at the end",
                [b"!".to_vec(), vec![b' '; filler], vec![0xff]].concat(),
            ),
            (
                "a whole valid document, then a bad trailing byte",
                [br#"{"a":1}"#.to_vec(), vec![b' '; filler], vec![0xff]].concat(),
            ),
            (
                "bad byte inside a string the parser is part way through",
                [
                    b"\"".to_vec(),
                    vec![b'a'; filler],
                    vec![0xff],
                    b"\"".to_vec(),
                ]
                .concat(),
            ),
        ]
    }

    /// Visitor used only to collect parser errors without building terms.
    struct NoopVisitor;

    impl<'de> sonic_rs::JsonVisitor<'de> for NoopVisitor {}

    /// Each parser is charged to the offset it reports, even when they choose
    /// different faults from the same input.
    #[test]
    fn parser_errors_are_charged_to_their_reported_offsets() {
        let dom_reports_late = [false, true, true];

        for ((what, doc), dom_late) in malformed_documents().into_iter().zip(dom_reports_late) {
            let visitor_err = sonic_rs::parse_into_visitor(&doc, &mut NoopVisitor).unwrap_err();
            let visitor_charge = bytes_scanned(&visitor_err, doc.len());
            assert!(visitor_charge <= doc.len(), "{what}: visitor over-charged");
            assert!(
                visitor_charge > doc.len() / 2,
                "{what}: visitor charged {visitor_charge} of {} for offset {}",
                doc.len(),
                visitor_err.offset()
            );

            let dom_err = sonic_rs::from_slice::<sonic_rs::Value>(&doc).unwrap_err();
            let dom_charge = bytes_scanned(&dom_err, doc.len());
            assert!(dom_charge <= doc.len(), "{what}: DOM over-charged");
            assert_eq!(
                dom_charge > doc.len() / 2,
                dom_late,
                "{what}: DOM charged {dom_charge} of {} for offset {}",
                doc.len(),
                dom_err.offset()
            );
        }
    }
}
