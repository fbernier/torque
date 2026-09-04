use crate::atoms;
use crate::nif_util::{make_tuple2, make_tuple3, timeslice_percent, MapEntries};
use crate::types::MAX_DEPTH;
use rustler::sys::{
    c_int, c_uint, enif_get_atom, enif_get_double, enif_get_int64, enif_get_list_cell,
    enif_get_tuple, enif_get_uint64, enif_inspect_binary, enif_is_empty_list, enif_release_binary,
    enif_term_to_binary, ErlNifBinary, ErlNifCharEncoding, ErlNifEnv, ERL_NIF_TERM,
};
use rustler::{schedule, Binary, Encoder, Env, NewBinary, Term, TermType};
use std::cell::RefCell;
use std::mem::MaybeUninit;

/// Skip scheduler accounting when its call costs more than the work measured.
const TIMESLICE_MIN_BYTES: usize = 4096;

/// Maximum retained thread-local scratch buffer.
const BUF_RETAIN_CAP: usize = 1 << 20;

/// Output bytes allowed before a normal-scheduler root encode suspends.
const ENCODE_BUDGET: usize = 20 * 1024;

/// Hard normal-scheduler output limit. Roots suspend between elements at
/// `ENCODE_BUDGET`; an oversized nested element cannot resume and retries dirty.
const ENCODE_HARD_LIMIT: usize = 64 * 1024;

/// Enforces the hard limit between nested container members.
#[inline(always)]
fn check_budget<const BOUNDED: bool>(buf: &[u8]) -> Result<(), EncodeError> {
    if BOUNDED && buf.len() > ENCODE_HARD_LIMIT {
        return Err(EncodeError::DirtyRequired);
    }
    Ok(())
}

enum Progress {
    Done,
    /// Top-level element at which encoding should resume.
    Suspended {
        next: usize,
    },
}

/// Encodes a root term, resuming at top-level element `start` when nonzero.
/// `buf` then ends after element `start - 1`, before its following separator.
fn encode_root<'a, const BOUNDED: bool>(
    env: Env<'a>,
    env_raw: *mut ErlNifEnv,
    term: Term<'a>,
    buf: &mut Vec<u8>,
    start: usize,
) -> Result<Progress, EncodeError> {
    match term.get_type() {
        TermType::Map => {
            let iter = MapEntries::new(env, term).ok_or(EncodeError::UnsupportedType)?;
            if start == 0 {
                buf.push(b'{');
            }
            for (i, (key, value)) in iter.enumerate() {
                if i < start {
                    continue;
                }
                if BOUNDED && buf.len() > ENCODE_BUDGET {
                    return Ok(Progress::Suspended { next: i });
                }
                if i > 0 {
                    buf.push(b',');
                }
                encode_map_key::<BOUNDED>(env_raw, key, buf)?;
                buf.push(b':');
                encode_term::<BOUNDED>(env, env_raw, value, buf, MAX_DEPTH - 1)?;
            }
            buf.push(b'}');
            Ok(Progress::Done)
        }
        TermType::List => {
            if start == 0 {
                buf.push(b'[');
            }
            let mut current = term.as_c_arg();
            let mut head: ERL_NIF_TERM = 0;
            let mut tail: ERL_NIF_TERM = 0;
            let mut i = 0usize;
            while unsafe { enif_get_list_cell(env_raw, current, &mut head, &mut tail) } != 0 {
                if i >= start {
                    if BOUNDED && buf.len() > ENCODE_BUDGET {
                        return Ok(Progress::Suspended { next: i });
                    }
                    if i > 0 {
                        buf.push(b',');
                    }
                    let item = unsafe { Term::new(env, head) };
                    encode_term::<BOUNDED>(env, env_raw, item, buf, MAX_DEPTH - 1)?;
                }
                current = tail;
                i += 1;
            }
            if unsafe { enif_is_empty_list(env_raw, current) } == 0 {
                return Err(EncodeError::UnsupportedType);
            }
            buf.push(b']');
            Ok(Progress::Done)
        }
        // Anything else is a scalar or a proplist tuple: too small to suspend
        // usefully, or already handled by the recursive path.
        _ => encode_term::<BOUNDED>(env, env_raw, term, buf, MAX_DEPTH).map(|()| Progress::Done),
    }
}

