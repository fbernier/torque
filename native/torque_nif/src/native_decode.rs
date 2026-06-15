//! Fused single-pass decoder built on sonic-rs's native push-based parser.
//!
//! Unlike `serde_decode` (which drives sonic-rs's much slower serde
//! `Deserializer`), this implements sonic-rs's `JsonVisitor` directly, building
//! Erlang terms during the SIMD parse — no serde plumbing, no intermediate
//! `Value` tree, and zero-copy sub-binaries for unescaped strings.
//!
//! Terms are assembled with a postfix value stack: scalars push a term; a
//! container-end pops its children, builds the list/map term, and pushes the
//! result. After a successful parse the stack holds exactly the root term.

use rustler::sys::{
    enif_make_double, enif_make_int64, enif_make_list_from_array, enif_make_map_from_arrays,
    enif_make_map_put, enif_make_new_map, enif_make_sub_binary, enif_make_uint64, ERL_NIF_TERM,
};
use rustler::{Encoder, Env, NewBinary, Term};
use sonic_rs::JsonVisitor;
use std::mem::MaybeUninit;

use crate::atoms;
use crate::nif_util::make_tuple2;
use crate::types::MAX_DEPTH;

const STACK_SIZE: usize = 64;

struct InputRef {
    term: ERL_NIF_TERM,
    base: *const u8,
    len: usize,
}

struct TermBuilder<'a> {
    env: Env<'a>,
    input: InputRef,
    /// Postfix value stack: completed terms plus the open containers' children.
    values: Vec<ERL_NIF_TERM>,
    /// `values` index where each currently-open container's children begin.
    frames: Vec<usize>,
    too_deep: bool,
}

impl<'a> TermBuilder<'a> {
    #[inline]
    fn push(&mut self, term: ERL_NIF_TERM) {
        self.values.push(term);
    }

    /// Sub-binary (zero-copy) when the str lives in the input buffer, else copy
    /// (escaped strings are unescaped into the parser's scratch buffer).
    #[inline]
    fn str_term(&self, s: &str) -> ERL_NIF_TERM {
        let ptr = s.as_ptr();
        if ptr >= self.input.base {
            let offset = unsafe { ptr.offset_from(self.input.base) } as usize;
            let len = s.len();
            if offset + len <= self.input.len {
                return unsafe {
                    enif_make_sub_binary(self.env.as_c_arg(), self.input.term, offset, len)
                };
            }
        }
        let mut binary = NewBinary::new(self.env, s.len());
        binary.as_mut_slice().copy_from_slice(s.as_bytes());
        let term: Term = binary.into();
        term.as_c_arg()
    }
}

/// Build a map term from interleaved `[k0, v0, k1, v1, ...]` children.
/// De-interleaves into separate key/value arrays for `enif_make_map_from_arrays`.
#[inline]
fn build_map(env: Env, kv: &[ERL_NIF_TERM]) -> ERL_NIF_TERM {
    let pairs = kv.len() / 2;
    if pairs <= STACK_SIZE {
        let mut keys: [MaybeUninit<ERL_NIF_TERM>; STACK_SIZE] = [MaybeUninit::uninit(); STACK_SIZE];
        let mut vals: [MaybeUninit<ERL_NIF_TERM>; STACK_SIZE] = [MaybeUninit::uninit(); STACK_SIZE];
        for i in 0..pairs {
            keys[i].write(kv[2 * i]);
            vals[i].write(kv[2 * i + 1]);
        }
        // SAFETY: keys[..pairs]/vals[..pairs] were just written.
        unsafe {
            make_map(
                env,
                std::slice::from_raw_parts(keys.as_ptr() as *const ERL_NIF_TERM, pairs),
                std::slice::from_raw_parts(vals.as_ptr() as *const ERL_NIF_TERM, pairs),
            )
        }
    } else {
        let mut keys = Vec::with_capacity(pairs);
        let mut vals = Vec::with_capacity(pairs);
        for i in 0..pairs {
            keys.push(kv[2 * i]);
            vals.push(kv[2 * i + 1]);
        }
        make_map(env, &keys, &vals)
    }
}

#[inline]
fn make_map(env: Env, keys: &[ERL_NIF_TERM], vals: &[ERL_NIF_TERM]) -> ERL_NIF_TERM {
    unsafe {
        let mut map: ERL_NIF_TERM = 0;
        if enif_make_map_from_arrays(
            env.as_c_arg(),
            keys.as_ptr(),
            vals.as_ptr(),
            keys.len(),
            &mut map,
        ) != 0
        {
            map
        } else {
            // Duplicate keys: last value wins (matches value_to_term/serde_decode).
            map = enif_make_new_map(env.as_c_arg());
            for i in 0..keys.len() {
                let mut new_map: ERL_NIF_TERM = 0;
                enif_make_map_put(env.as_c_arg(), map, keys[i], vals[i], &mut new_map);
                map = new_map;
            }
            map
        }
    }
}

impl<'de, 'a> JsonVisitor<'de> for TermBuilder<'a> {
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

    #[inline]
    fn visit_str(&mut self, value: &str) -> bool {
        let t = self.str_term(value);
        self.push(t);
        true
    }

    #[inline]
    fn visit_array_start(&mut self, _hint: usize) -> bool {
        if self.frames.len() >= MAX_DEPTH as usize {
            self.too_deep = true;
            return false;
        }
        self.frames.push(self.values.len());
        true
    }

    #[inline]
    fn visit_array_end(&mut self, _len: usize) -> bool {
        let start = match self.frames.pop() {
            Some(s) => s,
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
        self.frames.push(self.values.len());
        true
    }

    #[inline]
    fn visit_object_end(&mut self, _len: usize) -> bool {
        let start = match self.frames.pop() {
            Some(s) => s,
            None => return false,
        };
        let map = build_map(self.env, &self.values[start..]);
        self.values.truncate(start);
        self.values.push(map);
        true
    }
}

pub fn decode_to_term<'a>(env: Env<'a>, input_term: ERL_NIF_TERM, bytes: &[u8]) -> Term<'a> {
    let mut builder = TermBuilder {
        env,
        input: InputRef {
            term: input_term,
            base: bytes.as_ptr(),
            len: bytes.len(),
        },
        values: Vec::with_capacity(64),
        frames: Vec::with_capacity(16),
        too_deep: false,
    };

    match sonic_rs::parse_into_visitor(bytes, &mut builder) {
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
    }
}
