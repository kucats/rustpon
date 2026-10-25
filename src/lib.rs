//! Strict multipart wire parsing, with request-owned staging and no PHP calls.
//!
//! Only `Parser::finish` publishes a request. A failed feed drops every staged
//! upload and cannot be resumed. The caller must signal actual transport EOF;
//! a closing multipart delimiter alone does not finish the request.
#![forbid(unsafe_code)]
#![cfg(unix)]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const BLOCK: usize = 8192;
const MAX_BOUNDARY: usize = 70;
const WINDOW: usize = MAX_BOUNDARY + 6;
static NEXT_UPLOAD: AtomicU64 = AtomicU64::new(0);

pub mod files;
pub mod post;
pub mod urlencoded;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    BodyBytes,
    Parts,
    Fields,
    Files,
    HeaderLineBytes,
    HeadersPerPart,
    HeadersTotal,
    HeaderBytesPerPart,
    HeaderBytesTotal,
    NameBytes,
    FilenameBytes,
    FieldBytes,
    FieldBytesTotal,
    FileBytes,
    FileBytesTotal,
    RetainedBytes,
    MemoryCapacity,
    BoundaryComparisons,
}

/// Fixed-size errors: no attacker strings or formatted diagnostics are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidBoundary,
    InvalidLimits,
    InvalidSpoolDirectory,
    Malformed,
    Unsupported,
    Truncated,
    Cancelled,
    Allocation,
    Limit(Limit),
    Io(io::ErrorKind),
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub body_bytes: u64,
    pub parts: usize,
    pub fields: usize,
    pub files: usize,
    pub header_line_bytes: usize,
    pub headers_per_part: usize,
    pub headers_total: usize,
    pub header_bytes_per_part: usize,
    pub header_bytes_total: usize,
    pub name_bytes: usize,
    pub filename_bytes: usize,
    pub field_bytes: usize,
    pub field_bytes_total: usize,
    pub file_bytes: u64,
    pub file_bytes_total: u64,
    /// Maximum byte comparisons performed by the multipart delimiter matcher.
    /// This is a CPU-work quota, independent of the input byte budget.
    pub boundary_comparisons: u64,
    /// Preallocated arena capacity for names, content types and field values.
    pub retained_bytes: usize,
    /// Parser object + arena/part/path capacities. Excludes allocator/kernel
    /// overhead, caller buffers and the later PHP adapter; this is not RSS.
    pub memory_capacity: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            body_bytes: 66 * 1024 * 1024,
            parts: 128,
            fields: 128,
            files: 16,
            header_line_bytes: 4096,
            headers_per_part: 32,
            headers_total: 256,
            header_bytes_per_part: 16 * 1024,
            header_bytes_total: 64 * 1024,
            name_bytes: 1024,
            filename_bytes: 1024,
            field_bytes: 64 * 1024,
            field_bytes_total: 256 * 1024,
            file_bytes: 32 * 1024 * 1024,
            file_bytes_total: 64 * 1024 * 1024,
            boundary_comparisons: 16 * 66 * 1024 * 1024,
            retained_bytes: 1024 * 1024,
            memory_capacity: 2 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub body_bytes: u64,
    pub header_bytes: usize,
    pub headers: usize,
    pub parts: usize,
    pub fields: usize,
    pub files: usize,
    pub field_bytes: usize,
    pub file_bytes: u64,
    pub memory_capacity: usize,
    pub boundary_comparisons: u64,
}

#[derive(Debug)]
pub struct Upload {
    file: File,
    length: u64,
}

