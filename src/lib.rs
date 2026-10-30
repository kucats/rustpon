//! Strict multipart wire parsing, with request-owned staging and no PHP calls.
//!
//! Only `Parser::finish` publishes a request. A failed feed drops every staged
//! upload and cannot be resumed. The caller must signal actual transport EOF;
//! a closing multipart delimiter alone does not finish the request.
#![forbid(unsafe_code)]
#![cfg(unix)]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::ops::Range;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const BLOCK: usize = 8192;
// A misbehaving Read implementation must not spin forever on Interrupted.
// Reset the quota after any successful nonempty read.
pub(crate) const MAX_CONSECUTIVE_INTERRUPTS: usize = 1024;
const MAX_BOUNDARY: usize = 70;
const WINDOW: usize = MAX_BOUNDARY + 6;
static NEXT_UPLOAD: AtomicU64 = AtomicU64::new(0);

pub mod admission;
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
    // No descriptor exists when PHP-mode staging cannot create a temporary file.
    file: Option<File>,
    length: u64,
    discarded: bool,
    // Opt-in PHP EOF recovery only; partial uploads must never be copied.
    partial: bool,
    // A recoverable PHP-mode staging write failure; never copy this upload.
    write_failed: bool,
    // Missing staging directory in opt-in PHP mode (UPLOAD_ERR_NO_TMP_DIR=6).
    no_tmp_dir: bool,
}

// Special mode bits are not meaningful for anonymous upload payloads and
// can indicate an injected inode. Keep the rule shared by staging and copy.
fn private_staging_mode(metadata: &fs::Metadata) -> bool {
    metadata.permissions().mode() & 0o7077 == 0
}

// The source is checked before copying and after each emitted block.
// A failed check requires the caller to discard destination bytes.
fn validate_staged_copy_source(file: &File, expected_length: u64) -> io::Result<fs::Metadata> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 0
        || !private_staging_mode(&metadata)
        || metadata.len() != expected_length
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(metadata)
}

// Compare against the initial inode snapshot after every emitted block, not
// only after the final block. This bounds further output following observable
// source tampering; it is not an atomic snapshot or a rollback mechanism.
fn validate_staged_copy_snapshot(
    file: &File,
    expected_length: u64,
    baseline: &fs::Metadata,
) -> io::Result<()> {
    let current = validate_staged_copy_source(file, expected_length)?;
    if baseline.dev() != current.dev()
        || baseline.ino() != current.ino()
        || baseline.uid() != current.uid()
        || baseline.gid() != current.gid()
        || baseline.permissions().mode() != current.permissions().mode()
        || baseline.mtime() != current.mtime()
        || baseline.mtime_nsec() != current.mtime_nsec()
        || baseline.ctime() != current.ctime()
        || baseline.ctime_nsec() != current.ctime_nsec()
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(())
}

