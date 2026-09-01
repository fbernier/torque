//! Vendored, Torque-patched copy of sonic-rs. See native/sonic-rs/Cargo.toml.
//! Upstream third-party code: lints are silenced (we only minimally patch it).
#![allow(warnings)]
#![allow(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![doc(test(attr(warn(unused))))]

mod config;
pub mod error;
mod index;
mod input;
mod pointer;
pub mod reader;
mod util;

pub mod extract;
pub mod format;
pub mod lazyvalue;
pub mod parser;
pub mod serde;
pub mod value;
pub mod writer;

// re-export FastStr
pub use ::faststr::FastStr;
// re-export the serde trait
pub use ::serde::{Deserialize, Serialize};
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
    to_object_iter_unchecked, ArrayJsonIter, LazyArray, LazyObject, LazyValue, ObjectJsonIter,
    OwnedLazyValue,
};
#[doc(inline)]
pub use crate::pointer::{JsonPointer, PointerNode, PointerTree};
#[doc(inline)]
pub use crate::serde::de::{MapAccess, SeqAccess};
#[doc(inline)]
pub use crate::serde::{
    from_reader, from_slice, from_slice_unchecked, from_str, to_lazyvalue, to_string,
    to_string_pretty, to_vec, to_vec_pretty, to_writer, to_writer_pretty, Deserializer,
    JsonNumberTrait, Number, RawNumber, Serializer, StreamDeserializer,
};
#[doc(inline)]
pub use crate::value::{
    from_value, get::get_by_schema, to_value, Array, JsonContainerTrait, JsonType,
    JsonValueMutTrait, JsonValueTrait, Object, Value, ValueRef,
};

// --- Torque patch: expose the native push-based visitor parse ---
pub use crate::value::visitor::JsonVisitor;

/// Returns whether `len` fits the parsers' `u32`-based offsets.
/// Expressing the limit this way remains valid on 32-bit targets.
#[inline]
pub fn json_too_large(len: usize) -> bool {
    len > u32::MAX as usize
}

/// Drives [`JsonVisitor`] over `json` without a padding copy. Unescaped strings
/// borrow `json`; size, trailing-content, and UTF-8 checks match serde parsing.
pub fn parse_into_visitor<'de, V>(json: &'de [u8], visitor: &mut V) -> Result<()>
where
    V: JsonVisitor<'de>,
{
    // Bring the `Reader` trait into scope for `check_utf8_final`.
    use crate::reader::Reader as _;

    if json_too_large(json.len()) {
        return Err(crate::error::make_error(format!(
            "Only support JSON less than 4 GB, the input JSON is too large here, len is {}",
            json.len()
        )));
    }
    let mut parser = crate::parser::Parser::new(Read::from(json));
    let mut strbuf = Vec::new();
    // `Some` preserves the input and uses scratch space only for escaped strings.
    // `None` unescapes in place and requires a padded, mutable buffer.
    parser.parse_dom(visitor, Some(&mut strbuf), 0)?;
    parser.parse_trailing()?;
    parser.read.check_utf8_final()
}

pub mod prelude;
