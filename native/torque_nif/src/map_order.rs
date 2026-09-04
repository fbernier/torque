//! Erlang term ordering for JSON object keys.
//!
//! `enif_make_map_from_arrays` sorts unordered keys via insertion sort in ERTS
//! (`erts_validate_and_sort_flatmap`), which is O(n²) in key comparisons.
//! Since non-BEAM JSON producers emit declaration order, sorting keys before
//! term creation using raw bytes avoids expensive compound term comparisons.

use std::cell::RefCell;
use std::mem::MaybeUninit;

/// Largest ERTS flatmap. Larger maps use hash ordering and gain nothing from
/// pre-sorting. Mirrors `MAP_SMALL_MAP_LIMIT` in ERTS.
pub const FLATMAP_LIMIT: usize = 32;

/// Smallest object whose ERTS insertion sort repays our fixed setup cost.
/// Keep this shared threshold aligned with both member-count benchmark sweeps.
pub const MIN_ORDERED_MEMBERS: usize = 4;

/// Big-endian key prefix for the common integer-comparison path. Equal prefixes
/// fall back to the full key, which covers long shared prefixes and NUL bytes.
#[inline]
pub fn prefix_be(bytes: &[u8]) -> u64 {
    let n = if bytes.len() < 8 { bytes.len() } else { 8 };
    let mut buf = [0u8; 8];
    buf[..n].copy_from_slice(&bytes[..n]);
    u64::from_be_bytes(buf)
}

#[derive(Clone, Copy)]
struct SortKey {
    prefix: u64,
    idx: u32,
}

/// Whether keys are already in Erlang binary order.
/// `prefixes.len()` must not exceed [`FLATMAP_LIMIT`].
#[inline]
fn ordered<F>(prefixes: &[u64], tie_lt: &mut F) -> bool
where
    F: FnMut(usize, usize) -> bool,
{
    debug_assert!(prefixes.len() <= FLATMAP_LIMIT);
    if prefixes.len() < 2 || prefixes.windows(2).all(|w| w[0] < w[1]) {
        return true;
    }
    // Equal prefixes need the full key comparison.
    prefixes
        .windows(2)
        .enumerate()
        .all(|(i, w)| w[0] < w[1] || (w[0] == w[1] && !tie_lt(i + 1, i)))
}

/// Stable insertion sort into `perm[..prefixes.len()]`.
/// `tie_lt` resolves equal prefixes; `perm` must have room for every prefix.
#[inline]
fn sort_keys<F>(prefixes: &[u64], perm: &mut [u8], mut tie_lt: F)
where
    F: FnMut(usize, usize) -> bool,
{
    let n = prefixes.len();
    debug_assert!(n <= FLATMAP_LIMIT && perm.len() >= n);
    let mut sk: [MaybeUninit<SortKey>; FLATMAP_LIMIT] = [MaybeUninit::uninit(); FLATMAP_LIMIT];
    for (i, &prefix) in prefixes.iter().enumerate() {
        let cur = SortKey {
            prefix,
            idx: i as u32,
        };
        let mut j = i;
        while j > 0 && {
            // SAFETY: sk[..i] was written by earlier iterations.
            let prev = unsafe { sk[j - 1].assume_init() };
            cur.prefix < prev.prefix || (cur.prefix == prev.prefix && tie_lt(i, prev.idx as usize))
        } {
            sk[j] = sk[j - 1];
            j -= 1;
        }
        sk[j].write(cur);
    }
    for (slot, entry) in perm[..n].iter_mut().zip(&sk[..n]) {
        // SAFETY: sk[..n] was fully written above.
        *slot = unsafe { entry.assume_init() }.idx as u8;
    }
}

