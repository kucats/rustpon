//! Bounded multipart field semantics for a 64-bit PHP 8.4.27 target.
//! No Zend values, FFI, recursive ownership, or payload copies.
use crate::{Payload, Request, Upload};
use std::io::{self, Write};

const MAX_DEPTH: usize = 64;
const MAX_NAME: usize = 8192;
const TMP: &[u8] = b"[tmp_name]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidLimits,
    Allocation,
    Depth,
    NameBytes,
    Nodes,
    KeyBytes,
    ProtectedNames,
    Work,
    MemoryCapacity,
    MalformedName,
    MalformedUploadName,
    AppendExhausted,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub depth: usize,
    pub name_bytes: usize,
    /// Root included; detached entries remain charged until request disposal.
    pub nodes: usize,
    pub key_bytes: usize,
    pub protected_names: usize,
    /// Conservative byte/entry operation charge, including failed lookups.
    pub work: u64,
    /// Combined request + adapter capacities; excludes allocator/kernel/RSS.
    pub memory_capacity: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            depth: MAX_DEPTH,
            name_bytes: 4096,
            nodes: 4096,
            key_bytes: 64 * 1024,
            protected_names: 256,
            work: 8 * 1024 * 1024,
            memory_capacity: 2 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub nodes: usize,
    pub key_bytes: usize,
    pub protected_names: usize,
    pub fields: usize,
    pub skipped_fields: usize,
    pub work: u64,
    pub memory_capacity: usize,
}

#[derive(Debug, Default, Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
}
impl Span {
    fn bytes(self, bytes: &[u8]) -> &[u8] {
        &bytes[self.start..self.end]
    }
}

#[derive(Debug, Clone, Copy)]
enum Key {
    Integer(i64),
    Bytes(Span),
}
#[derive(Debug, Clone, Copy)]
enum Segment {
    Key(Span),
    Append,
}
#[derive(Debug, Clone, Copy)]
struct Array {
    first: Option<usize>,
    last: Option<usize>,
    next_index: i64,
}
impl Array {
    fn empty() -> Self {
        Self {
            first: None,
            last: None,
            next_index: i64::MIN,
        }
    }
}
#[derive(Debug, Clone, Copy)]
enum Value {
    Bytes(Span),
    Array(Array),
}
#[derive(Debug, Clone, Copy)]
struct Node {
    key: Key,
    value: Value,
    next: Option<usize>,
}

#[derive(Clone, Copy)]
struct Frame {
    next: Option<usize>,
    first: bool,
}

/// Owns the original wire request, including anonymous upload descriptors.
/// Private flat indices prevent cycles and recursive drop. Replacements keep
/// abandoned descendants charged; only successful conversion returns a tree.
#[derive(Debug)]
pub struct PostRequest {
    request: Request,
    nodes: Vec<Node>,
    keys: Vec<u8>,
    protected: Vec<Span>,
    scratch: Vec<u8>,
    segments: [Segment; MAX_DEPTH + 1],
    limits: Limits,
    stats: Stats,
}

fn sum(parts: &[usize]) -> Result<usize, Error> {
    parts
        .iter()
        .try_fold(0usize, |n, p| n.checked_add(*p).ok_or(Error::InvalidLimits))
}

fn reserve<T>(size: usize) -> Result<Vec<T>, Error> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(size)
        .map_err(|_| Error::Allocation)?;
    Ok(buffer)
}

impl Request {
    /// Consumes this request on both success and failure. On adapter failure,
    /// all field storage and upload descriptors are released before returning.
    pub fn into_post(self, limits: Limits) -> Result<PostRequest, Error> {
        PostRequest::build(self, limits)
    }
}

