//! Vendored, Torque-patched copy of sonic-rs. See native/sonic-rs/Cargo.toml.
//! Upstream third-party code: lints are silenced (we only minimally patch it).
#![allow(warnings)]
#![allow(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![doc(test(attr(warn(unused))))]

mod config;
pub mod error;
mod index;
mod input;
mod parser;
mod pointer;
mod reader;
mod util;

pub mod format;
pub mod lazyvalue;
pub mod serde;
pub mod value;
pub mod writer;

// re-export FastStr
// re-export the serde trait
pub use ::serde::{Deserialize, Serialize};
pub use faststr::FastStr;
#[doc(inline)]
pub use reader::Read;

#[doc(inline)]
pub use crate::error::{Error, Result};
#[doc(inline)]
pub use crate::index::Index;
#[doc(inline)]
pub use crate::input::JsonInput;
#[doc(inline)]
pub use crate::lazyvalue::{
    get, get_from_bytes, get_from_bytes_unchecked, get_from_faststr, get_from_faststr_unchecked,
    get_from_slice, get_from_slice_unchecked, get_from_str, get_from_str_unchecked, get_many,
    get_many_unchecked, get_unchecked, to_array_iter, to_array_iter_unchecked, to_object_iter,
    to_object_iter_unchecked, ArrayJsonIter, LazyValue, ObjectJsonIter, OwnedLazyValue,
};
#[doc(inline)]
pub use crate::pointer::{JsonPointer, PointerNode, PointerTree};
#[doc(inline)]
pub use crate::serde::{
    from_slice, from_slice_unchecked, from_str, to_string, to_string_pretty, to_vec, to_vec_pretty,
    to_writer, to_writer_pretty, Deserializer, JsonNumberTrait, Number, RawNumber, Serializer,
    StreamDeserializer,
};
#[doc(inline)]
pub use crate::value::{
    from_value, to_value, Array, JsonContainerTrait, JsonType, JsonValueMutTrait, JsonValueTrait,
    Object, Value, ValueRef,
};

// --- Torque patch: expose the native push-based visitor parse ---
pub use crate::value::node::RawStr;
pub use crate::value::visitor::JsonVisitor;

/// Returns whether `len` fits the parsers' `u32`-based offsets.
/// Expressing the limit this way remains valid on 32-bit targets.
#[inline]
pub fn json_too_large(len: usize) -> bool {
    len > u32::MAX as usize
}

/// Parse `json` by driving the push-based [`JsonVisitor`] directly over the
/// original input slice (no padding copy), so the borrowed `&str` handed to
/// `visit_str` for unescaped strings points into `json`. Added for Torque's
/// fused term-building decoder.
pub fn parse_into_visitor<'de, V>(json: &'de [u8], visitor: &mut V) -> Result<()>
where
    V: JsonVisitor<'de>,
{
    // The same bound `from_trait` puts on the DOM parser, so both entry points
    // refuse the same input: offsets into the document are tracked as `u32` in
    // both this crate and the visitors built on it.
    if json_too_large(json.len()) {
        return Err(crate::error::make_error(format!(
            "Only support JSON less than 4 GiB, the input JSON is too large here, len is {}",
            json.len()
        )));
    }
    let mut parser = crate::parser::Parser::new(Read::from(json));
    let mut strbuf = Vec::new();
    parser.parse_dom2(visitor, &mut strbuf)?;
    parser.parse_trailing()
}

pub mod prelude;
