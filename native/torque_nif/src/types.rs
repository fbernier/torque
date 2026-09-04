use rustler::sys::{enif_make_list_from_array, enif_make_map_put, enif_make_new_map, ERL_NIF_TERM};
use rustler::{Env, NewBinary, Term};
use sonic_rs::{JsonContainerTrait, JsonType, JsonValueTrait};
use std::mem::MaybeUninit;

use crate::atoms;
use crate::decoder::{Work, KEY_BYTES_PER_NODE};
use crate::map_order::{order_members_of, prefix_be, FLATMAP_LIMIT, MIN_ORDERED_MEMBERS};
use crate::native_decode::KeyCache;
use crate::nif_util::map_from_arrays;

const STACK_SIZE: usize = 64;

/// Maximum JSON nesting depth accepted by `value_to_term`, the native decoder,
/// and the encoder. Inputs nested deeper than this return
/// `{:error, :nesting_too_deep}` rather than overflowing the stack and crashing
/// the VM. Sized for the small dirty-CPU-scheduler stack, which inputs >20 KB
/// are dispatched to: depths near 512 overflow it, so the limit is kept well below.
pub const MAX_DEPTH: u32 = 128;

/// Depth errors are public lookup results; budget exhaustion requests a retry.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ConversionError {
    NestingTooDeep,
    DirtyRequired,
}

/// Per-NIF conversion state. The lifetime ties cached key pointers to values
/// retained for the entire call; raw terms never escape into another NIF env.
pub(crate) struct Conversion<'a, 'v> {
    env: Env<'a>,
    budget: usize,
    keys: Option<Box<KeyCache>>,
    keys_seen: usize,
    credit: usize,
    values: std::marker::PhantomData<&'v sonic_rs::Value>,
}

impl<'a, 'v> Conversion<'a, 'v> {
    #[inline]
    pub(crate) fn new(env: Env<'a>, budget: usize) -> Self {
        Self {
            env,
            budget,
            keys: None,
            keys_seen: 0,
            credit: 0,
            values: std::marker::PhantomData,
        }
    }

    #[inline]
    fn key(&mut self, key: &'v str, work: &mut Work) -> Result<ERL_NIF_TERM, ConversionError> {
        // A request-sized object should not allocate and zero an entire key
        // cache. Record arrays amortize copying the first stack-sized key set.
        if self.keys.is_none() && self.keys_seen < STACK_SIZE {
            self.keys_seen += 1;
            return self.string::<true>(key, work).map(|term| term.as_c_arg());
        }
        // Bound hashing/comparison as well as any copy before entering the cache.
        self.charge(0, key.len(), work)?;
        let cache = self.keys.get_or_insert_with(|| {
            let mut cache = Box::new(KeyCache::new());
            cache.next_epoch();
            cache
        });
        // SAFETY: all keys have the context's value lifetime, and this context
        // belongs to one env. Misses copy, so result binaries retain no arena.
        Ok(unsafe { cache.copied_key(self.env, key) })
    }

    #[inline]
    fn string<const NESTED: bool>(
        &mut self,
        s: &str,
        work: &mut Work,
    ) -> Result<Term<'a>, ConversionError> {
        if NESTED {
            self.charge(0, s.len(), work)?;
        } else {
            work.materialize(0, s.len(), self.budget)?;
        }
        let mut binary = NewBinary::new(self.env, s.len());
        binary.as_mut_slice().copy_from_slice(s.as_bytes());
        Ok(binary.into())
    }

    #[inline]
    fn charge(
        &mut self,
        nodes: usize,
        bytes: usize,
        work: &mut Work,
    ) -> Result<(), ConversionError> {
        if self.budget != usize::MAX {
            let cost = nodes
                .saturating_mul(KEY_BYTES_PER_NODE)
                .saturating_add(bytes);
            if cost > self.credit {
                // The cold refusal path computes document-versus-caller
                // attribution without putting those divisions in every key.
                let result = work.materialize(nodes, bytes, self.budget);
                if result.is_ok() {
                    self.credit = work.conversion_credit(self.budget);
                }
                return result;
            }
            self.credit -= cost;
        }
        work.record_materialized(nodes, bytes);
        Ok(())
    }
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
) -> Term<'a> {
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
        Term::new(env, map)
    }
}

/// Fallback for Rust-built `Value` objects, which use a hash map rather than a
/// document pair slice and cannot contain duplicate keys.
#[cold]
fn object_from_map<'a, 'v>(
    conversion: &mut Conversion<'a, 'v>,
    value: &'v sonic_rs::Value,
    depth: u32,
    work: &mut Work,
) -> Result<Term<'a>, ConversionError> {
    let obj = value.as_object().expect("object value");
    let env = conversion.env;
    let child_depth = depth - 1;
    conversion.charge(2 * obj.len(), 0, work)?;
    unsafe {
        let mut map = enif_make_new_map(env.as_c_arg());
        for (k, v) in obj.iter() {
            let key = conversion.key(k, work)?;
            let val = conversion.convert::<true>(v, child_depth, work)?.as_c_arg();
            let mut new_map: ERL_NIF_TERM = 0;
            enif_make_map_put(env.as_c_arg(), map, key, val, &mut new_map);
            map = new_map;
        }
        Ok(Term::new(env, map))
    }
}