thread_local! {
    /// Reused across encode calls on each scheduler thread. Avoids a
    /// malloc/free per call, which is the dominant per-call cost for small
    /// payloads. NIFs run to completion without preemption and the encoder
    /// never re-enters this NIF, so the borrow is never nested.
    static ENCODE_BUF: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(2048));
}

/// Call only after the result owns its bytes, including suspended partials.
/// Dropping an oversized allocation enforces the cap even when its length
/// exceeded it; `shrink_to` cannot shrink below the live length.
#[inline]
fn trim_buffer(buf: &mut Vec<u8>) {
    buf.clear();
    if buf.capacity() > BUF_RETAIN_CAP {
        *buf = Vec::new();
    }
}

enum EncodeError {
    /// Work passed the budget where no resume index exists; restart dirty.
    DirtyRequired,
    UnsupportedType,
    NonFiniteFloat,
    InvalidKey,
    MalformedProplist,
    DepthExceeded,
    InvalidUtf8,
}

#[inline]
fn error_reason(e: EncodeError) -> ERL_NIF_TERM {
    match e {
        EncodeError::DirtyRequired => atoms::dirty_required().as_c_arg(),
        EncodeError::DepthExceeded => atoms::nesting_too_deep().as_c_arg(),
        EncodeError::UnsupportedType => atoms::unsupported_type().as_c_arg(),
        EncodeError::NonFiniteFloat => atoms::non_finite_float().as_c_arg(),
        EncodeError::InvalidKey => atoms::invalid_key().as_c_arg(),
        EncodeError::MalformedProplist => atoms::malformed_proplist().as_c_arg(),
        EncodeError::InvalidUtf8 => atoms::invalid_utf8().as_c_arg(),
    }
}

/// Hand the finished scratch buffer to a freshly allocated Erlang binary,
/// reporting the work to the scheduler only when it's large enough to matter.
/// Timeslice accounting is skipped on dirty schedulers, where it's meaningless.
#[inline]
fn buf_to_binary<'a>(env: Env<'a>, buf: &[u8], report_timeslice: bool) -> Term<'a> {
    if report_timeslice && buf.len() >= TIMESLICE_MIN_BYTES {
        schedule::consume_timeslice(env, timeslice_percent(buf.len()));
    }
    let mut binary = NewBinary::new(env, buf.len());
    binary.as_mut_slice().copy_from_slice(buf);
    binary.into()
}

/// ERTS caps an atom at 255 characters; the Latin-1 read appends a NUL.
const ATOM_NAME_MAX: usize = 256;

/// Reads a Latin-1 atom name without allocation. Atoms outside Latin-1 return
/// `None` and take `write_wide_atom_name`; UTF-8 atom reads require NIF 2.17.
#[inline]
unsafe fn atom_to_stack_buf(
    env_raw: *mut ErlNifEnv,
    term_raw: ERL_NIF_TERM,
    buf: &mut [u8; ATOM_NAME_MAX],
) -> Option<&[u8]> {
    let len = enif_get_atom(
        env_raw,
        term_raw,
        buf.as_mut_ptr(),
        buf.len() as c_uint,
        ErlNifCharEncoding::ERL_NIF_LATIN1,
    );
    if len == 0 {
        None
    } else {
        // The return count includes the terminator, not just the prefix before
        // an embedded NUL. Even an empty atom therefore returns one.
        Some(&buf[..len as usize - 1])
    }
}

/// External term format: a version tag, then `SMALL_ATOM_UTF8_EXT` carrying a
/// one-byte name length or `ATOM_UTF8_EXT` a two-byte big-endian one.
const ETF_VERSION: u8 = 131;
const SMALL_ATOM_UTF8_EXT: u8 = 119;
const ATOM_UTF8_EXT: u8 = 118;