impl Upload {
    pub fn len(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Copies from offset zero, bounded by the already enforced upload quota.
    /// The descriptor stays owned by this request. No client path is exposed.
    pub fn copy_to(&mut self, output: &mut impl Write) -> io::Result<u64> {
        self.file.seek(SeekFrom::Start(0))?;
        io::copy(&mut self.file, output)
    }
}

#[derive(Debug)]
enum Payload {
    Field(Range<usize>),
    File(Upload),
}

#[derive(Debug)]
struct Part {
    name: Range<usize>,
    filename: Option<Range<usize>>,
    content_type: Option<Range<usize>>,
    payload: Payload,
}

/// Ordered wire IR. Names are raw bytes, with no PHP normalization/expansion.
#[derive(Debug)]
pub struct Request {
    arena: Vec<u8>,
    parts: Vec<Part>,
    stats: Stats,
}

#[derive(Debug, PartialEq, Eq)]
pub struct PartView<'a> {
    pub name: &'a [u8],
    pub filename: Option<&'a [u8]>,
    pub content_type: Option<&'a [u8]>,
    pub value: Option<&'a [u8]>,
    pub file_bytes: Option<u64>,
}

impl Request {
    pub fn parts(&self) -> impl ExactSizeIterator<Item = PartView<'_>> {
        self.parts.iter().map(|part| PartView {
            name: &self.arena[part.name.clone()],
            filename: part.filename.as_ref().map(|r| &self.arena[r.clone()]),
            content_type: part.content_type.as_ref().map(|r| &self.arena[r.clone()]),
            value: match &part.payload {
                Payload::Field(r) => Some(&self.arena[r.clone()]),
                Payload::File(_) => None,
            },
            file_bytes: match &part.payload {
                Payload::File(upload) => Some(upload.length),
                Payload::Field(_) => None,
            },
        })
    }

    pub fn upload_mut(&mut self, part_index: usize) -> Option<&mut Upload> {
        match &mut self.parts.get_mut(part_index)?.payload {
            Payload::File(upload) => Some(upload),
            Payload::Field(_) => None,
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Start(usize),
    InitialSuffix(Option<u8>),
    Headers,
    Body,
    End(usize),
    Failed(Error),
}

#[derive(Debug, Default)]
struct Headers {
    name: Option<Range<usize>>,
    filename: Option<Range<usize>>,
    content_type: Option<Range<usize>>,
    count: usize,
    bytes: usize,
}

/// Caller supplies an existing private spool directory (mode 0700), which
/// must remain under trusted control throughout parsing.
pub struct Parser {
    limits: Limits,
    request: Option<Request>,
    state: State,
    marker: [u8; MAX_BOUNDARY + 4],
    marker_len: usize,
    line: [u8; BLOCK],
    line_len: usize,
    headers: Headers,
    active: Option<Part>,
    window: [u8; WINDOW],
    window_start: usize,
    window_len: usize,
    output: [u8; BLOCK],
    output_len: usize,
    part_bytes: u64,
    spool_path: PathBuf,
    #[cfg(test)]
    fail_create: bool,
    #[cfg(test)]
    fail_write: bool,
}

fn add_usize(value: usize, amount: usize, max: usize, limit: Limit) -> Result<usize, Error> {
    value
        .checked_add(amount)
        .filter(|n| *n <= max)
        .ok_or(Error::Limit(limit))
}

fn add_u64(value: u64, amount: u64, max: u64, limit: Limit) -> Result<u64, Error> {
    value
        .checked_add(amount)
        .filter(|n| *n <= max)
        .ok_or(Error::Limit(limit))
}

impl Parser {
    pub fn new(boundary: &[u8], limits: Limits, spool_dir: &Path) -> Result<Self, Error> {
        // Deliberately narrower than MIME bchars: no spaces or quoted boundary.
        if boundary.is_empty()
            || boundary.len() > MAX_BOUNDARY
            || !boundary
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || b"'()+_,-./:=?".contains(b))
        {
            return Err(Error::InvalidBoundary);
        }
        if limits.header_line_bytes < 2 || limits.header_line_bytes > BLOCK {
            return Err(Error::InvalidLimits);
        }
        let metadata = fs::symlink_metadata(spool_dir)?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::InvalidSpoolDirectory);
        }
        let path_size = spool_dir
            .as_os_str()
            .len()
            .checked_add(80)
            .ok_or(Error::InvalidLimits)?;
        let requested = limits
            .parts
            .checked_mul(size_of::<Part>())
            .and_then(|n| n.checked_add(limits.retained_bytes))
            .and_then(|n| n.checked_add(path_size))
            .and_then(|n| n.checked_add(size_of::<Self>()))
            .ok_or(Error::InvalidLimits)?;
        if requested > limits.memory_capacity {
            return Err(Error::Limit(Limit::MemoryCapacity));
        }
        let mut arena = Vec::new();
        arena
            .try_reserve_exact(limits.retained_bytes)
            .map_err(|_| Error::Allocation)?;
        let mut parts = Vec::new();
        parts
            .try_reserve_exact(limits.parts)
            .map_err(|_| Error::Allocation)?;
        let mut spool_path = PathBuf::new();
        spool_path
            .try_reserve_exact(path_size)
            .map_err(|_| Error::Allocation)?;
        spool_path.push(spool_dir);
        let actual = size_of::<Self>()
            .checked_add(arena.capacity())
            .and_then(|n| {
                parts
                    .capacity()
                    .checked_mul(size_of::<Part>())
                    .and_then(|p| n.checked_add(p))
            })
            .and_then(|n| n.checked_add(spool_path.capacity()))
            .ok_or(Error::InvalidLimits)?;
        if actual > limits.memory_capacity {
            return Err(Error::Limit(Limit::MemoryCapacity));
        }
        let mut marker = [0; MAX_BOUNDARY + 4];
        marker[..4].copy_from_slice(b"\r\n--");
        marker[4..4 + boundary.len()].copy_from_slice(boundary);
        Ok(Self {
            limits,
            request: Some(Request {
                arena,
                parts,
                stats: Stats {
                    memory_capacity: actual,
                    ..Stats::default()
                },
            }),
            state: State::Start(0),
            marker,
            marker_len: 4 + boundary.len(),
            line: [0; BLOCK],
            line_len: 0,
            headers: Headers::default(),
            active: None,
            window: [0; WINDOW],
            window_start: 0,
            window_len: 0,
            output: [0; BLOCK],
            output_len: 0,
            part_bytes: 0,
            spool_path,
            #[cfg(test)]
            fail_create: false,
            #[cfg(test)]
            fail_write: false,
        })
    }

    fn request(&mut self) -> &mut Request {
        self.request.as_mut().expect("internal live request")
    }

    fn fail(&mut self, error: Error) -> Error {
        self.active.take();
        self.request.take();
        self.state = State::Failed(error);
        error
    }

    pub fn cancel(&mut self) {
        self.fail(Error::Cancelled);
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if let State::Failed(error) = self.state {
            return Err(error);
        }
        match self.feed_inner(bytes) {
            Ok(()) => Ok(()),
            Err(error) => Err(self.fail(error)),
        }
    }

    fn feed_inner(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let total = add_u64(
            self.request().stats.body_bytes,
            bytes.len() as u64,
            self.limits.body_bytes,
            Limit::BodyBytes,
        )?;
        self.request().stats.body_bytes = total;
        for &byte in bytes {
            match self.state {
                State::Start(index) => {
                    if byte != self.marker[index + 2] {
                        return Err(Error::Malformed);
                    }
                    self.state = if index + 1 == self.marker_len - 2 {
                        State::InitialSuffix(None)
                    } else {
                        State::Start(index + 1)
                    };
                }
                State::InitialSuffix(None) => self.state = State::InitialSuffix(Some(byte)),
                State::InitialSuffix(Some(first)) => match [first, byte] {
                    [b'\r', b'\n'] => self.start_headers()?,
                    [b'-', b'-'] => self.state = State::End(0),
                    _ => return Err(Error::Malformed),
                },
                State::Headers => self.header_byte(byte)?,
                State::Body => self.body_byte(byte)?,
                State::End(0) if byte == b'\r' => self.state = State::End(1),
                State::End(1) if byte == b'\n' => self.state = State::End(2),
                State::End(_) => return Err(Error::Malformed),
                State::Failed(_) => unreachable!("feed rejects a failed parser"),
            }
        }
        Ok(())
    }

    /// Call only at transport EOF; dropping the parser is the abort path.
    pub fn finish(mut self) -> Result<Request, Error> {
        match self.state {
            State::End(0 | 2) => Ok(self.request.take().expect("live completed request")),
            State::Failed(error) => Err(error),
            _ => Err(Error::Truncated),
        }
    }

    fn start_headers(&mut self) -> Result<(), Error> {
        let count = add_usize(
            self.request().stats.parts,
            1,
            self.limits.parts,
            Limit::Parts,
        )?;
        self.request().stats.parts = count;
        self.headers = Headers::default();
        self.line_len = 0;
        self.state = State::Headers;
        Ok(())
    }

    fn header_byte(&mut self, byte: u8) -> Result<(), Error> {
        self.headers.bytes = add_usize(
            self.headers.bytes,
            1,
            self.limits.header_bytes_per_part,
            Limit::HeaderBytesPerPart,
        )?;
        let total = add_usize(
            self.request().stats.header_bytes,
            1,
            self.limits.header_bytes_total,
            Limit::HeaderBytesTotal,
        )?;
        self.request().stats.header_bytes = total;
        let next = add_usize(
            self.line_len,
            1,
            self.limits.header_line_bytes,
            Limit::HeaderLineBytes,
        )?;
        self.line[self.line_len] = byte;
        self.line_len = next;
        if byte != b'\n' {
            return Ok(());
        }
        if self.line_len < 2 || self.line[self.line_len - 2] != b'\r' {
            return Err(Error::Malformed);
        }
        if self.line_len == 2 {
            return self.start_body();
        }
        self.headers.count = add_usize(
            self.headers.count,
            1,
            self.limits.headers_per_part,
            Limit::HeadersPerPart,
        )?;
        let count = add_usize(
            self.request().stats.headers,
            1,
            self.limits.headers_total,
            Limit::HeadersTotal,
        )?;
        self.request().stats.headers = count;
        let header = parse_header(&self.line[..self.line_len - 2])?;
        let request = self.request.as_mut().expect("live headers");
        match header {
            Header::Disposition { name, filename } => {
                if self.headers.name.is_some() {
                    return Err(Error::Malformed);
                }
                self.headers.name = Some(store_param(
                    &mut request.arena,
                    name,
                    self.limits.name_bytes,
                    Limit::NameBytes,
                    self.limits.retained_bytes,
                )?);
                if let Some(filename) = filename {
                    self.headers.filename = Some(store_param(
                        &mut request.arena,
                        filename,
                        self.limits.filename_bytes,
                        Limit::FilenameBytes,
                        self.limits.retained_bytes,
                    )?);
                }
            }
            Header::ContentType(value) => {
                if self.headers.content_type.is_some() {
                    return Err(Error::Malformed);
                }
                self.headers.content_type = Some(store(
                    &mut request.arena,
                    value,
                    self.limits.retained_bytes,
                )?);
            }
            Header::Other => (),
        }
        self.line_len = 0;
        Ok(())
    }

    fn start_body(&mut self) -> Result<(), Error> {
        let name = self.headers.name.take().ok_or(Error::Malformed)?;
        let filename = self.headers.filename.take();
        let payload = if filename.is_some() {
            let files = add_usize(
                self.request().stats.files,
                1,
                self.limits.files,
                Limit::Files,
            )?;
            self.request().stats.files = files;
            #[cfg(test)]
            if self.fail_create {
                return Err(Error::Io(io::ErrorKind::PermissionDenied));
            }
            Payload::File(Upload {
                file: anonymous_upload(&mut self.spool_path)?,
                length: 0,
            })
        } else {
            let fields = add_usize(
                self.request().stats.fields,
                1,
                self.limits.fields,
                Limit::Fields,
            )?;
            self.request().stats.fields = fields;
            let start = self.request().arena.len();
            Payload::Field(start..start)
        };
        self.active = Some(Part {
            name,
            filename,
            content_type: self.headers.content_type.take(),
            payload,
        });
        self.part_bytes = 0;
        self.output_len = 0;
        self.window_start = 0;
        self.window_len = 0;
        self.state = State::Body;
        Ok(())
    }

    fn body_byte(&mut self, byte: u8) -> Result<(), Error> {
        let target = self.marker_len + 2;
        self.window[(self.window_start + self.window_len) % WINDOW] = byte;
        self.window_len += 1;
        if self.window_len < target {
            return Ok(());
        }
        let mut delimiter = true;
        for index in 0..self.marker_len {
            let comparisons = add_u64(
                self.request().stats.boundary_comparisons,
                1,
                self.limits.boundary_comparisons,
                Limit::BoundaryComparisons,
            )?;
            self.request().stats.boundary_comparisons = comparisons;
            if self.window[(self.window_start + index) % WINDOW] != self.marker[index] {
                delimiter = false;
                break;
            }
        }
        let suffix = [
            self.window[(self.window_start + self.marker_len) % WINDOW],
            self.window[(self.window_start + self.marker_len + 1) % WINDOW],
        ];
        if delimiter && (suffix == *b"\r\n" || suffix == *b"--") {
            self.flush_output()?;
            let part = self.active.take().expect("active body");
            // Entire part table was reserved by new; never grow past the quota.
            self.request().parts.push(part);
            self.window_len = 0;
            if suffix == *b"--" {
                self.state = State::End(0);
            } else {
                self.start_headers()?;
            }
        } else {
            let output = self.window[self.window_start];
            self.window_start = (self.window_start + 1) % WINDOW;
            self.window_len -= 1;
            self.emit(output)?;
        }
        Ok(())
    }

    fn emit(&mut self, byte: u8) -> Result<(), Error> {
        let is_file = matches!(
            self.active.as_ref().expect("active payload").payload,
            Payload::File(_)
        );
        if is_file {
            self.part_bytes =
                add_u64(self.part_bytes, 1, self.limits.file_bytes, Limit::FileBytes)?;
            let total = add_u64(
                self.request().stats.file_bytes,
                1,
                self.limits.file_bytes_total,
                Limit::FileBytesTotal,
            )?;
            self.request().stats.file_bytes = total;
        } else {
            self.part_bytes = add_u64(
                self.part_bytes,
                1,
                self.limits.field_bytes as u64,
                Limit::FieldBytes,
            )?;
            let total = add_usize(
                self.request().stats.field_bytes,
                1,
                self.limits.field_bytes_total,
                Limit::FieldBytesTotal,
            )?;
            self.request().stats.field_bytes = total;
            // Charge pending field bytes before buffering; their later append
            // cannot allocate or exceed the retained arena budget.
            add_usize(
                self.request().arena.len(),
                self.output_len + 1,
                self.limits.retained_bytes,
                Limit::RetainedBytes,
            )?;
        }
        self.output[self.output_len] = byte;
        self.output_len += 1;
        if self.output_len == BLOCK {
            self.flush_output()?;
        }
        Ok(())
    }

    fn flush_output(&mut self) -> Result<(), Error> {
        if self.output_len == 0 {
            return Ok(());
        }
        let part = self.active.as_mut().expect("active flush");
        match &mut part.payload {
            Payload::Field(range) => {
                let arena = &mut self.request.as_mut().expect("live field").arena;
                store(
                    arena,
                    &self.output[..self.output_len],
                    self.limits.retained_bytes,
                )?;
                range.end = arena.len();
            }
            Payload::File(upload) => {
                #[cfg(test)]
                if self.fail_write {
                    return Err(Error::Io(io::ErrorKind::StorageFull));
                }
                write_chunk(&mut upload.file, &self.output[..self.output_len])?;
                upload.length += self.output_len as u64;
            }
        }
        self.output_len = 0;
        Ok(())
    }
}