impl<'a, 'v> Conversion<'a, 'v> {
    /// Converts one retained value, checking cumulative work before every
    /// container allocation and binary copy, including recursive descendants.
    #[inline]
    pub(crate) fn value_to_term(
        &mut self,
        value: &'v sonic_rs::Value,
        depth: u32,
        work: &mut Work,
    ) -> Result<Term<'a>, ConversionError> {
        self.convert::<false>(value, depth, work)
    }

    #[inline]
    fn convert<const NESTED: bool>(
        &mut self,
        value: &'v sonic_rs::Value,
        depth: u32,
        work: &mut Work,
    ) -> Result<Term<'a>, ConversionError> {
        let env = self.env;
        match value.get_type() {
            JsonType::Null => Ok(atoms::nil().to_term(env)),
            JsonType::Boolean => Ok(if value.as_bool().unwrap() {
                atoms::r#true().to_term(env)
            } else {
                atoms::r#false().to_term(env)
            }),
            JsonType::Number => {
                if let Some(n) = value.as_i64() {
                    Ok(unsafe { Term::new(env, rustler::sys::enif_make_int64(env.as_c_arg(), n)) })
                } else if let Some(n) = value.as_u64() {
                    Ok(
                        unsafe {
                            Term::new(env, rustler::sys::enif_make_uint64(env.as_c_arg(), n))
                        },
                    )
                } else {
                    Ok(unsafe {
                        Term::new(
                            env,
                            rustler::sys::enif_make_double(env.as_c_arg(), value.as_f64().unwrap()),
                        )
                    })
                }
            }
            JsonType::String => self.string::<NESTED>(value.as_str().unwrap(), work),
            JsonType::Array => {
                if depth == 0 {
                    return Err(ConversionError::NestingTooDeep);
                }
                if !NESTED {
                    self.credit = work.conversion_credit(self.budget);
                }
                let arr = value.as_value_slice().unwrap_or(&[]);
                let count = arr.len();
                let child_depth = depth - 1;
                self.charge(count, 0, work)?;
                if count <= STACK_SIZE {
                    let mut terms: [MaybeUninit<ERL_NIF_TERM>; STACK_SIZE] =
                        [MaybeUninit::uninit(); STACK_SIZE];
                    for (i, v) in arr.iter().enumerate() {
                        terms[i].write(self.convert::<true>(v, child_depth, work)?.as_c_arg());
                    }
                    unsafe {
                        Ok(Term::new(
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
                        terms.push(self.convert::<true>(v, child_depth, work)?.as_c_arg());
                    }
                    unsafe {
                        Ok(Term::new(
                            env,
                            enif_make_list_from_array(env.as_c_arg(), terms.as_ptr(), count as u32),
                        ))
                    }
                }
            }
            JsonType::Object => {
                if depth == 0 {
                    return Err(ConversionError::NestingTooDeep);
                }
                if !NESTED {
                    self.credit = work.conversion_credit(self.budget);
                }
                let pairs = match value.as_pair_slice() {
                    Some(pairs) => pairs,
                    None => return object_from_map(self, value, depth, work),
                };
                let count = pairs.len();
                let child_depth = depth - 1;
                self.charge(2 * count, 0, work)?;
                if count <= STACK_SIZE {
                    let mut keys: [MaybeUninit<ERL_NIF_TERM>; STACK_SIZE] =
                        [MaybeUninit::uninit(); STACK_SIZE];
                    let mut vals: [MaybeUninit<ERL_NIF_TERM>; STACK_SIZE] =
                        [MaybeUninit::uninit(); STACK_SIZE];
                    // Keep raw keys for ordering instead of unpacking each Value twice.
                    let mut key_strs: [MaybeUninit<&str>; FLATMAP_LIMIT] =
                        [MaybeUninit::uninit(); FLATMAP_LIMIT];
                    let orderable = (MIN_ORDERED_MEMBERS..=FLATMAP_LIMIT).contains(&count);
                    for (i, (k, v)) in pairs.iter().enumerate() {
                        let key = k.as_node_str().unwrap_or("");
                        if orderable {
                            key_strs[i].write(key);
                        }
                        keys[i].write(self.key(key, work)?);
                        vals[i].write(self.convert::<true>(v, child_depth, work)?.as_c_arg());
                    }
                    let mut applied = None;
                    // SAFETY: keys and values are initialized through count; key
                    // strings are also initialized when orderable is true.
                    unsafe {
                        let (keys, vals) = (
                            std::slice::from_raw_parts_mut(keys.as_mut_ptr().cast(), count),
                            std::slice::from_raw_parts_mut(vals.as_mut_ptr().cast(), count),
                        );
                        if orderable {
                            let key_strs =
                                std::slice::from_raw_parts(key_strs.as_ptr().cast(), count);
                            reorder_object(key_strs, keys, vals, &mut applied);
                        }
                        let mut map: ERL_NIF_TERM = 0;
                        if map_from_arrays(env, keys.as_ptr(), vals.as_ptr(), count, &mut map) {
                            Ok(Term::new(env, map))
                        } else {
                            Ok(dedup_built(env, keys, vals, applied))
                        }
                    }
                } else {
                    let mut keys: Vec<ERL_NIF_TERM> = Vec::with_capacity(count);
                    let mut vals: Vec<ERL_NIF_TERM> = Vec::with_capacity(count);
                    for (k, v) in pairs.iter() {
                        keys.push(self.key(k.as_node_str().unwrap_or(""), work)?);
                        vals.push(self.convert::<true>(v, child_depth, work)?.as_c_arg());
                    }
                    let mut map: ERL_NIF_TERM = 0;
                    unsafe {
                        if map_from_arrays(env, keys.as_ptr(), vals.as_ptr(), count, &mut map) {
                            Ok(Term::new(env, map))
                        } else {
                            // Only flatmaps are reordered; these retain source order.
                            Ok(dedup_built(env, &keys, &vals, None))
                        }
                    }
                }
            }
        }
    }
}
