mod atoms;
mod decoder;
mod encoder;
mod escape;
pub(crate) mod native_decode;
pub(crate) mod nif_util;
mod types;

/// `null` is the term JSON null becomes and `missing` the term a lookup returns
/// for an absent path: `nil` for both from Elixir, `null` and `undefined` from
/// Erlang.
pub struct ParsedDocument {
    pub value: sonic_rs::Value,
    pub unique_keys: bool,
    pub null: rustler::Atom,
    pub missing: rustler::Atom,
}

#[rustler::resource_impl]
impl rustler::Resource for ParsedDocument {}

/// A single pre-compiled JSON Pointer segment (see `decoder::compile_one`).
pub enum PathSeg {
    Key(String),
    // numeric segment: index if container is array, else object key
    Num { idx: usize, key: String },
}

/// Reusable JSON Pointer paths and their extraction policy. `paths` serves
/// parsed-document lookups; `plan` serves fused parse-and-extract calls.
pub struct CompiledPaths {
    pub paths: Vec<Vec<PathSeg>>,
    pub plan: sonic_rs::extract::ExtractPlan,
    pub unique_keys: bool,
    /// Whether fused extraction validates syntax in unselected regions.
    pub validate: bool,
    /// As in `ParsedDocument`, for fused extraction.
    pub null: rustler::Atom,
    pub missing: rustler::Atom,
}

#[rustler::resource_impl]
impl rustler::Resource for CompiledPaths {}

rustler::init!("Elixir.Torque.Native");
