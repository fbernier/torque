use rustler::sys::{enif_make_list_from_array, enif_make_map_put, enif_make_new_map, ERL_NIF_TERM};
use rustler::{Env, NewBinary, Term};
use sonic_rs::{JsonContainerTrait, JsonType, JsonValueTrait};
use std::mem::MaybeUninit;

use crate::atoms;
use crate::map_order::{order_members_of, prefix_be, FLATMAP_LIMIT, MIN_ORDERED_MEMBERS};
use crate::nif_util::map_from_arrays;

const STACK_SIZE: usize = 64;

/// Maximum JSON nesting depth accepted by `value_to_term`, the native decoder,
/// and the encoder. Inputs nested deeper than this return
/// `{:error, :nesting_too_deep}` rather than overflowing the stack and crashing
/// the VM. Sized for the small dirty-CPU-scheduler stack, which inputs >20 KB
/// are dispatched to: depths near 512 overflow it, so the limit is kept well below.
pub const MAX_DEPTH: u32 = 128;

#[inline]
fn make_binary_term<'a>(env: Env<'a>, s: &str) -> Term<'a> {
    let bytes = s.as_bytes();
    let mut binary = NewBinary::new(env, bytes.len());
    binary.as_mut_slice().copy_from_slice(bytes);
    binary.into()
}

/// Orders flatmap keys from their raw strings before ERTS sees the built terms.
/// The applied permutation is retained so duplicate-key fallback can recover
/// document order. Kept out of line so its scratch arrays do not enlarge the
/// recursive `value_to_term` frame.
#[inline(never)]
fn reorder_object(
    key_strs: &[&str],
    keys: &mut [ERL_NIF_TERM],
    vals: &mut [ERL_NIF_TERM],
    applied: &mut Option<[u8; FLATMAP_LIMIT]>,
) {
    let n = keys.len();
    debug_assert_eq!(key_strs.len(), n);
    order_members_of(
        key_strs,
        |key| prefix_be(key.as_bytes()),
        |a, b| key_strs[a] < key_strs[b],
        |perm| {
            let mut sorted_k: [MaybeUninit<ERL_NIF_TERM>; FLATMAP_LIMIT] =
                [MaybeUninit::uninit(); FLATMAP_LIMIT];
            let mut sorted_v: [MaybeUninit<ERL_NIF_TERM>; FLATMAP_LIMIT] =
                [MaybeUninit::uninit(); FLATMAP_LIMIT];
            for (i, &member) in perm.iter().enumerate() {
                let s = member as usize;
                sorted_k[i].write(keys[s]);
                sorted_v[i].write(vals[s]);
            }
            for i in 0..n {
                // SAFETY: both scratch arrays are initialized through `n`.
                keys[i] = unsafe { sorted_k[i].assume_init() };
                vals[i] = unsafe { sorted_v[i].assume_init() };
            }

            let mut order = [0u8; FLATMAP_LIMIT];
            order[..n].copy_from_slice(perm);
            *applied = Some(order);
        },
    );
}

/// Inserts in document order so duplicate keys keep their last value. `order`
/// maps sorted positions back to source members when pre-sorting ran.
#[cold]
fn dedup_built<'a>(
    env: Env<'a>,
    keys: &[ERL_NIF_TERM],
    vals: &[ERL_NIF_TERM],
    order: Option<[u8; FLATMAP_LIMIT]>,
) -> Option<Term<'a>> {
    let n = keys.len();
    let mut source = [0u8; FLATMAP_LIMIT];
    let source: &[u8] = match order {
        Some(order) => {
            debug_assert!(n <= FLATMAP_LIMIT);
            for (position, &member) in order[..n].iter().enumerate() {
                source[member as usize] = position as u8;
            }
            &source[..n]
        }
        None => &[],
    };

    unsafe {
        let mut map = enif_make_new_map(env.as_c_arg());
        for member in 0..n {
            let i = if source.is_empty() {
                member
            } else {
                source[member] as usize
            };
            let mut new_map: ERL_NIF_TERM = 0;
            enif_make_map_put(env.as_c_arg(), map, keys[i], vals[i], &mut new_map);
            map = new_map;
        }
        Some(Term::new(env, map))
    }
}

/// Fallback for Rust-built `Value` objects, which use a hash map rather than a
/// document pair slice and cannot contain duplicate keys.
#[cold]
fn object_from_map<'a>(
    env: Env<'a>,
    value: &sonic_rs::Value,
    depth: u32,
    nodes: &mut usize,
) -> Option<Term<'a>> {
    let obj = value.as_object()?;
    let child_depth = depth - 1;
    *nodes += 2 * obj.len();
    unsafe {
        let mut map = enif_make_new_map(env.as_c_arg());
        for (k, v) in obj.iter() {
            let key = make_binary_term(env, k).as_c_arg();
            let val = value_to_term(env, v, child_depth, nodes)?.as_c_arg();
            let mut new_map: ERL_NIF_TERM = 0;
            enif_make_map_put(env.as_c_arg(), map, key, val, &mut new_map);
            map = new_map;
        }
        Some(Term::new(env, map))
    }
}