fn write_chunk(writer: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    writer.write_all(bytes)
}

fn store(arena: &mut Vec<u8>, bytes: &[u8], max: usize) -> Result<Range<usize>, Error> {
    let start = arena.len();
    let end = add_usize(start, bytes.len(), max, Limit::RetainedBytes)?;
    // new() reserved the entire allowed capacity; all appends are checked.
    arena.extend_from_slice(bytes);
    Ok(start..end)
}

#[derive(Clone, Copy)]
struct Param<'a> {
    bytes: &'a [u8],
    quoted: bool,
}

fn store_param(
    arena: &mut Vec<u8>,
    param: Param<'_>,
    max: usize,
    limit: Limit,
    retained: usize,
) -> Result<Range<usize>, Error> {
    let start = arena.len();
    let mut escaped = false;
    let mut count = 0;
    for &byte in param.bytes {
        if param.quoted && byte == b'\\' && !escaped {
            escaped = true;
            continue;
        }
        count = add_usize(count, 1, max, limit)?;
        add_usize(arena.len(), 1, retained, Limit::RetainedBytes)?;
        arena.push(byte);
        escaped = false;
    }
    Ok(start..arena.len())
}

enum Header<'a> {
    Disposition {
        name: Param<'a>,
        filename: Option<Param<'a>>,
    },
    ContentType(&'a [u8]),
    Other,
}