/// Reorders members into Erlang term order and calls `permute` only when needed.
/// `prefixes.len()` must not exceed [`FLATMAP_LIMIT`].
#[inline]
fn order_members<F, P>(prefixes: &[u64], mut tie_lt: F, permute: P) -> bool
where
    F: FnMut(usize, usize) -> bool,
    P: FnOnce(&[u8]),
{
    if prefixes.len() < MIN_ORDERED_MEMBERS || ordered(prefixes, &mut tie_lt) {
        return false;
    }
    permute_members(prefixes, tie_lt, permute);
    true
}

/// `order_members` for callers holding the members themselves: derives each
/// prefix and applies the member-count bounds, so the prefix scratch lives here
/// rather than once per conversion path.
#[inline]
pub fn order_members_of<T, K, F, P>(members: &[T], prefix_of: K, tie_lt: F, permute: P) -> bool
where
    K: Fn(&T) -> u64,
    F: FnMut(usize, usize) -> bool,
    P: FnOnce(&[u8]),
{
    let n = members.len();
    if !(MIN_ORDERED_MEMBERS..=FLATMAP_LIMIT).contains(&n) {
        return false;
    }
    let mut prefixes: [MaybeUninit<u64>; FLATMAP_LIMIT] = [MaybeUninit::uninit(); FLATMAP_LIMIT];
    for (slot, member) in prefixes[..n].iter_mut().zip(members) {
        slot.write(prefix_of(member));
    }
    // SAFETY: prefixes[..n] was initialized above.
    let prefixes = unsafe { std::slice::from_raw_parts(prefixes.as_ptr().cast(), n) };
    order_members(prefixes, tie_lt, permute)
}

/// Kept out of line so ordered input never touches the thread-local cache.
#[inline(never)]
fn permute_members<F, P>(prefixes: &[u64], tie_lt: F, permute: P)
where
    F: FnMut(usize, usize) -> bool,
    P: FnOnce(&[u8]),
{
    SHAPES.with(|cell| permute(cell.borrow_mut().permutation(prefixes, tie_lt)));
}

thread_local! {
    /// Shape cache shared by the two non-overlapping conversion paths.
    static SHAPES: RefCell<ShapeCache> = const { RefCell::new(ShapeCache::new()) };
}

/// Enough direct-mapped slots for interleaved record and nested-object shapes.
const SHAPE_SLOTS: usize = 8;
const _: () = assert!(SHAPE_SLOTS.is_power_of_two());

/// Shift selecting the well-mixed high bits for the current slot count.
const SHAPE_SHIFT: u32 = u64::BITS - SHAPE_SLOTS.trailing_zeros();

struct ShapeEntry {
    /// Member count; zero marks an unused entry.
    len: u32,
    /// Adjacent sorted members whose prefixes tie and require full-key checks.
    ties: u32,
    prefixes: [u64; FLATMAP_LIMIT],
    /// Source member at each sorted position.
    perm: [u8; FLATMAP_LIMIT],
}

const EMPTY_SHAPE: ShapeEntry = ShapeEntry {
    len: 0,
    ties: 0,
    prefixes: [0; FLATMAP_LIMIT],
    perm: [0; FLATMAP_LIMIT],
};

/// Direct-mapped cache of permutations for repeated object shapes.
/// Prefix ties are checked against full keys before a cached order is reused.
struct ShapeCache {
    entries: [ShapeEntry; SHAPE_SLOTS],
}

impl ShapeCache {
    const fn new() -> ShapeCache {
        ShapeCache {
            entries: [EMPTY_SHAPE; SHAPE_SLOTS],
        }
    }

