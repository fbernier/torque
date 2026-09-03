//! Prepared multi-path extraction for Torque.
//!
//! An [`ExtractPlan`] is built once and reused without per-call plan allocation
//! or hashing. Segments can address object keys or array indexes, and results
//! are returned per path. Unselected regions are either fully parsed or skipped
//! structurally according to [`Validate`].
//!
//! Duplicate object keys are last-value-wins unless [`Keys::Unique`] promises
//! uniqueness, in which case the first match stands and unchecked extraction
//! may stop after finding every requested key.
//!
//! # Linting
//!
//! This module is Torque's, not upstream's. `lib.rs` disables every rustc and
//! clippy lint for the vendored crate, which would silently cover this file
//! too — `cargo clippy` on the crate would report nothing about extraction,
//! including its unsafe. These attributes opt the module back in, which is
//! what makes CI's lint step over this manifest mean anything.
#![deny(warnings)]
#![deny(clippy::all)]

use std::sync::Arc;

use ahash::AHashMap;

use sonic_number::ParserNumber;

use crate::{
    error::{ErrorCode, Result},
    parser::Reference,
    parser::{restore_neg_zero, Parser, MAX_PARSE_DEPTH},
    reader::{Read, Reader},
    util::utf8::from_utf8,
    value::shared::Shared,
    JsonInput, JsonValueTrait, Value,
};

/// Compiled pointer segment. Numeric tokens retain object-key and array-index
/// forms. Keys borrow from the compiled path; the plan copies only new keys.
#[derive(Debug, Clone, Copy)]
pub enum Seg<'a> {
    /// Matches an object key only.
    Key(&'a str),
    /// Matches an array index, or the same digits used as an object key.
    Index { idx: usize, key: &'a str },
}

/// Whether the regions no path selects are validated while being skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validate {
    /// Parses skipped regions, matching full-document validation.
    Yes,
    /// Uses structural SIMD skipping. Syntax errors inside unselected regions
    /// are not reported, but consumed UTF-8 and literals remain validated.
    No,
}

/// Whether object keys are promised to be unique.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keys {
    /// A key may repeat, and the last occurrence wins.
    Repeatable,
    /// No key repeats, so the first match stands.
    Unique,
}

#[derive(Debug, Default)]
struct Node {
    /// Object-key edges in insertion order.
    keys: Vec<(String, u32)>,
    /// Array-index edges in insertion order.
    indices: Vec<(usize, u32)>,
    /// Result slots ending at this node.
    slots: Vec<u32>,
    /// Whether a path continues below this node.
    has_children: bool,
}

/// Immutable extraction plan built from an ordered path set.
#[derive(Debug)]
pub struct ExtractPlan {
    nodes: Vec<Node>,
    slots: usize,
    /// Construction-only indexes for wide nodes. Boxed because most nodes
    /// remain unindexed.
    #[allow(clippy::box_collection)]
    index: Vec<Option<Box<AHashMap<String, u32>>>>,
}

/// Child count where plan construction switches from scanning to hashing.
const INDEX_KEYS_ABOVE: usize = 32;

impl Default for ExtractPlan {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtractPlan {
    pub fn new() -> Self {
        Self {
            nodes: vec![Node::default()],
            slots: 0,
            index: vec![None],
        }
    }

    /// Adds a path and assigns its result slot. Empty and duplicate paths retain
    /// distinct slots. Paths beyond the parser nesting limit are not added to
    /// the walk.
    ///
    /// Borrowed segments avoid an intermediate path allocation.
    pub fn add_path<'s>(&mut self, segs: impl ExactSizeIterator<Item = Seg<'s>>) {
        let slot = self.slots as u32;
        self.slots += 1;
        if segs.len() > MAX_PARSE_DEPTH {
            return;
        }

        let mut cur = 0usize;
        for seg in segs {
            self.nodes[cur].has_children = true;
            cur = match seg {
                Seg::Key(k) => self.child_for_key(cur, k),
                Seg::Index { idx, key } => {
                    // Share one child between the key and index interpretations.
                    let child = self.child_for_key(cur, key);
                    if !self.nodes[cur].indices.iter().any(|(i, _)| *i == idx) {
                        self.nodes[cur].indices.push((idx, child as u32));
                    }
                    child
                }
            };
        }
        self.nodes[cur].slots.push(slot);
    }

    /// Drops construction indexes before the plan becomes long-lived.
    pub fn finish(&mut self) {
        self.index = Vec::new();
        self.nodes.shrink_to_fit();
    }

