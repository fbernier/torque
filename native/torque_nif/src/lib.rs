mod atoms;
mod decoder;
mod encoder;
mod escape;
pub(crate) mod native_decode;
pub(crate) mod nif_util;
mod types;

pub struct ParsedDocument {
    pub value: sonic_rs::Value,
    pub unique_keys: bool,
}

#[rustler::resource_impl]
impl rustler::Resource for ParsedDocument {}

/// A single pre-compiled JSON Pointer segment (see `decoder::compile_one`).
pub enum PathSeg {
    Key(String),
    // numeric segment: index if container is array, else object key
    Num { idx: usize, key: String },
}

/// A reusable, pre-compiled set of JSON Pointer paths plus the object-key
/// lookup strategy to use when extracting them.
pub struct CompiledPaths {
    pub paths: Vec<Vec<PathSeg>>,
    pub unique_keys: bool,
}

#[rustler::resource_impl]
impl rustler::Resource for CompiledPaths {}

rustler::init!("Elixir.Torque.Native");