fn token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn trim(mut bytes: &[u8]) -> &[u8] {
    while matches!(bytes.first(), Some(b' ' | b'\t')) {
        bytes = &bytes[1..];
    }
    while matches!(bytes.last(), Some(b' ' | b'\t')) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn parse_header(line: &[u8]) -> Result<Header<'_>, Error> {
    if line.iter().any(|b| (*b < 32 && *b != b'\t') || *b == 127) {
        return Err(Error::Malformed);
    }
    let colon = line
        .iter()
        .position(|b| *b == b':')
        .ok_or(Error::Malformed)?;
    let key = &line[..colon];
    if key.is_empty() || !key.iter().copied().all(token) {
        return Err(Error::Malformed);
    }
    let value = trim(&line[colon + 1..]);
    if key.eq_ignore_ascii_case(b"content-disposition") {
        return parse_disposition(value);
    }
    if key.eq_ignore_ascii_case(b"content-transfer-encoding") {
        return Err(Error::Unsupported);
    }
    if key.eq_ignore_ascii_case(b"content-type") {
        // Parameters/charset conversion are deliberately outside this subset.
        let slash = value
            .iter()
            .position(|b| *b == b'/')
            .ok_or(Error::Malformed)?;
        if slash == 0
            || slash + 1 == value.len()
            || !value[..slash].iter().copied().all(token)
            || !value[slash + 1..].iter().copied().all(token)
        {
            return Err(Error::Unsupported);
        }
        if value[..slash].eq_ignore_ascii_case(b"multipart") {
            return Err(Error::Unsupported);
        }
        return Ok(Header::ContentType(value));
    }
    Ok(Header::Other)
}