    fn child_for_key(&mut self, at: usize, key: &str) -> usize {
        if let Some(map) = self.index[at].as_ref() {
            if let Some(child) = map.get(key) {
                return *child as usize;
            }
        } else {
            if let Some((_, child)) = self.nodes[at].keys.iter().find(|(k, _)| k == key) {
                return *child as usize;
            }
            if self.nodes[at].keys.len() == INDEX_KEYS_ABOVE {
                // Index existing children once, then hash subsequent additions.
                let map: AHashMap<String, u32> = self.nodes[at]
                    .keys
                    .iter()
                    .map(|(k, c)| (k.clone(), *c))
                    .collect();
                self.index[at] = Some(Box::new(map));
            }
        }

        self.nodes.push(Node::default());
        self.index.push(None);
        let child = self.nodes.len() - 1;
        let owned = key.to_string();
        if let Some(map) = self.index[at].as_mut() {
            map.insert(owned.clone(), child as u32);
        }
        self.nodes[at].keys.push((owned, child as u32));
        child
    }
}

/// Extracted value. Unescaped strings may borrow the input; all other values
/// own their storage through `Value`.
#[derive(Debug, Clone)]
pub enum Extracted<'de> {
    /// Bytes inside the input, valid UTF-8, no escapes.
    Str(&'de str),
    /// Anything else: containers, numbers, literals, and strings that had to be
    /// unescaped into scratch space.
    Value(Value),
}

/// Extracts every planned path in insertion order. Missing paths return `None`.
pub fn extract<'de, Input: JsonInput<'de>>(
    json: Input,
    plan: &ExtractPlan,
    validate: Validate,
    keys: Keys,
) -> Result<Vec<Option<Extracted<'de>>>> {
    let slice = json.to_u8_slice();
    // `Value` stores offsets in `u32`, so enforce the same bound as full parsing.
    if crate::json_too_large(slice.len()) {
        return Err(crate::error::make_error(format!(
            "Only support JSON less than 4 GB, the input JSON is too large here, len is {}",
            slice.len()
        )));
    }
    let reader = Read::new(slice, false);
    let mut parser = Parser::new(reader);

    let mut out: Vec<Option<Extracted<'de>>> = (0..plan.slots).map(|_| None).collect();
    // Escaped strings share this scratch buffer. Documents without escapes do
    // not allocate it.
    let mut strbuf = Vec::new();
    let mut ex = Extractor {
        plan,
        out: &mut out,
        checked: validate == Validate::Yes,
        first_wins: keys == Keys::Unique,
        stamps: Vec::new(),
        generation: 0,
    };
    ex.value(&mut parser, &mut strbuf, 0, 0)?;

    if validate == Validate::Yes {
        // Match full parsing's trailing-content check.
        parser.parse_trailing()?;
    }

    let index = parser.read.index();
    if json.need_utf8_valid() {
        from_utf8(&slice[..index])?;
    }
    Ok(out)
}

/// Plan keys a single object can track in one word.
const INLINE_SEEN: usize = 64;

/// Records which of a plan node's keys the object being walked has already
/// supplied, so a repeated key is recognised at any plan width.
enum Seen {
    /// Bit per plan key, for nodes up to `INLINE_SEEN` keys wide.
    Inline(u64),
    /// Stamp written into `Extractor::stamps` at each child's node index.
    /// Wider nodes cost one scratch allocation for the whole extraction
    /// rather than a bitset per object.
    Stamped(u32),
}

struct Extractor<'p, 'o, 'de> {
    plan: &'p ExtractPlan,
    out: &'o mut Vec<Option<Extracted<'de>>>,
    checked: bool,
    first_wins: bool,
    /// Stamp per plan node, indexed by node id. Empty until a node wider than
    /// `INLINE_SEEN` is entered, so narrow plans never allocate it.
    stamps: Vec<u32>,
    /// Stamp handed to the most recently entered wide object.
    generation: u32,
}

impl<'de> Extractor<'_, '_, 'de> {
    /// Prepares duplicate tracking for an object whose plan node has `keys`
    /// planned keys.
    #[inline]
    fn new_seen(&mut self, keys: usize) -> Seen {
        if keys <= INLINE_SEEN {
            return Seen::Inline(0);
        }
        if self.stamps.is_empty() {
            let nodes = self.plan.nodes.len();
            self.stamps = vec![0; nodes];
        }
        // One stamp per wide object entered, so this is bounded by the
        // document's `{` count and the 4 GB input cap keeps it under u32::MAX.
        self.generation += 1;
        Seen::Stamped(self.generation)
    }

    /// Records plan key `i`, whose child node is `child`, as supplied by the
    /// object being walked. Reports whether it had already been supplied.
    #[inline]
    fn mark(&mut self, seen: &mut Seen, i: usize, child: u32) -> bool {
        match seen {
            Seen::Inline(bits) => {
                let bit = 1u64 << i;
                let already = *bits & bit != 0;
                *bits |= bit;
                already
            }
            Seen::Stamped(generation) => {
                let slot = &mut self.stamps[child as usize];
                let already = *slot == *generation;
                *slot = *generation;
                already
            }
        }
    }