/// Appends an atom outside Latin-1 by reading its UTF-8 name from the external
/// term format. Only the cold fallback pays for the temporary binary.
#[cold]
fn write_wide_atom_name(
    env_raw: *mut ErlNifEnv,
    term_raw: ERL_NIF_TERM,
    buf: &mut Vec<u8>,
) -> Result<(), EncodeError> {
    let mut bin = MaybeUninit::<ErlNifBinary>::uninit();
    if unsafe { enif_term_to_binary(env_raw, term_raw, bin.as_mut_ptr()) } == 0 {
        return Err(EncodeError::UnsupportedType);
    }
    let mut bin = unsafe { bin.assume_init() };
    let bytes = unsafe { std::slice::from_raw_parts(bin.data, bin.size) };

    let name = match bytes {
        [ETF_VERSION, SMALL_ATOM_UTF8_EXT, len, rest @ ..] => rest.get(..*len as usize),
        [ETF_VERSION, ATOM_UTF8_EXT, hi, lo, rest @ ..] => {
            rest.get(..u16::from_be_bytes([*hi, *lo]) as usize)
        }
        _ => None,
    };
    let result = match name {
        Some(name) => {
            crate::escape::escape_to_vec(name, buf);
            Ok(())
        }
        None => Err(EncodeError::UnsupportedType),
    };
    unsafe { enif_release_binary(&mut bin) };
    result
}

/// Append an atom's name to `buf` as an escaped JSON string body (no quotes).
///
/// The Latin-1 read yields bytes >= 0x80 for the U+0080..U+00FF range, which
/// are transcoded to two-byte UTF-8 so the emitted JSON stays valid.
#[inline]
fn write_atom_name(
    env_raw: *mut ErlNifEnv,
    term_raw: ERL_NIF_TERM,
    buf: &mut Vec<u8>,
) -> Result<(), EncodeError> {
    let mut atom_buf = [0u8; ATOM_NAME_MAX];
    let Some(name) = (unsafe { atom_to_stack_buf(env_raw, term_raw, &mut atom_buf) }) else {
        return write_wide_atom_name(env_raw, term_raw, buf);
    };
    if name.is_ascii() {
        crate::escape::escape_to_vec(name, buf);
    } else {
        let mut utf8 = [0u8; ATOM_NAME_MAX * 2];
        let mut n = 0usize;
        for &b in name {
            if b < 0x80 {
                utf8[n] = b;
                n += 1;
            } else {
                utf8[n] = 0xC0 | (b >> 6);
                utf8[n + 1] = 0x80 | (b & 0x3F);
                n += 2;
            }
        }
        crate::escape::escape_to_vec(&utf8[..n], buf);
    }
    Ok(())
}

#[inline]
fn encode_impl<'a>(env: Env<'a>, term: Term<'a>, report_timeslice: bool) -> Term<'a> {
    let env_raw = env.as_c_arg();
    ENCODE_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        // A caught unwind may have skipped the normal scratch cleanup.
        buf.clear();
        let result = match encode_term::<false>(env, env_raw, term, &mut buf, MAX_DEPTH) {
            Ok(()) => {
                let bin_term = buf_to_binary(env, &buf, report_timeslice);
                make_tuple2(env, atoms::ok().as_c_arg(), bin_term.as_c_arg())
            }
            Err(e) => make_tuple2(env, atoms::error().as_c_arg(), error_reason(e)),
        };
        trim_buffer(&mut buf);
        result
    })
}

/// Normal-scheduler encode. Suspends into `{:suspended, partial, next}` rather
/// than discarding what it built, so the dirty rerun copies 20 KB instead of
/// re-encoding it.
#[inline]
fn encode_bounded<'a>(env: Env<'a>, term: Term<'a>, iodata: bool) -> Term<'a> {
    let env_raw = env.as_c_arg();
    ENCODE_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        buf.clear();
        let progress = encode_root::<true>(env, env_raw, term, &mut buf, 0)
            .and_then(|progress| check_budget::<true>(&buf).map(|()| progress));
        let result = match progress {
            Ok(Progress::Done) => {
                let bin = buf_to_binary(env, &buf, true);
                if iodata {
                    bin
                } else {
                    make_tuple2(env, atoms::ok().as_c_arg(), bin.as_c_arg())
                }
            }
            Ok(Progress::Suspended { next }) => {
                let mut partial = NewBinary::new(env, buf.len());
                partial.as_mut_slice().copy_from_slice(&buf);
                let partial: Term = partial.into();
                make_tuple3(
                    env,
                    atoms::suspended().as_c_arg(),
                    partial.as_c_arg(),
                    next.encode(env).as_c_arg(),
                )
            }
            Err(EncodeError::DirtyRequired) => atoms::dirty_required().to_term(env),
            Err(e) => encode_error_term(env, env_raw, e, iodata),
        };
        trim_buffer(&mut buf);
        result
    })
}