/// Convert a sonic-rs Value to an Erlang term.
///
/// `depth` is the remaining nesting budget; returns `None` when it reaches zero
/// on an object or array, signalling that the document is too deeply nested.
///
/// `nodes` approximates the number of terms built, counted once per container
/// child (object children count double for their keys), so scalar conversions
/// cost nothing. Callers on normal schedulers use it for post-hoc timeslice
/// accounting — conversion work is proportional to the extracted subtree, not
/// to the pointer traversal that found it.
#[inline]
pub fn value_to_term<'a>(
    env: Env<'a>,
    value: &sonic_rs::Value,
    depth: u32,
    nodes: &mut usize,
) -> Option<Term<'a>> {
    match value.get_type() {
        JsonType::Null => Some(atoms::nil().to_term(env)),
        JsonType::Boolean => Some(if value.as_bool().unwrap() {
            atoms::r#true().to_term(env)
        } else {
            atoms::r#false().to_term(env)
        }),
        JsonType::Number => {
            if let Some(n) = value.as_i64() {
                Some(unsafe { Term::new(env, rustler::sys::enif_make_int64(env.as_c_arg(), n)) })
            } else if let Some(n) = value.as_u64() {
                Some(unsafe { Term::new(env, rustler::sys::enif_make_uint64(env.as_c_arg(), n)) })
            } else {
                value.as_f64().map(|n| unsafe {
                    Term::new(env, rustler::sys::enif_make_double(env.as_c_arg(), n))
                })
            }
        }
        JsonType::String => Some(make_binary_term(env, value.as_str().unwrap())),
        JsonType::Array => {
            if depth == 0 {
                return None;
            }
            let arr = value.as_value_slice().unwrap_or(&[]);
            let count = arr.len();
            let child_depth = depth - 1;
            *nodes += count;
            if count <= STACK_SIZE {
                let mut terms: [MaybeUninit<ERL_NIF_TERM>; STACK_SIZE] =
                    [MaybeUninit::uninit(); STACK_SIZE];
                for (i, v) in arr.iter().enumerate() {
                    terms[i].write(value_to_term(env, v, child_depth, nodes)?.as_c_arg());
                }
                unsafe {
                    Some(Term::new(
                        env,
                        enif_make_list_from_array(
                            env.as_c_arg(),
                            terms.as_ptr() as *const ERL_NIF_TERM,
                            count as u32,
                        ),
                    ))
                }
            } else {
                let mut terms: Vec<ERL_NIF_TERM> = Vec::with_capacity(count);
                for v in arr.iter() {
                    terms.push(value_to_term(env, v, child_depth, nodes)?.as_c_arg());
                }
                unsafe {
                    Some(Term::new(
                        env,
                        enif_make_list_from_array(env.as_c_arg(), terms.as_ptr(), count as u32),
                    ))
                }
            }
        }
        JsonType::Object => {
            if depth == 0 {
                return None;
            }
            let pairs = match value.as_pair_slice() {
                Some(pairs) => pairs,
                None => return object_from_map(env, value, depth, nodes),
            };
            let count = pairs.len();
            let child_depth = depth - 1;
            *nodes += 2 * count;
            if count <= STACK_SIZE {
                let mut keys: [MaybeUninit<ERL_NIF_TERM>; STACK_SIZE] =
                    [MaybeUninit::uninit(); STACK_SIZE];
                let mut vals: [MaybeUninit<ERL_NIF_TERM>; STACK_SIZE] =
                    [MaybeUninit::uninit(); STACK_SIZE];
                // Keep raw keys for ordering instead of unpacking each `Value` twice.
                let mut key_strs: [MaybeUninit<&str>; FLATMAP_LIMIT] =
                    [MaybeUninit::uninit(); FLATMAP_LIMIT];
                let orderable = (MIN_ORDERED_MEMBERS..=FLATMAP_LIMIT).contains(&count);
                for (i, (k, v)) in pairs.iter().enumerate() {
                    let key = k.as_node_str().unwrap_or("");
                    if orderable {
                        key_strs[i].write(key);
                    }
                    keys[i].write(make_binary_term(env, key).as_c_arg());
                    vals[i].write(value_to_term(env, v, child_depth, nodes)?.as_c_arg());
                }
                let mut applied = None;
                // SAFETY: keys and values are initialized through `count`; key strings
                // are also initialized through `count` when `orderable` is true.
                unsafe {
                    let (keys, vals) = (
                        std::slice::from_raw_parts_mut(keys.as_mut_ptr().cast(), count),
                        std::slice::from_raw_parts_mut(vals.as_mut_ptr().cast(), count),
                    );
                    if orderable {
                        let key_strs = std::slice::from_raw_parts(key_strs.as_ptr().cast(), count);
                        reorder_object(key_strs, keys, vals, &mut applied);
                    }

                    let mut map: ERL_NIF_TERM = 0;
                    if map_from_arrays(env, keys.as_ptr(), vals.as_ptr(), count, &mut map) {
                        Some(Term::new(env, map))
                    } else {
                        dedup_built(env, keys, vals, applied)
                    }
                }
            } else {
                let mut keys: Vec<ERL_NIF_TERM> = Vec::with_capacity(count);
                let mut vals: Vec<ERL_NIF_TERM> = Vec::with_capacity(count);
                for (k, v) in pairs.iter() {
                    keys.push(make_binary_term(env, k.as_node_str().unwrap_or("")).as_c_arg());
                    vals.push(value_to_term(env, v, child_depth, nodes)?.as_c_arg());
                }
                let mut map: ERL_NIF_TERM = 0;
                unsafe {
                    if map_from_arrays(env, keys.as_ptr(), vals.as_ptr(), count, &mut map) {
                        Some(Term::new(env, map))
                    } else {
                        // Only flatmaps are reordered, so these remain in document order.
                        dedup_built(env, &keys, &vals, None)
                    }
                }
            }
        }
    }
}