impl PostRequest {
    fn build(request: Request, limits: Limits) -> Result<Self, Error> {
        if limits.depth > MAX_DEPTH || limits.name_bytes > MAX_NAME {
            return Err(Error::InvalidLimits);
        }
        if limits.nodes == 0 {
            return Err(Error::Nodes);
        }
        // Current retained capacities, not the parser's historical object size.
        let wire_capacity = sum(&[
            request.arena.capacity(),
            request
                .parts
                .capacity()
                .checked_mul(size_of::<crate::Part>())
                .ok_or(Error::InvalidLimits)?,
        ])?;
        let scratch_size = limits.name_bytes + TMP.len();
        let requested = sum(&[
            size_of::<Self>(),
            size_of::<[Frame; MAX_DEPTH + 2]>(),
            wire_capacity,
            limits.key_bytes,
            scratch_size,
            limits
                .nodes
                .checked_mul(size_of::<Node>())
                .ok_or(Error::InvalidLimits)?,
            limits
                .protected_names
                .checked_mul(size_of::<Span>())
                .ok_or(Error::InvalidLimits)?,
        ])?;
        if requested > limits.memory_capacity {
            return Err(Error::MemoryCapacity);
        }
        let nodes = reserve::<Node>(limits.nodes)?;
        let keys = reserve::<u8>(limits.key_bytes)?;
        let protected = reserve::<Span>(limits.protected_names)?;
        let scratch = reserve::<u8>(scratch_size)?;
        let actual = sum(&[
            size_of::<Self>(),
            size_of::<[Frame; MAX_DEPTH + 2]>(),
            wire_capacity,
            keys.capacity(),
            scratch.capacity(),
            nodes
                .capacity()
                .checked_mul(size_of::<Node>())
                .ok_or(Error::InvalidLimits)?,
            protected
                .capacity()
                .checked_mul(size_of::<Span>())
                .ok_or(Error::InvalidLimits)?,
        ])?;
        if actual > limits.memory_capacity {
            return Err(Error::MemoryCapacity);
        }
        let mut this = Self {
            request,
            nodes,
            keys,
            protected,
            scratch,
            segments: [Segment::Append; MAX_DEPTH + 1],
            limits,
            stats: Stats {
                memory_capacity: actual,
                ..Stats::default()
            },
        };
        this.nodes.push(Node {
            key: Key::Integer(0),
            value: Value::Array(Array::empty()),
            next: None,
        });
        for index in 0..this.request.parts.len() {
            let part = &this.request.parts[index];
            let name = Span {
                start: part.name.start,
                end: part.name.end,
            };
            let leaf = match &part.payload {
                Payload::Field(range) => Some(Span {
                    start: range.start,
                    end: range.end,
                }),
                Payload::File(_) => None,
            };
            let raw = name.bytes(&this.request.arena);
            this.charge(raw.len() as u64)?;
            let raw = name.bytes(&this.request.arena);
            if leaf.is_none() && !valid_upload_name(raw) {
                return Err(Error::MalformedUploadName);
            }
            let count = normalize(raw, &mut this.scratch, &mut this.segments, limits)?;
            if let Some(value) = leaf {
                this.stats.fields += 1;
                if count == 0 || this.is_protected()? {
                    this.stats.skipped_fields += 1;
                    continue;
                }
                this.insert_path(count, value)?;
            } else if count != 0 {
                this.add_protection()?;
                // Well-formed upload names generate root[tmp_name][indices...].
                // PHP skips malformed upload controls; this adapter rejects the
                // whole conversion instead of publishing a partial projection.
                {
                    let Segment::Key(base) = this.segments[0] else {
                        unreachable!()
                    };
                    let len = this.scratch.len();
                    this.scratch.resize(len + TMP.len(), 0);
                    this.scratch
                        .copy_within(base.end..len, base.end + TMP.len());
                    this.scratch[base.end..base.end + TMP.len()].copy_from_slice(TMP);
                    this.add_protection()?;
                }
            }
        }
        this.stats.nodes = this.nodes.len();
        this.stats.key_bytes = this.keys.len();
        this.stats.protected_names = this.protected.len();
        Ok(this)
    }

    fn charge(&mut self, amount: u64) -> Result<(), Error> {
        self.stats.work = self
            .stats
            .work
            .checked_add(amount)
            .filter(|n| *n <= self.limits.work)
            .ok_or(Error::Work)?;
        Ok(())
    }