/// Dirty-scheduler completion: seeds the buffer with what the normal attempt
/// already produced and carries on from the element it stopped at.
#[inline]
fn encode_resume<'a>(
    env: Env<'a>,
    term: Term<'a>,
    partial: &[u8],
    next: usize,
    iodata: bool,
) -> Term<'a> {
    let env_raw = env.as_c_arg();
    ENCODE_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        buf.clear();
        buf.extend_from_slice(partial);
        let result = match encode_root::<false>(env, env_raw, term, &mut buf, next) {
            Ok(_) => {
                let bin = buf_to_binary(env, &buf, false);
                if iodata {
                    bin
                } else {
                    make_tuple2(env, atoms::ok().as_c_arg(), bin.as_c_arg())
                }
            }
            Err(e) => encode_error_term(env, env_raw, e, iodata),
        };
        trim_buffer(&mut buf);
        result
    })
}

#[inline]
fn encode_error_term<'a>(
    env: Env<'a>,
    env_raw: *mut ErlNifEnv,
    e: EncodeError,
    iodata: bool,
) -> Term<'a> {
    if iodata {
        unsafe {
            Term::new(
                env,
                rustler::sys::enif_raise_exception(env_raw, error_reason(e)),
            )
        }
    } else {
        make_tuple2(env, atoms::error().as_c_arg(), error_reason(e))
    }
}

// Private BEAM stubs: Torque.Native checks representation sizes before these
// entry points, so binary inspection and BigInt decoding cannot copy huge terms.
#[rustler::nif]
fn encode_checked<'a>(env: Env<'a>, term: Term<'a>) -> Term<'a> {
    encode_bounded(env, term, false)
}

#[rustler::nif(schedule = "DirtyCpu")]
fn encode_finish_dirty<'a>(env: Env<'a>, term: Term<'a>, partial: Binary, next: usize) -> Term<'a> {
    encode_resume(env, term, partial.as_slice(), next, false)
}

/// Opt-in dirty variant: output size can't be predicted from the input term
/// without a full traversal, so large encodes are dispatched by the caller.
#[rustler::nif(schedule = "DirtyCpu")]
fn encode_dirty<'a>(env: Env<'a>, term: Term<'a>) -> Term<'a> {
    encode_impl(env, term, false)
}

/// Returns the raw binary on success, raises on error.
/// Skips the {:ok, binary} tuple wrapping for maximum throughput.
#[inline]
fn encode_iodata_impl<'a>(env: Env<'a>, term: Term<'a>, report_timeslice: bool) -> Term<'a> {
    let env_raw = env.as_c_arg();
    ENCODE_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        buf.clear();
        let result = match encode_term::<false>(env, env_raw, term, &mut buf, MAX_DEPTH) {
            Ok(()) => buf_to_binary(env, &buf, report_timeslice),
            Err(e) => unsafe {
                Term::new(
                    env,
                    rustler::sys::enif_raise_exception(env_raw, error_reason(e)),
                )
            },
        };
        trim_buffer(&mut buf);
        result
    })
}

#[rustler::nif]
fn encode_iodata_checked<'a>(env: Env<'a>, term: Term<'a>) -> Term<'a> {
    encode_bounded(env, term, true)
}

#[rustler::nif(schedule = "DirtyCpu")]
fn encode_iodata_finish_dirty<'a>(
    env: Env<'a>,
    term: Term<'a>,
    partial: Binary,
    next: usize,
) -> Term<'a> {
    encode_resume(env, term, partial.as_slice(), next, true)
}

#[rustler::nif(schedule = "DirtyCpu")]
fn encode_iodata_dirty<'a>(env: Env<'a>, term: Term<'a>) -> Term<'a> {
    encode_iodata_impl(env, term, false)
}