fn parse_disposition(mut value: &[u8]) -> Result<Header<'_>, Error> {
    let end = value.iter().position(|b| *b == b';').unwrap_or(value.len());
    if !trim(&value[..end]).eq_ignore_ascii_case(b"form-data") {
        return Err(Error::Unsupported);
    }
    value = &value[end..];
    let mut name = None;
    let mut filename = None;
    while !value.is_empty() {
        if value[0] != b';' {
            return Err(Error::Malformed);
        }
        value = trim(&value[1..]);
        let end = value.iter().position(|b| !token(*b)).unwrap_or(value.len());
        if end == 0 {
            return Err(Error::Malformed);
        }
        let key = &value[..end];
        value = trim(&value[end..]);
        if value.first() != Some(&b'=') {
            return Err(Error::Malformed);
        }
        value = trim(&value[1..]);
        let quoted = value.first() == Some(&b'"');
        let bytes;
        if quoted {
            value = &value[1..];
            let mut index = 0;
            loop {
                match value.get(index) {
                    None => return Err(Error::Malformed),
                    Some(b'"') => break,
                    Some(b'\\') => {
                        index += 1;
                        if value.get(index).is_none() {
                            return Err(Error::Malformed);
                        }
                    }
                    _ => (),
                }
                index += 1;
            }
            bytes = &value[..index];
            value = trim(&value[index + 1..]);
        } else {
            let end = value.iter().position(|b| !token(*b)).unwrap_or(value.len());
            if end == 0 {
                return Err(Error::Malformed);
            }
            bytes = &value[..end];
            value = trim(&value[end..]);
        }
        let param = Param { bytes, quoted };
        if key.eq_ignore_ascii_case(b"name") {
            if name.replace(param).is_some() {
                return Err(Error::Malformed);
            }
        } else if key.eq_ignore_ascii_case(b"filename") {
            if filename.replace(param).is_some() {
                return Err(Error::Malformed);
            }
        } else {
            return Err(Error::Unsupported);
        }
    }
    let name = name.ok_or(Error::Malformed)?;
    if name.bytes.is_empty() {
        return Err(Error::Malformed);
    }
    Ok(Header::Disposition { name, filename })
}

