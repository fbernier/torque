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

/// Parse `json` by driving the push-based [`JsonVisitor`] directly over the
/// original input slice (no padding copy), so the borrowed `&str` handed to
/// `visit_str` for unescaped strings points into `json`. Added for Torque's
/// fused term-building decoder.
pub fn parse_into_visitor<'de, V>(json: &'de [u8], visitor: &mut V) -> Result<()>
where
    V: JsonVisitor<'de>,
{
    let mut parser = crate::parser::Parser::new(Read::from(json));
    let mut strbuf = Vec::new();
    parser.parse_dom(visitor, Some(&mut strbuf), 0)?;
    parser.parse_trailing()
}

/// Bytes the in-place parser reads past the end of the JSON.
pub const PADDING: usize = 64;

/// Writes `json` into `buffer`, exactly `PADDING` bytes longer, followed by
/// the string terminator sentinel and zeroes the in-place parser stops on.
pub fn pad_into(json: &[u8], buffer: &mut [u8]) {
    let (body, padding) = buffer.split_at_mut(json.len());
    body.copy_from_slice(json);
    padding[..3].copy_from_slice(b"x\"x");
    padding[3..].fill(0);
}

/// Parse `json` like [`from_slice`], but in place in `buffer`, the caller's
/// [`pad_into`] copy of it: the value's strings, unescaped where they stand,
/// point into `buffer`. Added for Torque's parsed documents.
///
/// # Safety
///
/// `buffer` must hold `pad_into(json, _)`'s output and stay allocated and
/// unmodified while the returned `Value`, or any clone of it, lives.
pub unsafe fn from_padded_in_place(buffer: &mut [u8], json: &[u8]) -> Result<Value> {
    if json.len() > u32::MAX as usize {
        return Err(crate::error::make_error(format!(
            "Only support JSON less than 4 GB, the input JSON is too large here, len is {}",
            json.len()
        )));
    }
    use crate::reader::Reader;
    // The same pre-scan, trailing check and UTF-8 verdict as `from_slice`.
    let mut parser = crate::parser::Parser::new(Read::from(json));
    let mut shared = std::sync::Arc::new(crate::value::shared::Shared::default());
    std::sync::Arc::as_ptr(&shared).expose_provenance();
    let smut = std::sync::Arc::get_mut(&mut shared).unwrap_unchecked();
    let mut value = Value::new();
    let n = value.parse_padded(buffer, json, Default::default(), smut)?;
    parser.read.eat(n);
    parser.parse_trailing()?;
    parser.read.check_utf8_final()?;
    Ok(value)
}

pub mod prelude;