#[inline]
fn encode_term<'a, const BOUNDED: bool>(
    env: Env<'a>,
    env_raw: *mut ErlNifEnv,
    term: Term<'a>,
    buf: &mut Vec<u8>,
    depth: u32,
) -> Result<(), EncodeError> {
    match term.get_type() {
        TermType::Map => encode_map::<BOUNDED>(env, env_raw, term, buf, depth),
        TermType::List => encode_list::<BOUNDED>(env, env_raw, term, buf, depth),
        TermType::Binary => encode_binary::<BOUNDED>(env_raw, term, buf),
        TermType::Integer => encode_integer::<BOUNDED>(env_raw, term, buf),
        TermType::Float => encode_float(env_raw, term, buf),
        TermType::Atom => encode_atom(env_raw, term, buf),
        TermType::Tuple => encode_tuple::<BOUNDED>(env, env_raw, term, buf, depth),
        _ => Err(EncodeError::UnsupportedType),
    }
}

fn encode_map<'a, const BOUNDED: bool>(
    env: Env<'a>,
    env_raw: *mut ErlNifEnv,
    term: Term<'a>,
    buf: &mut Vec<u8>,
    depth: u32,
) -> Result<(), EncodeError> {
    if depth == 0 {
        return Err(EncodeError::DepthExceeded);
    }
    let iter = MapEntries::new(env, term).ok_or(EncodeError::UnsupportedType)?;
    buf.push(b'{');
    let mut first = true;
    for (key, value) in iter {
        if !first {
            buf.push(b',');
        }
        first = false;
        check_budget::<BOUNDED>(buf)?;
        encode_map_key::<BOUNDED>(env_raw, key, buf)?;
        buf.push(b':');
        encode_term::<BOUNDED>(env, env_raw, value, buf, depth - 1)?;
    }
    buf.push(b'}');
    Ok(())
}

#[inline]
fn encode_map_key<const BOUNDED: bool>(
    env_raw: *mut ErlNifEnv,
    key: Term,
    buf: &mut Vec<u8>,
) -> Result<(), EncodeError> {
    // `enif_inspect_binary` doubles as the type check for the common binary-key
    // path, avoiding a separate `enif_term_type` call.
    let mut bin = MaybeUninit::<ErlNifBinary>::uninit();
    if unsafe { enif_inspect_binary(env_raw, key.as_c_arg(), bin.as_mut_ptr()) } != 0 {
        let slice = unsafe {
            let bin = bin.assume_init();
            std::slice::from_raw_parts(bin.data, bin.size)
        };
        // Every source byte may become "\\u00XX". Include quotes and leave
        // room for all enclosing containers to close.
        if BOUNDED
            && slice.len()
                > ENCODE_HARD_LIMIT.saturating_sub(buf.len() + 2 + MAX_DEPTH as usize) / 6
        {
            return Err(EncodeError::DirtyRequired);
        }
        return crate::escape::write_json_string(slice, buf).map_err(|_| EncodeError::InvalidUtf8);
    }
    buf.push(b'"');
    match key.get_type() {
        TermType::Atom => {
            write_atom_name(env_raw, key.as_c_arg(), buf)?;
        }
        // Object names must be strings (RFC 8259 §4), so integer keys are
        // stringified rather than rejected, matching Jason. Skips escaping
        // because a decimal integer is only digits and a leading '-'.
        TermType::Integer => encode_integer::<BOUNDED>(env_raw, key, buf)?,
        _ => return Err(EncodeError::InvalidKey),
    }
    buf.push(b'"');
    Ok(())
}

fn encode_list<'a, const BOUNDED: bool>(
    env: Env<'a>,
    env_raw: *mut ErlNifEnv,
    term: Term<'a>,
    buf: &mut Vec<u8>,
    depth: u32,
) -> Result<(), EncodeError> {
    if depth == 0 {
        return Err(EncodeError::DepthExceeded);
    }
    buf.push(b'[');
    let mut first = true;
    let mut current = term.as_c_arg();
    let mut head: ERL_NIF_TERM = 0;
    let mut tail: ERL_NIF_TERM = 0;
    while unsafe { enif_get_list_cell(env_raw, current, &mut head, &mut tail) } != 0 {
        if !first {
            buf.push(b',');
        }
        first = false;
        check_budget::<BOUNDED>(buf)?;
        let item = unsafe { Term::new(env, head) };
        encode_term::<BOUNDED>(env, env_raw, item, buf, depth - 1)?;
        current = tail;
    }
    // Improper list: the loop ends on a non-cons tail, which must be [].
    if unsafe { enif_is_empty_list(env_raw, current) } == 0 {
        return Err(EncodeError::UnsupportedType);
    }
    buf.push(b']');
    Ok(())
}