/// Filename creation is independent of client metadata. create_new prevents
/// following an existing path; unlink happens before any payload is written.
/// NEXT_UPLOAD is a name counter only, never a pointer to request state.
fn anonymous_upload(path: &mut PathBuf) -> Result<File, Error> {
    use std::fmt::Write as _;
    struct Name {
        bytes: [u8; 80],
        len: usize,
    }
    impl std::fmt::Write for Name {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            let end = self
                .len
                .checked_add(text.len())
                .filter(|n| *n <= self.bytes.len())
                .ok_or(std::fmt::Error)?;
            self.bytes[self.len..end].copy_from_slice(text.as_bytes());
            self.len = end;
            Ok(())
        }
    }
    for _ in 0..64 {
        let mut name = Name {
            bytes: [0; 80],
            len: 0,
        };
        write!(
            &mut name,
            "php-request-{:x}-{:x}.tmp",
            std::process::id(),
            NEXT_UPLOAD.fetch_add(1, Ordering::Relaxed)
        )
        .map_err(|_| Error::InvalidLimits)?;
        path.push(std::str::from_utf8(&name.bytes[..name.len]).expect("ASCII temporary name"));
        let opened = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&*path);
        let result = match opened {
            Ok(file) => match fs::remove_file(&*path) {
                Ok(()) => Ok(file),
                Err(error) => {
                    drop(file);
                    let _ = fs::remove_file(&*path);
                    Err(Error::from(error))
                }
            },
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                path.pop();
                continue;
            }
            Err(error) => Err(Error::from(error)),
        };
        path.pop();
        return result;
    }
    Err(Error::Io(io::ErrorKind::AlreadyExists))
}

/// Convenience for standalone callers. Reader errors drop all staged resources.
pub fn parse_reader(
    reader: &mut impl Read,
    boundary: &[u8],
    limits: Limits,
    spool_dir: &Path,
) -> Result<Request, Error> {
    let mut parser = Parser::new(boundary, limits, spool_dir)?;
    let mut buffer = [0; BLOCK];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return parser.finish(),
            Ok(count) => parser.feed(&buffer[..count])?,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests;