// Re-read the just-emitted block from the staged inode. Metadata timestamps
// can be coarse or restored, so compare actual bytes as a second, bounded
// integrity signal. This is not an atomic snapshot: callers still must discard
// the destination on any error.
fn validate_staged_copy_bytes(file: &File, offset: u64, expected: &[u8]) -> io::Result<()> {
    debug_assert!(expected.len() <= BLOCK);
    let mut actual = [0_u8; BLOCK];
    let mut received = 0;
    let mut interrupts = 0;
    while received < expected.len() {
        match file.read_at(
            &mut actual[received..expected.len()],
            offset + received as u64,
        ) {
            Ok(0) => return Err(io::ErrorKind::InvalidData.into()),
            Ok(count) => {
                received += count;
                interrupts = 0;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                interrupts += 1;
                if interrupts >= MAX_CONSECUTIVE_INTERRUPTS {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
    if &actual[..expected.len()] != expected {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(())
}

impl Upload {
    pub fn len(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Copies exactly the staged length from offset zero with bounded memory
    /// and bounded consecutive Interrupted retries on both I/O directions.
    /// A failed copy may have written a prefix: callers must discard that
    /// destination. The descriptor stays owned by this request, so a retry
    /// can start at offset zero. Reads use explicit offsets so another handle
    /// sharing the same open-file description cannot redirect the copy by
    /// seeking. No client path is exposed.
    pub fn copy_to(&mut self, output: &mut impl Write) -> io::Result<u64> {
        self.copy_to_with_read_hook(output, || {})
    }

    // The hook is a no-op in production. Tests use it to inject an inode
    // mutation deterministically *after* pread and *before* the first write.
    fn copy_to_with_read_hook(
        &mut self,
        output: &mut impl Write,
        mut after_read: impl FnMut(),
    ) -> io::Result<u64> {
        // Preserve the received length for PHP error=1 metadata, but never
        // expose a partial prefix from a discarded upload.
        if self.discarded {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let file = self.file.as_mut().ok_or(io::ErrorKind::InvalidData)?;
        let expected_length = self.length;
        // A staged upload must still be anonymous, private, regular, and
        // complete when the caller copies it. A descriptor can outlive its
        // original directory and can be modified by local code holding it.
        // Reject tampering before emitting any bytes. This is a point-in-time
        // check, not a substitute for descriptor-relative upload creation.
        let source_before = validate_staged_copy_source(file, self.length)?;
        // File::try_clone shares the kernel file offset with the original.
        // pread-style reads avoid both that shared offset and interference
        // from a destination writer that seeks through a cloned descriptor.
        let mut offset = 0_u64;
        let mut buffer = [0_u8; BLOCK];
        let mut read_interrupts = 0;
        while offset < self.length {
            // This is at most BLOCK, so the cast is safe on all Unix targets.
            let capacity = (self.length - offset).min(BLOCK as u64) as usize;
            let count = match file.read_at(&mut buffer[..capacity], offset) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(count) => {
                    read_interrupts = 0;
                    count
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    read_interrupts += 1;
                    if read_interrupts >= MAX_CONSECUTIVE_INTERRUPTS {
                        return Err(error);
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
            // A source can change while pread fills the buffer, before the
            // destination has received anything. Reject observable mutations
            // before emitting the just-read block. This is still not an
            // atomic snapshot of the inode.
            after_read();
            validate_staged_copy_snapshot(file, expected_length, &source_before)?;
            // A short-writing destination may mutate the staged inode after
            // emitting only a few bytes. Revalidate after *each* successful
            // write, before offering it the remainder of this read block.
            write_chunk_checked(output, &buffer[..count], || {
                validate_staged_copy_snapshot(file, expected_length, &source_before)
            })?;
            // Read back the completed block before transferring the next one.
            // This catches persistent byte drift even when metadata alone
            // cannot distinguish the two inode contents.
            validate_staged_copy_bytes(file, offset, &buffer[..count])?;
            validate_staged_copy_snapshot(file, expected_length, &source_before)?;
            offset += count as u64;
        }
        // Also validate zero-byte uploads, for which the loop does not run.
        validate_staged_copy_snapshot(file, self.length, &source_before)?;
        Ok(self.length)
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
    // Detect spool directory replacement between construction and each upload.
    spool_device: u64,
    spool_inode: u64,
    php_upload_max_bytes: Option<u64>,
    php_recover_write_errors: bool,
    #[cfg(test)]
    fail_create: bool,
    #[cfg(test)]
    fail_no_tmp_dir: bool,
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
        // Pin an absolute, resolved path before accepting request bytes.
        // Otherwise a relative path can move with cwd, or a symlink in an
        // ancestor can be retargeted between parser creation and upload.
        // Preserve the final-component symlink rejection above, and detect
        // replacements racing with canonicalization by comparing identities.
        let canonical_spool = fs::canonicalize(spool_dir)?;
        let resolved = fs::symlink_metadata(&canonical_spool)?;
        if !resolved.is_dir()
            || resolved.permissions().mode() & 0o077 != 0
            || resolved.dev() != metadata.dev()
            || resolved.ino() != metadata.ino()
        {
            return Err(Error::InvalidSpoolDirectory);
        }
        let path_size = canonical_spool
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
        spool_path.push(&canonical_spool);
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
            spool_device: metadata.dev(),
            spool_inode: metadata.ino(),
            php_upload_max_bytes: None,
            php_recover_write_errors: false,
            #[cfg(test)]
            fail_create: false,
            #[cfg(test)]
            fail_no_tmp_dir: false,
            #[cfg(test)]
            fail_write: false,
        })
    }

    /// Opt-in PHP upload_max_filesize quota enforced while consuming bytes.
    /// Oversized uploads are drained without further disk writes; any staged
    /// prefix is truncated, and subsequent uploads continue. None and zero
    /// disable this quota. Independent wire limits remain fail-closed.
    pub fn with_php_upload_max_bytes(mut self, max: Option<u64>) -> Self {
        self.php_upload_max_bytes = max.filter(|value| *value != 0);
        self
    }

    /// Opt-in PHP upload error=7 recovery. On a staging write failure, the
    /// affected file is truncated and marked uncopiable, while later parts
    /// continue. Strict Parser::new callers remain fail-closed. A failure to
    /// truncate the anonymous staging file still aborts the whole request.
    pub fn with_php_upload_write_recovery(mut self) -> Self {
        self.php_recover_write_errors = true;
        self
    }

    // Recheck before each upload. A missing directory is handled by the
    // existing PHP error=6 policy; a replaced or newly public directory must
    // never receive staged bytes. This reduces, but cannot eliminate, the
    // check/open TOCTOU window without a directory-relative descriptor API.
    fn validate_spool_directory(&self) -> Result<(), Error> {
        match fs::symlink_metadata(&self.spool_path) {
            Ok(metadata)
                if metadata.is_dir()
                    && metadata.permissions().mode() & 0o077 == 0
                    && metadata.dev() == self.spool_device
                    && metadata.ino() == self.spool_inode =>
            {
                Ok(())
            }
            Ok(_) => Err(Error::InvalidSpoolDirectory),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
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

    /// Opt-in PHP 8.4 EOF recovery for complete transport bodies with
    /// incomplete multipart parts. Unfinished fields publish their received
    /// bytes; unfinished uploads receive error=3 and lose staged bytes.
    /// Strict finish() remains transactional.
    pub fn finish_with_php_partial(mut self) -> Result<Request, Error> {
        match self.state {
            State::End(0 | 2) => Ok(self.request.take().expect("live completed request")),
            State::Body
                if matches!(
                    self.active.as_ref().map(|part| &part.payload),
                    Some(Payload::File(_))
                ) =>
            {
                // Pending delimiter-lookahead bytes are still body bytes at
                // EOF. Account for hard quotas without staging more data.
                if let Some(Part {
                    payload: Payload::File(upload),
                    ..
                }) = self.active.as_mut()
                {
                    if let Some(file) = upload.file.as_mut() {
                        file.set_len(0)?;
                    }
                    upload.discarded = true;
                    upload.partial = true;
                }
                self.output_len = 0;
                while self.window_len > 0 {
                    let byte = self.window[self.window_start];
                    self.window_start = (self.window_start + 1) % WINDOW;
                    self.window_len -= 1;
                    self.emit(byte)?;
                }
                self.output_len = 0;
                let part = self.active.take().expect("active partial file");
                self.request().parts.push(part);
                Ok(self.request.take().expect("live partial request"))
            }
            State::Body
                if matches!(
                    self.active.as_ref().map(|part| &part.payload),
                    Some(Payload::Field(_))
                ) =>
            {
                // The delimiter matcher keeps up to one marker and suffix
                // byte in lookahead. At EOF, PHP publishes the unfinished
                // field, excluding a trailing CRLF or partial delimiter.
                // Preserve hard field quotas while draining this lookahead.
                let mut pending = [0_u8; WINDOW];
                for (index, slot) in pending.iter_mut().take(self.window_len).enumerate() {
                    *slot = self.window[(self.window_start + index) % WINDOW];
                }
                let mut retained = self.window_len;
                for start in 0..self.window_len {
                    let suffix = &pending[start..self.window_len];
                    let matched = suffix.len().min(self.marker_len);
                    if matched >= 2 && suffix[..matched] == self.marker[..matched] {
                        retained = start;
                        break;
                    }
                }
                self.window_len = 0;
                for &byte in &pending[..retained] {
                    self.emit(byte)?;
                }
                self.flush_output()?;
                let part = self.active.take().expect("active partial field");
                self.request().parts.push(part);
                Ok(self.request.take().expect("live partial request"))
            }
            // PHP 8.4 registers a partial upload once a complete
            // Content-Disposition line with a filename has been received,
            // even if the blank line terminating the headers never arrives.
            // Do not interpret an unterminated header line as complete.
            State::Headers
                if self.line_len == 0
                    && self.headers.name.is_some()
                    && self.headers.filename.is_some() =>
            {
                self.start_body()?;
                self.finish_with_php_partial()
            }
            State::Headers
                if self.line_len == 0
                    && self.headers.name.is_some()
                    && self.headers.filename.is_none() =>
            {
                // A complete field disposition can publish an empty value
                // even when the terminating blank header line is missing.
                self.start_body()?;
                self.finish_with_php_partial()
            }
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
                    true,
                )?);
                if let Some(filename) = filename {
                    self.headers.filename = Some(store_param(
                        &mut request.arena,
                        filename,
                        self.limits.filename_bytes,
                        Limit::FilenameBytes,
                        self.limits.retained_bytes,
                        false,
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
            self.validate_spool_directory()?;
            #[cfg(test)]
            if self.fail_create {
                return Err(Error::Io(io::ErrorKind::PermissionDenied));
            }
            #[cfg(test)]
            let opened = if self.fail_no_tmp_dir {
                Err(Error::Io(io::ErrorKind::NotFound))
            } else {
                anonymous_upload(&mut self.spool_path, self.spool_device, self.spool_inode)
            };
            #[cfg(not(test))]
            let opened =
                anonymous_upload(&mut self.spool_path, self.spool_device, self.spool_inode);
            // PHP reports error=6 when the temporary directory disappears.
            // Other create/unlink errors remain fatal rather than misreported.
            let (file, no_tmp_dir) = match opened {
                Ok(file) => (Some(file), false),
                Err(Error::Io(io::ErrorKind::NotFound)) if self.php_recover_write_errors => {
                    // An unlink race can also return NotFound after creation.
                    // Recover only if the spool directory itself is missing.
                    #[cfg(test)]
                    let injected = self.fail_no_tmp_dir;
                    #[cfg(not(test))]
                    let injected = false;
                    if !injected
                        && !fs::symlink_metadata(&self.spool_path)
                            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
                    {
                        return Err(Error::Io(io::ErrorKind::NotFound));
                    }
                    (None, true)
                }
                Err(error) => return Err(error),
            };
            Payload::File(Upload {
                file,
                length: 0,
                discarded: no_tmp_dir,
                partial: false,
                write_failed: false,
                no_tmp_dir,
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
        if is_file {
            // Logical byte accounting continues even after PHP rejects the
            // upload, so error=1 and the independent wire quotas remain exact.
            let over_php_quota = self
                .php_upload_max_bytes
                .is_some_and(|max| self.part_bytes > max);
            let upload = match &mut self.active.as_mut().expect("active file").payload {
                Payload::File(upload) => upload,
                Payload::Field(_) => unreachable!("file branch"),
            };
            upload.length = self.part_bytes;
            if over_php_quota {
                if !upload.discarded {
                    // Discard any buffered bytes and reclaim earlier chunks.
                    if let Some(file) = upload.file.as_mut() {
                        file.set_len(0)?;
                    }
                    upload.discarded = true;
                }
                self.output_len = 0;
                return Ok(());
            }
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
                if !upload.discarded {
                    #[cfg(test)]
                    let write_result = if self.fail_write {
                        Err(io::Error::from(io::ErrorKind::StorageFull))
                    } else {
                        write_chunk(
                            upload.file.as_mut().expect("live staged upload"),
                            &self.output[..self.output_len],
                        )
                    };
                    #[cfg(not(test))]
                    let write_result = write_chunk(
                        upload.file.as_mut().expect("live staged upload"),
                        &self.output[..self.output_len],
                    );
                    if let Err(error) = write_result {
                        if !self.php_recover_write_errors {
                            return Err(error.into());
                        }
                        // Never publish a prefix from a failed write. Keep
                        // consuming the current part and subsequent parts.
                        // If cleanup itself fails, fail closed as before.
                        upload
                            .file
                            .as_mut()
                            .expect("live staged upload")
                            .set_len(0)?;
                        upload.discarded = true;
                        upload.write_failed = true;
                    }
                }
            }
        }
        self.output_len = 0;
        Ok(())
    }
}

// Unlike Write::write_all, do not retry an indefinitely Interrupted writer.
// Any successful write resets the consecutive-interrupt budget.
fn write_chunk(writer: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    write_chunk_checked(writer, bytes, || Ok(()))
}

// Check the source after each positive write, not just after a complete
// BLOCK-sized read. This also covers partial writes and retries. The caller
// must discard already-emitted destination bytes if the check fails.
fn write_chunk_checked(
    writer: &mut impl Write,
    mut bytes: &[u8],
    mut check: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let mut consecutive_interrupts = 0;
    while !bytes.is_empty() {
        match writer.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            // A buggy/custom Write must not panic by overreporting progress.
            // Fail closed even though std::io::Write forbids this behavior.
            Ok(count) if count > bytes.len() => return Err(io::ErrorKind::InvalidData.into()),
            Ok(count) => {
                bytes = &bytes[count..];
                consecutive_interrupts = 0;
                check()?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                consecutive_interrupts += 1;
                if consecutive_interrupts >= MAX_CONSECUTIVE_INTERRUPTS {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
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
    unescape_quoted: bool,
) -> Result<Range<usize>, Error> {
    let start = arena.len();
    let mut escaped = false;
    let mut count = 0;
    for &byte in param.bytes {
        if unescape_quoted && param.quoted && byte == b'\\' && !escaped {
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
        // PHP 8.4 exposes the media type before the first semicolon in
        // `$_FILES["..."]["type"]`, including spaces preceding that
        // semicolon. Parameters are client-controlled and not interpreted.
        let media = value.split(|b| *b == b';').next().unwrap_or(value);
        let validated = trim(media);
        let slash = validated
            .iter()
            .position(|b| *b == b'/')
            .ok_or(Error::Malformed)?;
        if slash == 0
            || slash + 1 == validated.len()
            || !validated[..slash].iter().copied().all(token)
            || !validated[slash + 1..].iter().copied().all(token)
        {
            return Err(Error::Unsupported);
        }
        if validated[..slash].eq_ignore_ascii_case(b"multipart") {
            return Err(Error::Unsupported);
        }
        return Ok(Header::ContentType(media));
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

// Staged data must not be written to an inode that another local account
// can read, or into a private directory owned by a different account. The
// directory owner is compared with the *opened* inode's owner so a privileged
// caller cannot accidentally stage into an untrusted user's 0700 directory.
// A restrictive umask may remove owner bits; only group/other access matters.
fn private_staging_inode(metadata: &fs::Metadata, directory_owner: Option<u32>) -> bool {
    metadata.is_file() && directory_owner == Some(metadata.uid()) && private_staging_mode(metadata)
}

/// Filename creation is independent of client metadata. create_new prevents
/// following an existing path; unlink happens before any payload is written.
/// NEXT_UPLOAD is a name counter only, never a pointer to request state.
fn anonymous_upload(path: &mut PathBuf, device: u64, inode: u64) -> Result<File, Error> {
    anonymous_upload_with_hook(path, device, inode, || {})
}

// The hook is a no-op outside tests. It makes a path replacement immediately
// after create_new deterministic instead of relying on a timing-sensitive race.
fn anonymous_upload_with_hook(
    path: &mut PathBuf,
    device: u64,
    inode: u64,
    after_create: impl FnMut(),
) -> Result<File, Error> {
    anonymous_upload_with_hooks(path, device, inode, after_create, || {})
}

// Keep a separate post-unlink injection point to test directory replacement
// after the last pathname operation but before any request bytes are written.
fn anonymous_upload_with_hooks(
    path: &mut PathBuf,
    device: u64,
    inode: u64,
    mut after_create: impl FnMut(),
    mut after_unlink: impl FnMut(),
) -> Result<File, Error> {
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
            Ok(file) => {
                after_create();
                // The directory may have changed between Parser's preflight
                // and create_new. Before unlinking or writing any client data,
                // verify both the parent identity and the created file inode.
                // A mismatch fails closed; never unlink a different file.
                let parent = path.parent().ok_or(Error::InvalidSpoolDirectory)?;
                let directory = fs::symlink_metadata(parent);
                let directory_owner = directory.as_ref().ok().map(MetadataExt::uid);
                let entry = fs::symlink_metadata(&*path);
                let opened = file.metadata();
                let valid_directory = matches!(
                    directory,
                    Ok(ref metadata)
                        if metadata.is_dir()
                            && metadata.permissions().mode() & 0o077 == 0
                            && metadata.dev() == device
                            && metadata.ino() == inode
                );
                let valid_entry = matches!(
                    (entry, opened),
                    (Ok(entry), Ok(opened))
                        if entry.is_file()
                            && entry.dev() == opened.dev()
                            && entry.ino() == opened.ino()
                            && private_staging_inode(&opened, directory_owner)
                );
                if !valid_directory || !valid_entry {
                    // A concurrent rename may leave an empty named file in
                    // the original directory. It contains no request bytes.
                    Err(Error::InvalidSpoolDirectory)
                } else {
                    // Do not retry an unlink error: the pathname may now
                    // refer to a different file after an adversarial swap.
                    fs::remove_file(&*path).map_err(Error::from)?;
                    after_unlink();
                    // The trusted directory may be swapped or made public
                    // after the pre-unlink identity check. Recheck before
                    // returning a writable descriptor. This narrows the
                    // pathname race window but does not replace openat/unlinkat.
                    let directory = fs::symlink_metadata(parent);
                    let directory_owner = directory.as_ref().ok().map(MetadataExt::uid);
                    let valid_directory = matches!(
                        directory,
                        Ok(ref metadata)
                            if metadata.is_dir()
                                && metadata.permissions().mode() & 0o077 == 0
                                && metadata.dev() == device
                                && metadata.ino() == inode
                    );
                    // A rename/hard-link race could leave the opened inode
                    // reachable under another name. Never write upload bytes
                    // unless the descriptor is genuinely anonymous.
                    let opened = file.metadata()?;
                    if !valid_directory
                        || !private_staging_inode(&opened, directory_owner)
                        || opened.nlink() != 0
                    {
                        Err(Error::InvalidSpoolDirectory)
                    } else {
                        Ok(file)
                    }
                }
            }
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
    // The generic reader retains strict transactional I/O semantics.
    parse_reader_with_php_limits_impl(
        reader,
        boundary,
        limits,
        spool_dir,
        None,
        None,
        ReaderMode::Strict,
    )
}

/// Standalone PHP upload_max_filesize policy applied during parsing.
/// This does not register native PHP uploads or replace an SAPI handler.
pub fn parse_reader_with_php_upload_max_bytes(
    reader: &mut impl Read,
    boundary: &[u8],
    limits: Limits,
    spool_dir: &Path,
    php_max_upload_bytes: Option<u64>,
) -> Result<Request, Error> {
    parse_reader_with_php_limits(
        reader,
        boundary,
        limits,
        spool_dir,
        php_max_upload_bytes,
        None,
    )
}

/// Opt-in streaming post_max_size fallback for callers without a trusted
/// Content-Length preflight. Once the body exceeds the nonzero PHP quota,
/// discard all staged uploads and drain the remaining input without parsing
/// or writing it to disk. Return an empty wire request with the actual byte
/// count, so admission can publish empty PHP POST/FILES metadata.
///
/// The independent hard body limit and Read errors still fail closed, even
/// after the PHP quota is exceeded. Unlike an SAPI Content-Length preflight,
/// malformed framing encountered *before* the threshold still fails closed.
/// This function does not handle HTTP framing or replace an SAPI handler.
pub fn parse_reader_with_php_limits(
    reader: &mut impl Read,
    boundary: &[u8],
    limits: Limits,
    spool_dir: &Path,
    php_max_upload_bytes: Option<u64>,
    php_max_post_bytes: Option<u64>,
) -> Result<Request, Error> {
    parse_reader_with_php_limits_impl(
        reader,
        boundary,
        limits,
        spool_dir,
        php_max_upload_bytes,
        php_max_post_bytes,
        ReaderMode::Php,
    )
}

/// Opt-in PHP EOF compatibility for a complete HTTP body with an incomplete
/// multipart file. A short read against trusted Content-Length is not eligible.
pub fn parse_reader_with_php_partial(
    reader: &mut impl Read,
    boundary: &[u8],
    limits: Limits,
    spool_dir: &Path,
    php_max_upload_bytes: Option<u64>,
    php_max_post_bytes: Option<u64>,
) -> Result<Request, Error> {
    parse_reader_with_php_limits_impl(
        reader,
        boundary,
        limits,
        spool_dir,
        php_max_upload_bytes,
        php_max_post_bytes,
        ReaderMode::PhpPartial,
    )
}

#[derive(Clone, Copy)]
enum ReaderMode {
    Strict,
    Php,
    PhpPartial,
}

fn parse_reader_with_php_limits_impl(
    reader: &mut impl Read,
    boundary: &[u8],
    limits: Limits,
    spool_dir: &Path,
    php_max_upload_bytes: Option<u64>,
    php_max_post_bytes: Option<u64>,
    mode: ReaderMode,
) -> Result<Request, Error> {
    let mut configured =
        Parser::new(boundary, limits, spool_dir)?.with_php_upload_max_bytes(php_max_upload_bytes);
    if !matches!(mode, ReaderMode::Strict) {
        configured = configured.with_php_upload_write_recovery();
    }
    let mut parser = Some(configured);
    let mut body_bytes = 0u64;
    let mut buffer = [0; BLOCK];
    let mut consecutive_interrupts = 0usize;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                return match parser {
                    Some(parser) if matches!(mode, ReaderMode::PhpPartial) => {
                        parser.finish_with_php_partial()
                    }
                    Some(parser) => parser.finish(),
                    None => Ok(Request {
                        arena: Vec::new(),
                        parts: Vec::new(),
                        stats: Stats {
                            body_bytes,
                            ..Stats::default()
                        },
                    }),
                };
            }
            // A broken Read implementation must not cause an out-of-bounds
            // slice or bypass the hard quota after PHP has stopped parsing.
            Ok(count) if count > buffer.len() => {
                return Err(Error::Io(io::ErrorKind::InvalidData));
            }
            Ok(count) => {
                consecutive_interrupts = 0;
                body_bytes = add_u64(
                    body_bytes,
                    count as u64,
                    limits.body_bytes,
                    Limit::BodyBytes,
                )?;
                if php_max_post_bytes.is_some_and(|max| max != 0 && body_bytes > max) {
                    // Dropping the parser closes completed and active upload
                    // descriptors immediately; the rest is read-only draining.
                    parser = None;
                } else if let Some(parser) = parser.as_mut() {
                    parser.feed(&buffer[..count])?;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                consecutive_interrupts += 1;
                if consecutive_interrupts >= MAX_CONSECUTIVE_INTERRUPTS {
                    return Err(Error::Io(io::ErrorKind::Interrupted));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/**
 * Parse exactly one HTTP body with a trusted Content-Length supplied by the
 * transport. This does not parse HTTP headers or implement an SAPI.
 *
 * If Content-Length exceeds a nonzero PHP post_max_size, do not instantiate
 * the multipart parser or stage uploads. Drain exactly the declared bytes,
 * returning an empty request for PHP-compatible POST/FILES publication.
 * The independent hard body quota is checked before reading any bytes.
 *
 * An early EOF is always an error. Bytes after Content-Length belong to the
 * next transport message and are left unread. Callers must reject ambiguous
 * or untrusted HTTP framing before invoking this API.
 */
pub fn parse_reader_with_php_content_length(
    reader: &mut impl Read,
    boundary: &[u8],
    limits: Limits,
    spool_dir: &Path,
    php_max_upload_bytes: Option<u64>,
    php_max_post_bytes: Option<u64>,
    content_length: u64,
) -> Result<Request, Error> {
    if content_length > limits.body_bytes {
        return Err(Error::Limit(Limit::BodyBytes));
    }
    // A declared empty body has no multipart framing to validate. Like PHP
    // SAPI, publish empty POST/FILES without opening any upload descriptor.
    if content_length == 0 {
        return Ok(Request {
            arena: Vec::new(),
            parts: Vec::new(),
            stats: Stats::default(),
        });
    }
    let mut bounded = reader.take(content_length);
    if php_max_post_bytes.is_some_and(|max| max != 0 && content_length > max) {
        // Preflight before parsing even the first byte: malformed multipart
        // input cannot cause upload staging or a parser error in this branch.
        let mut buffer = [0u8; BLOCK];
        let mut consecutive_interrupts = 0usize;
        loop {
            match bounded.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) if count > buffer.len() => {
                    return Err(Error::Io(io::ErrorKind::InvalidData));
                }
                Ok(_) => consecutive_interrupts = 0,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    consecutive_interrupts += 1;
                    if consecutive_interrupts >= MAX_CONSECUTIVE_INTERRUPTS {
                        return Err(Error::Io(io::ErrorKind::Interrupted));
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        if bounded.limit() != 0 {
            return Err(Error::Truncated);
        }
        return Ok(Request {
            arena: Vec::new(),
            parts: Vec::new(),
            stats: Stats {
                body_bytes: content_length,
                ..Stats::default()
            },
        });
    }
    let request = parse_reader_with_php_limits(
        &mut bounded,
        boundary,
        limits,
        spool_dir,
        php_max_upload_bytes,
        php_max_post_bytes,
    )?;
    if bounded.limit() != 0 {
        return Err(Error::Truncated);
    }
    Ok(request)
}

#[cfg(test)]
mod tests;