#[inline]
fn encode_binary<const BOUNDED: bool>(
    env_raw: *mut ErlNifEnv,
    term: Term,
    buf: &mut Vec<u8>,
) -> Result<(), EncodeError> {
    let mut bin = MaybeUninit::<ErlNifBinary>::uninit();
    let slice = unsafe {
        if enif_inspect_binary(env_raw, term.as_c_arg(), bin.as_mut_ptr()) == 0 {
            return Err(EncodeError::UnsupportedType);
        }
        let bin = bin.assume_init();
        std::slice::from_raw_parts(bin.data, bin.size)
    };
    // Reserve worst-case escaped output, quotes and enclosing delimiters,
    // not just source bytes. Inspection itself was bounded on the BEAM.
    if BOUNDED
        && slice.len() > ENCODE_HARD_LIMIT.saturating_sub(buf.len() + 2 + MAX_DEPTH as usize) / 6
    {
        return Err(EncodeError::DirtyRequired);
    }
    crate::escape::write_json_string(slice, buf).map_err(|_| EncodeError::InvalidUtf8)
}

#[inline]
fn encode_integer<const BOUNDED: bool>(
    env_raw: *mut ErlNifEnv,
    term: Term,
    buf: &mut Vec<u8>,
) -> Result<(), EncodeError> {
    let mut n: i64 = 0;
    if unsafe { enif_get_int64(env_raw, term.as_c_arg(), &mut n) } != 0 {
        let mut itoa_buf = itoa::Buffer::new();
        buf.extend_from_slice(itoa_buf.format(n).as_bytes());
        return Ok(());
    }
    // Fallback for u64 range (i64::MAX + 1 ..= u64::MAX)
    let mut u: u64 = 0;
    if unsafe { enif_get_uint64(env_raw, term.as_c_arg(), &mut u) } != 0 {
        let mut itoa_buf = itoa::Buffer::new();
        buf.extend_from_slice(itoa_buf.format(u).as_bytes());
        return Ok(());
    }
    // Arbitrary-precision integer (Erlang bignum) — the term is an integer
    // (encode_term only routes integers here) but doesn't fit i64/u64, so
    // emit its exact decimal form. num-bigint's Display is plain base-10 with
    // a leading '-' for negatives, which is already a valid JSON number.
    if let Ok(big) = term.decode::<rustler::BigInt>() {
        use std::io::Write;
        // Decimal digits are bounded by bits / 3 + 2. Preflight because bignum
        // base-10 conversion is quadratic.

        if BOUNDED {
            let digits = (big.bits() / 3 + 2) as usize;
            if buf.len().saturating_add(digits) > ENCODE_HARD_LIMIT {
                return Err(EncodeError::DirtyRequired);
            }
        }
        let _ = write!(buf, "{}", big);
        return Ok(());
    }
    Err(EncodeError::UnsupportedType)
}

#[inline]
fn encode_float(env_raw: *mut ErlNifEnv, term: Term, buf: &mut Vec<u8>) -> Result<(), EncodeError> {
    let mut n: f64 = 0.0;
    if unsafe { enif_get_double(env_raw, term.as_c_arg(), &mut n) } == 0 {
        return Err(EncodeError::UnsupportedType);
    }
    // JSON has no non-finite numbers, and `format_finite` requires this check.
    if !n.is_finite() {
        return Err(EncodeError::NonFiniteFloat);
    }
    let mut fbuf = zmij::Buffer::new();
    buf.extend_from_slice(fbuf.format_finite(n).as_bytes());
    Ok(())
}

#[inline]
fn encode_atom(env_raw: *mut ErlNifEnv, term: Term, buf: &mut Vec<u8>) -> Result<(), EncodeError> {
    let raw = term.as_c_arg();
    if raw == atoms::r#true().as_c_arg() {
        buf.extend_from_slice(b"true");
    } else if raw == atoms::r#false().as_c_arg() {
        buf.extend_from_slice(b"false");
    } else if raw == atoms::nil().as_c_arg() {
        buf.extend_from_slice(b"null");
    } else {
        buf.push(b'"');
        write_atom_name(env_raw, raw, buf)?;
        buf.push(b'"');
    }
    Ok(())
}