    /// Handles the current parser value for one plan node. `depth` is shared by
    /// planned descent, skipped regions, and selected subtrees so extraction
    /// enforces the full parser's single nesting budget.
    fn value<R: Reader<'de>>(
        &mut self,
        parser: &mut Parser<R>,
        strbuf: &mut Vec<u8>,
        node: u32,
        depth: usize,
    ) -> Result<()> {
        let n = &self.plan.nodes[node as usize];

        // A terminal node needs the whole value. Resolve any longer paths from
        // that value when one requested path prefixes another.
        if !n.slots.is_empty() {
            let value = parse_value_in_place(parser, strbuf, depth)?;
            for slot in n.slots.iter() {
                self.out[*slot as usize] = Some(value.clone());
            }
            // Only owned container values can have planned descendants.
            if n.has_children {
                if let Extracted::Value(v) = &value {
                    self.descend_value(v, node);
                }
            }
            return Ok(());
        }

        match parser.skip_space_peek() {
            Some(b'{') if !n.keys.is_empty() => self.object(parser, strbuf, node, depth),
            Some(b'[') if !n.indices.is_empty() => self.array(parser, strbuf, node, depth),
            Some(_) => {
                parser.skip_one_value_at(self.checked, depth)?;
                Ok(())
            }
            None => Err(parser.error(ErrorCode::EofWhileParsing)),
        }
    }

    fn object<R: Reader<'de>>(
        &mut self,
        parser: &mut Parser<R>,
        strbuf: &mut Vec<u8>,
        node: u32,
        depth: usize,
    ) -> Result<()> {
        if depth >= MAX_PARSE_DEPTH {
            return Err(parser.error(ErrorCode::RecursionLimitExceeded));
        }
        parser.read.eat(1);
        match parser.skip_space() {
            Some(b'"') => {}
            Some(b'}') => return Ok(()),
            _ => return Err(parser.error(ErrorCode::ExpectObjectKeyOrEnd)),
        }

        let wanted = self.plan.nodes[node as usize].keys.len();
        let mut seen = self.new_seen(wanted);
        let mut found = 0usize;
        loop {
            let matched = {
                let key = parser.parse_str(strbuf)?;
                let key: &str = &key;
                self.plan.nodes[node as usize]
                    .keys
                    .iter()
                    .enumerate()
                    .find(|(_, (k, _))| k == key)
                    .map(|(i, (_, child))| (i, *child))
            };
            parser.parse_object_clo()?;

            match matched {
                Some((i, child)) => {
                    let already = self.mark(&mut seen, i, child);
                    if self.first_wins && already {
                        // Under the uniqueness promise, the first value already stands.
                        parser.skip_one_value_at(self.checked, depth + 1)?;
                    } else {
                        if already {
                            // Last-value-wins must clear every result below the
                            // previous value before the replacement is read.
                            self.clear(child);
                        } else {
                            found += 1;
                        }
                        self.value(parser, strbuf, child, depth + 1)?;
                    }
                }
                None => {
                    parser.skip_one_value_at(self.checked, depth + 1)?;
                }
            }

            // With no validation or keys left to find, skip the object remainder.
            if self.first_wins && !self.checked && found == wanted {
                return parser.skip_container(b'{', b'}');
            }

            match parser.skip_space() {
                Some(b',') => match parser.skip_space() {
                    Some(b'"') => continue,
                    _ => return Err(parser.error(ErrorCode::ExpectObjectKeyOrEnd)),
                },
                Some(b'}') => return Ok(()),
                Some(_) => return Err(parser.error(ErrorCode::ExpectedObjectCommaOrEnd)),
                None => return Err(parser.error(ErrorCode::EofWhileParsing)),
            }
        }
    }

    fn array<R: Reader<'de>>(
        &mut self,
        parser: &mut Parser<R>,
        strbuf: &mut Vec<u8>,
        node: u32,
        depth: usize,
    ) -> Result<()> {
        if depth >= MAX_PARSE_DEPTH {
            return Err(parser.error(ErrorCode::RecursionLimitExceeded));
        }
        parser.read.eat(1);
        if let Some(b']') = parser.skip_space_peek() {
            parser.read.eat(1);
            return Ok(());
        }

        let mut at = 0usize;
        loop {
            let child = self.plan.nodes[node as usize]
                .indices
                .iter()
                .find(|(i, _)| *i == at)
                .map(|(_, c)| *c);
            match child {
                Some(child) => self.value(parser, strbuf, child, depth + 1)?,
                None => {
                    parser.skip_one_value_at(self.checked, depth + 1)?;
                }
            }
            at += 1;

            match parser.skip_space() {
                Some(b',') => continue,
                Some(b']') => return Ok(()),
                Some(_) => return Err(parser.error(ErrorCode::ExpectedArrayCommaOrEnd)),
                None => return Err(parser.error(ErrorCode::EofWhileParsing)),
            }
        }
    }

    /// Clears results at and below a repeated object key before its replacement
    /// is read. The walk follows the plan, so cost depends on selected paths,
    /// not document size.
    fn clear(&mut self, node: u32) {
        let n = &self.plan.nodes[node as usize];
        for slot in n.slots.iter() {
            self.out[*slot as usize] = None;
        }
        // Index edges alias key edges; walking both would revisit each subtree.
        debug_assert!(n
            .indices
            .iter()
            .all(|(_, i)| n.keys.iter().any(|(_, k)| k == i)));
        for (_, child) in n.keys.iter() {
            self.clear(*child);
        }
    }

    /// Resolves descendants from an already-built value when one path prefixes another.
    fn descend_value(&mut self, value: &Value, node: u32) {
        let n = &self.plan.nodes[node as usize];
        for (key, child) in n.keys.iter() {
            let hit = if self.first_wins {
                value.get(key.as_str())
            } else {
                last_key(value, key)
            };
            if let Some(v) = hit {
                self.fill(v, *child);
            }
        }
        for (idx, child) in n.indices.iter() {
            if let Some(v) = value.get(*idx) {
                self.fill(v, *child);
            }
        }
    }

    fn fill(&mut self, value: &Value, node: u32) {
        let n = &self.plan.nodes[node as usize];
        for slot in n.slots.iter() {
            self.out[*slot as usize] = Some(Extracted::Value(value.clone()));
        }
        if n.has_children {
            self.descend_value(value, node);
        }
    }
}

