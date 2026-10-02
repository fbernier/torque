mod atoms;
mod decoder;
mod encoder;
mod escape;
pub(crate) mod native_decode;
pub(crate) mod nif_util;
mod types;

/// A parsed document. A large one's strings point into `buffer`, which is
/// declared after `value` so the value is dropped first; a small one's live
/// in the value's own padded copy.
pub struct ParsedDocument {
    pub value: sonic_rs::Value,
    pub unique_keys: bool,
    pub buffer: Option<ParseBuffer>,
}

#[rustler::resource_impl]
impl rustler::Resource for ParsedDocument {}

/// Documents at or under this keep their strings in the parsed value and get
/// results copy them: the environment a `ParseBuffer` needs costs ~500
/// cycles, 6% of parsing a 1.2 KB request, and the term builder only pays off
/// on larger subtrees.
pub const BUFFER_DOCUMENTS_ABOVE: usize = 4096;

/// The padded copy of a large document's input, parsed in place, as a refc
/// binary held in its own environment. Every string of the document lies in
/// it, so a get result can be a sub-binary that the VM accounts for and that
/// keeps alive only this buffer, not the `Value` tree.
pub struct ParseBuffer {
    env: *mut rustler::sys::ErlNifEnv,
    term: rustler::sys::ERL_NIF_TERM,
    data: *const u8,
    len: usize,
}

// SAFETY: after `parse` nothing writes the environment, the binary term in it
// or the bytes until `drop` frees them; `enif_make_copy` only reads `term`.
unsafe impl Send for ParseBuffer {}
unsafe impl Sync for ParseBuffer {}

impl ParseBuffer {
    /// Parses `json` into a value whose strings point into the returned
    /// buffer, or into the value itself when `json` is small.
    pub fn parse(json: &[u8]) -> Result<(sonic_rs::Value, Option<Self>), sonic_rs::Error> {
        if json.len() <= BUFFER_DOCUMENTS_ABOVE {
            return Ok((sonic_rs::from_slice(json)?, None));
        }
        let Some(mut owned) = rustler::OwnedBinary::new(json.len() + sonic_rs::PADDING) else {
            return Ok((sonic_rs::from_slice(json)?, None));
        };
        sonic_rs::pad_into(json, owned.as_mut_slice());
        // SAFETY: the bytes stay put through `release`, and then for as long
        // as the environment holding them, which the document keeps.
        let value = unsafe { sonic_rs::from_padded_in_place(owned.as_mut_slice(), json)? };
        // SAFETY: ERTS aborts rather than return a null environment, and
        // `owner` is only used here, while that environment exists.
        let env = unsafe { rustler::sys::enif_alloc_env() };
        let owner = unsafe { rustler::Env::new(&json, env) };
        let binary = owned.release(owner);
        Ok((
            value,
            Some(ParseBuffer {
                env,
                term: binary.to_term(owner).as_c_arg(),
                data: binary.as_slice().as_ptr(),
                len: json.len(),
            }),
        ))
    }

    /// The parsed input, after in-place unescaping.
    #[inline]
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the environment keeps the binary alive as long as `self`.
        unsafe { std::slice::from_raw_parts(self.data, self.len) }
    }

    /// The buffer as a binary term of `env`, for sub-binaries of it.
    #[inline]
    pub fn term_in(&self, env: rustler::Env) -> rustler::sys::ERL_NIF_TERM {
        // SAFETY: `term` is a binary in an environment nothing writes; a
        // copy shares the binary and bumps its reference count.
        unsafe { rustler::sys::enif_make_copy(env.as_c_arg(), self.term) }
    }
}

impl Drop for ParseBuffer {
    fn drop(&mut self) {
        // SAFETY: allocated in `parse` and freed exactly once.
        unsafe { rustler::sys::enif_free_env(self.env) };
    }
}

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
}

#[rustler::resource_impl]
impl rustler::Resource for CompiledPaths {}

rustler::init!("Elixir.Torque.Native");
