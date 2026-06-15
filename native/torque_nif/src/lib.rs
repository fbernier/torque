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

rustler::init!("Elixir.Torque.Native");