    /// Cached or newly sorted permutation for `prefixes`.
    #[inline]
    fn permutation<F>(&mut self, prefixes: &[u64], mut tie_lt: F) -> &[u8]
    where
        F: FnMut(usize, usize) -> bool,
    {
        let n = prefixes.len();
        debug_assert!((MIN_ORDERED_MEMBERS..=FLATMAP_LIMIT).contains(&n));
        let slot = slot_of(prefixes);
        if self.entries[slot].hits(prefixes, &mut tie_lt) {
            return &self.entries[slot].perm[..n];
        }

        let entry = &mut self.entries[slot];
        entry.len = n as u32;
        entry.prefixes[..n].copy_from_slice(prefixes);
        sort_keys(prefixes, &mut entry.perm[..n], tie_lt);
        entry.ties = 0;
        for i in 0..n - 1 {
            let (a, b) = (entry.perm[i] as usize, entry.perm[i + 1] as usize);
            entry.ties |= u32::from(prefixes[a] == prefixes[b]) << i;
        }
        &entry.perm[..n]
    }
}

/// Direct-mapped slot derived from shape length and boundary prefixes.
#[inline]
fn slot_of(prefixes: &[u64]) -> usize {
    let n = prefixes.len();
    let h = (prefixes[0] ^ prefixes[n - 1].rotate_left(32) ^ n as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (h >> SHAPE_SHIFT) as usize
}

impl ShapeEntry {
    /// Whether this entry matches the shape, including full-key prefix ties.
    #[inline]
    fn hits<F>(&self, prefixes: &[u64], tie_lt: &mut F) -> bool
    where
        F: FnMut(usize, usize) -> bool,
    {
        let n = prefixes.len();
        if self.len as usize != n || self.prefixes[..n] != prefixes[..n] {
            return false;
        }
        let mut ties = self.ties;
        while ties != 0 {
            let i = ties.trailing_zeros() as usize;
            ties &= ties - 1;
            // Different key tails require a fresh sort.
            if tie_lt(self.perm[i + 1] as usize, self.perm[i] as usize) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefixes_of(keys: &[&str]) -> Vec<u64> {
        keys.iter().map(|k| prefix_be(k.as_bytes())).collect()
    }

    /// Returns tie-break work so tests can distinguish a cache hit from a re-sort.
    fn tie_breaks(keys: &[&str]) -> (usize, Option<Vec<String>>) {
        let prefixes = prefixes_of(keys);
        let mut ties = 0;
        let mut out = None;
        order_members(
            &prefixes,
            |a, b| {
                ties += 1;
                keys[a] < keys[b]
            },
            |perm| out = Some(perm.iter().map(|&m| keys[m as usize].to_string()).collect()),
        );
        (ties, out)
    }

    fn sorted(keys: &[&str]) -> Option<Vec<String>> {
        tie_breaks(keys).1
    }

    fn expected(keys: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
        v.sort();
        v
    }

    #[test]
    fn already_ordered_input_is_reported_as_such() {
        for keys in [
            &[][..],
            &["solo"],
            &["a", "b", "c"],
            &["a", "ab", "abc"],
            &["created_at", "created_by"],
            &["a", "a\u{0}", "ab"],
            &["customer_id", "customer_name", "id", "zone"],
            &["dup", "dup", "dup"],
        ] {
            assert_eq!(sorted(keys), None, "{keys:?} was permuted");
        }
    }

    #[test]
    fn unordered_input_lands_in_term_order() {
        for keys in [
            &["zone", "id", "created_at", "a", "", "role", "balance"][..],
            &["created_zz", "created_aa", "created_mm", "created_bb"],
            &["abcdefghij", "abcdefgh", "abcdefghi", "abcdefghij0"],
            &["a\u{0}b", "a", "ab", "b"],
            &["a\u{0}", "a", "ab", "b"],
            &["created_by", "created_at", "created_cc", "created_ab"],
        ] {
            assert_eq!(
                sorted(keys).expect("must be sorted, not short-circuited"),
                expected(keys),
                "{keys:?}"
            );
        }
        // Ensure these fixtures take the equal-prefix path.
        assert_eq!(prefix_be(b"a"), prefix_be(b"a\0"));
    }

    #[test]
    fn objects_below_the_threshold_are_left_to_erts() {
        assert_eq!(sorted(&["b", "a"]), None);
        assert_eq!(sorted(&["created_by", "created_at"]), None);
        assert_eq!(sorted(&["c", "b", "a"]), None);
        assert_eq!(sorted(&["created_zz", "created_aa", "created_mm"]), None);
        assert_eq!(MIN_ORDERED_MEMBERS, 4);
        assert!(sorted(&["d", "c", "b", "a"]).is_some());
    }

    #[test]
    fn equal_keys_keep_source_order() {
        let keys = ["z", "a", "z", "b"];
        let mut perm = None;
        order_members(
            &prefixes_of(&keys),
            |a, b| keys[a] < keys[b],
            |p| perm = Some(p.to_vec()),
        );
        assert_eq!(perm, Some(vec![1, 3, 0, 2]));
    }

    /// Shared prefixes force tie-breaks, making memo hits observable.
    fn tied_shape(prefix: &str) -> Vec<String> {
        (0..8u8)
            .map(|i| format!("{prefix}{}", (b'h' - i) as char))
            .collect()
    }

    #[test]
    fn a_shape_seen_before_is_answered_from_the_memo() {
        let owned = tied_shape("created_");
        let keys: Vec<&str> = owned.iter().map(String::as_str).collect();
        let (miss, first) = tie_breaks(&keys);
        assert_eq!(first, Some(expected(&keys)));

        for _ in 0..4 {
            let (hit, again) = tie_breaks(&keys);
            assert_eq!(again, first);
            assert!(
                hit < miss,
                "shape re-sorted: {hit} tie-breaks against {miss}"
            );
        }
    }

    #[test]
    fn a_colliding_shape_does_not_borrow_the_wrong_permutation() {
        let first = ["created_zz", "created_aa", "created_mm", "created_bb"];
        let second = ["created_mm", "created_zz", "created_aa", "created_bb"];
        let prefixes = |keys: [&str; 4]| keys.map(|k| prefix_be(k.as_bytes()));
        assert_eq!(
            prefixes(first),
            prefixes(second),
            "the two shapes must collide for this to test anything"
        );

        for _ in 0..4 {
            assert_eq!(sorted(&first).unwrap(), expected(&first));
            assert_eq!(sorted(&second).unwrap(), expected(&second));
        }
    }

    #[test]
    fn interleaved_shapes_do_not_evict_each_other() {
        let owned: Vec<Vec<String>> = ["created_", "customer", "profile_"]
            .iter()
            .map(|prefix| tied_shape(prefix))
            .collect();
        let sets: Vec<Vec<&str>> = owned
            .iter()
            .map(|shape| shape.iter().map(String::as_str).collect())
            .collect();

        let mut slots: Vec<usize> = sets
            .iter()
            .map(|keys| slot_of(&prefixes_of(keys)))
            .collect();
        slots.sort_unstable();
        slots.dedup();
        assert_eq!(slots.len(), sets.len(), "these shapes share a slot");

        let misses: Vec<usize> = sets.iter().map(|keys| tie_breaks(keys).0).collect();

        for _ in 0..4 {
            for (&miss, keys) in misses.iter().zip(&sets) {
                let (hit, perm) = tie_breaks(keys);
                assert_eq!(perm, Some(expected(keys)));
                assert!(hit < miss, "interleaved shapes evicted each other");
            }
        }
    }

    #[test]
    fn prefix_be_pads_short_keys_and_truncates_long_ones() {
        assert_eq!(prefix_be(b""), 0);
        assert_eq!(prefix_be(b"a"), 0x6100_0000_0000_0000);
        assert_eq!(prefix_be(b"abcdefgh"), u64::from_be_bytes(*b"abcdefgh"));
        assert_eq!(prefix_be(b"abcdefghXX"), u64::from_be_bytes(*b"abcdefgh"));
        assert!(prefix_be(b"ab") < prefix_be(b"abc"));
        assert!(prefix_be(b"ab") < prefix_be(b"ac"));
    }
}
