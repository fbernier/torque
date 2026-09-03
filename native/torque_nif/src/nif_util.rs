use std::mem::MaybeUninit;

use rustler::sys::{
    enif_get_map_size, enif_make_map_from_arrays, enif_make_tuple_from_array,
    enif_map_iterator_create, enif_map_iterator_destroy, enif_map_iterator_get_pair,
    enif_map_iterator_next, ErlNifMapIterator, ErlNifMapIteratorEntry, ERL_NIF_TERM,
};
use rustler::{Env, Term};

use crate::map_order::FLATMAP_LIMIT;

/// Build a 2-tuple from two raw NIF terms.
#[inline]
pub fn make_tuple2<'a>(env: Env<'a>, a: ERL_NIF_TERM, b: ERL_NIF_TERM) -> Term<'a> {
    let arr: [ERL_NIF_TERM; 2] = [a, b];
    unsafe {
        Term::new(
            env,
            enif_make_tuple_from_array(env.as_c_arg(), arr.as_ptr(), 2),
        )
    }
}

/// Build a 3-tuple from three raw NIF terms.
#[inline]
pub fn make_tuple3<'a>(
    env: Env<'a>,
    a: ERL_NIF_TERM,
    b: ERL_NIF_TERM,
    c: ERL_NIF_TERM,
) -> Term<'a> {
    let arr: [ERL_NIF_TERM; 3] = [a, b, c];
    unsafe {
        Term::new(
            env,
            enif_make_tuple_from_array(env.as_c_arg(), arr.as_ptr(), 3),
        )
    }
}

pub const BYTES_PER_REDUCTION: usize = 20;
/// Reductions per full BEAM timeslice (CONTEXT_REDS).
pub const REDUCTION_COUNT: usize = 4000;

/// Compute a timeslice percentage (1–100) proportional to bytes processed.
#[inline]
pub fn timeslice_percent(bytes: usize) -> i32 {
    let reds = bytes / BYTES_PER_REDUCTION;
    ((reds * 100 / REDUCTION_COUNT) as i32).clamp(1, 100)
}

/// Build a map from key and value arrays.
///
/// Returns false if ERTS rejected the members or if duplicate keys collapsed
/// the map size (handling OTP bug erlang/otp#10975 for large maps).
///
/// # Safety
///
/// `keys` and `vals` must point to `count` initialized terms, and `map` to a
/// writable term.
#[inline]
pub unsafe fn map_from_arrays(
    env: Env,
    keys: *const ERL_NIF_TERM,
    vals: *const ERL_NIF_TERM,
    count: usize,
    map: *mut ERL_NIF_TERM,
) -> bool {
    if enif_make_map_from_arrays(env.as_c_arg(), keys, vals, count, map) == 0 {
        return false;
    }
    if count <= FLATMAP_LIMIT {
        return true;
    }
    let mut size = 0;
    enif_get_map_size(env.as_c_arg(), *map, &mut size) != 0 && size == count
}

/// Single-direction forward iterator over map entries.
///
/// Avoids the overhead of bidirectional iterators when only forward traversal
/// is needed. Destroys the underlying ERTS iterator on drop.
pub struct MapEntries<'a> {
    env: Env<'a>,
    iter: ErlNifMapIterator,
}

impl<'a> MapEntries<'a> {
    pub fn new(env: Env<'a>, map: Term<'a>) -> Option<Self> {
        let mut iter = MaybeUninit::<ErlNifMapIterator>::uninit();
        // SAFETY: `enif_map_iterator_create` initialises `iter` and reports
        // whether it did; anything but a map leaves it untouched and fails.
        let created = unsafe {
            enif_map_iterator_create(
                env.as_c_arg(),
                map.as_c_arg(),
                iter.as_mut_ptr(),
                ErlNifMapIteratorEntry::ERL_NIF_MAP_ITERATOR_HEAD,
            )
        };
        if created == 0 {
            return None;
        }
        Some(MapEntries {
            env,
            // SAFETY: created, as just checked.
            iter: unsafe { iter.assume_init() },
        })
    }
}

impl<'a> Iterator for MapEntries<'a> {
    type Item = (Term<'a>, Term<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        let (mut key, mut val) = (0 as ERL_NIF_TERM, 0 as ERL_NIF_TERM);
        // SAFETY: the iterator is live for as long as `self` is, and both terms
        // it hands back belong to `self.env`.
        unsafe {
            if enif_map_iterator_get_pair(self.env.as_c_arg(), &mut self.iter, &mut key, &mut val)
                == 0
            {
                return None;
            }
            enif_map_iterator_next(self.env.as_c_arg(), &mut self.iter);
            Some((Term::new(self.env, key), Term::new(self.env, val)))
        }
    }
}

impl Drop for MapEntries<'_> {
    fn drop(&mut self) {
        // SAFETY: created in `new` and destroyed exactly once, here.
        unsafe { enif_map_iterator_destroy(self.env.as_c_arg(), &mut self.iter) };
    }
}