    fn is_protected(&mut self) -> Result<bool, Error> {
        for index in 0..self.protected.len() {
            let span = self.protected[index];
            self.charge(1 + self.scratch.len() as u64)?;
            if span.bytes(&self.keys) == self.scratch {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn add_protection(&mut self) -> Result<(), Error> {
        if self.is_protected()? {
            return Ok(());
        }
        if self.protected.len() == self.limits.protected_names {
            return Err(Error::ProtectedNames);
        }
        let end = self
            .keys
            .len()
            .checked_add(self.scratch.len())
            .filter(|n| *n <= self.limits.key_bytes)
            .ok_or(Error::KeyBytes)?;
        let span = Span {
            start: self.keys.len(),
            end,
        };
        self.charge(self.scratch.len() as u64)?;
        self.keys.extend_from_slice(&self.scratch);
        self.protected.push(span);
        Ok(())
    }

    fn insert_path(&mut self, count: usize, value: Span) -> Result<(), Error> {
        let mut array = 0;
        for depth in 0..count {
            self.charge(1)?;
            let segment = self.segments[depth];
            let lookup = match segment {
                Segment::Append => {
                    let Value::Array(state) = self.nodes[array].value else {
                        unreachable!()
                    };
                    let next = if state.next_index == i64::MIN {
                        0
                    } else {
                        state.next_index
                    };
                    Key::Integer(next)
                }
                Segment::Key(span) => {
                    let bytes = span.bytes(&self.scratch);
                    if forbidden(bytes, &self.scratch) {
                        self.stats.skipped_fields += 1;
                        return Ok(());
                    }
                    numeric(bytes).map(Key::Integer).unwrap_or(Key::Bytes(span))
                }
            };
            let existing = self.find(array, lookup)?;
            if existing.is_some() && matches!(segment, Segment::Append) {
                return Err(Error::AppendExhausted);
            }
            let node = if let Some(index) = existing {
                index
            } else {
                self.add_node(array, lookup)?
            };
            if depth + 1 == count {
                self.nodes[node].value = Value::Bytes(value);
            } else {
                if !matches!(self.nodes[node].value, Value::Array(_)) {
                    self.nodes[node].value = Value::Array(Array::empty());
                }
                array = node;
            }
        }
        Ok(())
    }

    fn find(&mut self, array: usize, key: Key) -> Result<Option<usize>, Error> {
        let Value::Array(state) = self.nodes[array].value else {
            unreachable!()
        };
        let mut next = state.first;
        while let Some(index) = next {
            self.charge(1)?;
            let found = match (self.nodes[index].key, key) {
                (Key::Integer(left), Key::Integer(right)) => left == right,
                (Key::Bytes(left), Key::Bytes(right)) => {
                    self.charge(1 + right.bytes(&self.scratch).len() as u64)?;
                    left.bytes(&self.keys) == right.bytes(&self.scratch)
                }
                _ => false,
            };
            if found {
                return Ok(Some(index));
            }
            next = self.nodes[index].next;
        }
        Ok(None)
    }

    fn add_node(&mut self, array: usize, key: Key) -> Result<usize, Error> {
        if self.nodes.len() == self.limits.nodes {
            return Err(Error::Nodes);
        }
        self.charge(1)?;
        let stored_key = if let Key::Bytes(span) = key {
            let bytes = span.bytes(&self.scratch);
            let end = self
                .keys
                .len()
                .checked_add(bytes.len())
                .filter(|n| *n <= self.limits.key_bytes)
                .ok_or(Error::KeyBytes)?;
            let stored = Span {
                start: self.keys.len(),
                end,
            };
            self.charge(bytes.len() as u64)?;
            self.keys.extend_from_slice(span.bytes(&self.scratch));
            Key::Bytes(stored)
        } else {
            key
        };
        let index = self.nodes.len();
        self.nodes.push(Node {
            key: stored_key,
            value: Value::Array(Array::empty()),
            next: None,
        });
        let Value::Array(mut state) = self.nodes[array].value else {
            unreachable!()
        };
        if let Some(last) = state.last {
            self.nodes[last].next = Some(index);
        } else {
            state.first = Some(index);
        }
        state.last = Some(index);
        match key {
            Key::Integer(number) if number >= state.next_index => {
                state.next_index = number.saturating_add(1);
            }
            _ => (),
        }
        self.nodes[array].value = Value::Array(state);
        Ok(index)
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }
    pub fn wire(&self) -> &Request {
        &self.request
    }
    pub fn upload_mut(&mut self, part_index: usize) -> Option<&mut Upload> {
        self.request.upload_mut(part_index)
    }

    /// Tagged, ordered JSON: ["a", [[typed_key, typed_value], ...]].
    /// Byte strings use ["s", "hex"]; integers use ["i", "decimal"].
    /// Bounded explicit traversal; caller output buffering is outside the budget.
    pub fn write_json(&self, output: &mut impl Write) -> io::Result<()> {
        let Value::Array(root) = self.nodes[0].value else {
            unreachable!()
        };
        let mut stack = [Frame {
            next: None,
            first: true,
        }; MAX_DEPTH + 2];
        stack[0].next = root.first;
        let mut depth = 0;
        output.write_all(b"[\"a\",[")?;
        loop {
            let Some(index) = stack[depth].next else {
                output.write_all(b"]]")?;
                if depth == 0 {
                    return Ok(());
                }
                depth -= 1;
                output.write_all(b"]")?;
                continue;
            };
            let node = self.nodes[index];
            stack[depth].next = node.next;
            if !stack[depth].first {
                output.write_all(b",")?;
            }
            stack[depth].first = false;
            output.write_all(b"[")?;
            match node.key {
                Key::Integer(number) => write!(output, "[\"i\",\"{number}\"]")?,
                Key::Bytes(span) => write_hex_value(output, span.bytes(&self.keys))?,
            }
            output.write_all(b",")?;
            match node.value {
                Value::Bytes(span) => {
                    write_hex_value(output, span.bytes(&self.request.arena))?;
                    output.write_all(b"]")?;
                }
                Value::Array(array) => {
                    output.write_all(b"[\"a\",[")?;
                    depth += 1;
                    stack[depth] = Frame {
                        next: array.first,
                        first: true,
                    };
                }
            }
        }
    }
}

fn write_hex_value(output: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    const DIGITS: &[u8] = b"0123456789abcdef";
    output.write_all(b"[\"s\",\"")?;
    for byte in bytes {
        output.write_all(&[DIGITS[(byte >> 4) as usize], DIGITS[(byte & 15) as usize]])?;
    }
    output.write_all(b"\"]")
}

fn forbidden(key: &[u8], original: &[u8]) -> bool {
    [b"__Host-".as_slice(), b"__Secure-".as_slice()]
        .iter()
        .any(|prefix| key.starts_with(prefix) && !original.starts_with(prefix))
}

fn valid_upload_name(name: &[u8]) -> bool {
    let mut brackets = 0usize;
    for (index, byte) in name.iter().enumerate() {
        match byte {
            b'[' => brackets += 1,
            b']' => {
                let Some(next) = brackets.checked_sub(1) else {
                    return false;
                };
                brackets = next;
                if name.get(index + 1).is_some_and(|byte| *byte != b'[') {
                    return false;
                }
            }
            _ => (),
        }
    }
    brackets == 0
}

fn numeric(bytes: &[u8]) -> Option<i64> {
    let digits = bytes.strip_prefix(b"-").unwrap_or(bytes);
    if digits.is_empty()
        || !digits.iter().all(u8::is_ascii_digit)
        || (digits[0] == b'0' && bytes.len() > 1)
    {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse::<i64>().ok()
}

fn normalize(
    raw: &[u8],
    scratch: &mut Vec<u8>,
    path: &mut [Segment; MAX_DEPTH + 1],
    limits: Limits,
) -> Result<usize, Error> {
    if raw.len() > limits.name_bytes {
        return Err(Error::NameBytes);
    }
    scratch.clear();
    let start = raw.iter().position(|b| *b != b' ').unwrap_or(raw.len());
    let raw = &raw[start..];
    let base = raw.iter().position(|b| *b == b'[').unwrap_or(raw.len());
    if base == 0 {
        return Ok(0);
    }
    for &byte in &raw[..base] {
        scratch.push(if byte == b' ' || byte == b'.' {
            b'_'
        } else {
            byte
        });
    }
    path[0] = Segment::Key(Span {
        start: 0,
        end: base,
    });
    let mut count = 1;
    let mut cursor = base;
    while raw.get(cursor) == Some(&b'[') {
        if count > limits.depth {
            return Err(Error::Depth);
        }
        cursor += 1;
        while raw.get(cursor).is_some_and(|b| b" \r\n\t".contains(b)) {
            cursor += 1;
        }
        let end = raw[cursor..]
            .iter()
            .position(|b| *b == b']')
            .map(|n| n + cursor)
            .ok_or(Error::MalformedName)?;
        scratch.push(b'[');
        let start = scratch.len();
        scratch.extend_from_slice(&raw[cursor..end]);
        path[count] = if start == scratch.len() {
            Segment::Append
        } else {
            Segment::Key(Span {
                start,
                end: scratch.len(),
            })
        };
        scratch.push(b']');
        count += 1;
        cursor = end + 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests;