/// Last matching object value, preserving the document walk's duplicate rule.
fn last_key<'v>(value: &'v Value, key: &str) -> Option<&'v Value> {
    match value.as_pair_slice() {
        Some(pairs) => pairs
            .iter()
            .rfind(|(k, _)| k.as_node_str() == Some(key))
            .map(|(_, v)| v),
        // Hash-map-backed values cannot contain duplicate keys.
        None => value.get(key),
    }
}

/// Parses the current value in place. Scalars are built directly, unescaped
/// strings borrow the input, and only containers need an arena.
fn parse_value_in_place<'de, R: Reader<'de>>(
    parser: &mut Parser<R>,
    strbuf: &mut Vec<u8>,
    depth: usize,
) -> Result<Extracted<'de>> {
    match parser.skip_space_peek() {
        Some(b'{') | Some(b'[') => {
            let mut shared = Arc::new(Shared::default());
            // Expose the arena provenance before packing its node into `Value`.
            let smut: &mut Shared = unsafe { &mut *(Arc::as_ptr(&shared) as *mut _) };
            let mut parsed = Value::new();
            parsed.parse_without_padding(smut, strbuf, parser, depth)?;
            let _ = Arc::get_mut(&mut shared);
            Ok(Extracted::Value(parsed))
        }
        Some(b'"') => {
            parser.read.eat(1);
            match parser.parse_str(strbuf)? {
                // Borrow unescaped input; copy parser scratch before it is reused.
                Reference::Borrowed(s) => Ok(Extracted::Str(s)),
                Reference::Copied(s) => Ok(Extracted::Value(Value::copy_str(s))),
            }
        }
        Some(c @ b'-') | Some(c @ b'0'..=b'9') => {
            let start = parser.read.index();
            parser.read.eat(1);
            match parser.parse_number(c)? {
                ParserNumber::Unsigned(u) => Ok(Extracted::Value(Value::new_u64(u))),
                ParserNumber::Signed(i) => Ok(Extracted::Value(Value::new_i64(i))),
                ParserNumber::Float(f) => {
                    // Preserve negative zero across every decode path.
                    let token = parser.read.slice_unchecked(start, parser.read.index());
                    Value::new_f64(restore_neg_zero(f, token))
                        .map(Extracted::Value)
                        .ok_or_else(|| parser.error(ErrorCode::InvalidNumber))
                }
            }
        }
        Some(_) => {
            let (slice, _) = parser.skip_one(true)?;
            match slice {
                b"true" => Ok(Extracted::Value(Value::new_bool(true))),
                b"false" => Ok(Extracted::Value(Value::new_bool(false))),
                b"null" => Ok(Extracted::Value(Value::new_null())),
                _ => Err(parser.error(ErrorCode::InvalidJsonValue)),
            }
        }
        None => Err(parser.error(ErrorCode::EofWhileParsing)),
    }
}