/// Get a raw tuple slice without allocating a Vec.
#[inline]
unsafe fn get_tuple_raw<'a>(
    env_raw: *mut ErlNifEnv,
    term: Term,
) -> Result<&'a [ERL_NIF_TERM], EncodeError> {
    let mut arity: c_int = 0;
    let mut array_ptr = MaybeUninit::uninit();
    if enif_get_tuple(env_raw, term.as_c_arg(), &mut arity, array_ptr.as_mut_ptr()) != 1 {
        return Err(EncodeError::UnsupportedType);
    }
    Ok(std::slice::from_raw_parts(
        array_ptr.assume_init(),
        arity as usize,
    ))
}

fn encode_tuple<'a, const BOUNDED: bool>(
    env: Env<'a>,
    env_raw: *mut ErlNifEnv,
    term: Term<'a>,
    buf: &mut Vec<u8>,
    depth: u32,
) -> Result<(), EncodeError> {
    let elements = unsafe { get_tuple_raw(env_raw, term)? };
    if elements.len() == 1 {
        let inner = unsafe { Term::new(env, elements[0]) };
        if inner.get_type() == TermType::List {
            return encode_proplist::<BOUNDED>(env, env_raw, inner, buf, depth);
        }
    }
    Err(EncodeError::UnsupportedType)
}

fn encode_proplist<'a, const BOUNDED: bool>(
    env: Env<'a>,
    env_raw: *mut ErlNifEnv,
    term: Term<'a>,
    buf: &mut Vec<u8>,
    depth: u32,
) -> Result<(), EncodeError> {
    if depth == 0 {
        return Err(EncodeError::DepthExceeded);
    }
    buf.push(b'{');
    let mut first = true;
    let mut current = term.as_c_arg();
    let mut head: ERL_NIF_TERM = 0;
    let mut tail: ERL_NIF_TERM = 0;
    while unsafe { enif_get_list_cell(env_raw, current, &mut head, &mut tail) } != 0 {
        let pair = unsafe {
            let pair_term = Term::new(env, head);
            get_tuple_raw(env_raw, pair_term).map_err(|_| EncodeError::MalformedProplist)?
        };
        if pair.len() != 2 {
            return Err(EncodeError::MalformedProplist);
        }
        if !first {
            buf.push(b',');
        }
        first = false;
        check_budget::<BOUNDED>(buf)?;
        let key = unsafe { Term::new(env, pair[0]) };
        let val = unsafe { Term::new(env, pair[1]) };
        encode_map_key::<BOUNDED>(env_raw, key, buf)?;
        buf.push(b':');
        encode_term::<BOUNDED>(env, env_raw, val, buf, depth - 1)?;
        current = tail;
    }
    // Improper list: the loop ends on a non-cons tail, which must be [].
    if unsafe { enif_is_empty_list(env_raw, current) } == 0 {
        return Err(EncodeError::MalformedProplist);
    }
    buf.push(b'}');
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{trim_buffer, BUF_RETAIN_CAP};

    #[test]
    fn oversized_scratch_is_released_regardless_of_used_length() {
        for len in [0, BUF_RETAIN_CAP / 2, BUF_RETAIN_CAP + 1] {
            let mut buf = Vec::with_capacity(BUF_RETAIN_CAP * 2);
            buf.resize(len, b'x');

            trim_buffer(&mut buf);

            assert!(buf.is_empty());
            assert!(buf.capacity() <= BUF_RETAIN_CAP);
        }
    }

    #[test]
    fn ordinary_scratch_is_cleared_without_losing_its_allocation() {
        let mut buf = Vec::with_capacity(2048);
        buf.extend_from_slice(b"finished output");
        let allocation = buf.as_ptr();
        let capacity = buf.capacity();

        trim_buffer(&mut buf);

        assert!(buf.is_empty());
        assert_eq!(buf.as_ptr(), allocation);
        assert_eq!(buf.capacity(), capacity);
    }
}
