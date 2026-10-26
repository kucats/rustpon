//! Local stdin harness. No listener or network access.
use rustpon::post;
use rustpon::{
    Limits, parse_reader_with_php_content_length, parse_reader_with_php_limits,
    parse_reader_with_php_partial,
};
use std::io::{self, Write};
use std::path::Path;

struct HexWriter<W>(W);
impl<W: Write> Write for HexWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        for byte in bytes {
            self.0
                .write_all(&[DIGITS[(byte >> 4) as usize], DIGITS[(byte & 15) as usize]])?;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    let mode = args.get(1).and_then(|arg| arg.to_str());
    let post_mode = mode == Some("--post");
    let files_mode = mode == Some("--files");
    let admit_mode = mode == Some("--admit");
    let mut first = if post_mode || files_mode || admit_mode {
        2
    } else {
        1
    };
    let mut max_file_uploads = None;
    let mut max_upload_bytes = None;
    let mut max_post_bytes = None;
    let mut content_length = None;
    let mut php_partial_upload = false;
    // The two independent PHP quotas may be specified together, in either
    // order. Reject duplicates rather than silently overriding server policy.
    if files_mode || admit_mode {
        while let Some(flag) = args.get(first).and_then(|arg| arg.to_str()) {
            match flag {
                "--php-max-file-uploads" => {
                    if max_file_uploads.is_some() {
                        return Err("duplicate --php-max-file-uploads".into());
                    }
                    let value = args
                        .get(first + 1)
                        .and_then(|arg| arg.to_str())
                        .ok_or("missing max_file_uploads value")?;
                    max_file_uploads = Some(value.parse::<usize>()?);
                    first += 2;
                }
                "--php-post-max-size" if admit_mode => {
                    if max_post_bytes.is_some() {
                        return Err("duplicate --php-post-max-size".into());
                    }
                    let value = args
                        .get(first + 1)
                        .and_then(|arg| arg.to_str())
                        .ok_or("missing post_max_size value")?;
                    max_post_bytes = Some(value.parse::<u64>()?);
                    first += 2;
                }
                "--content-length" if admit_mode => {
                    if content_length.is_some() {
                        return Err("duplicate --content-length".into());
                    }
                    let value = args
                        .get(first + 1)
                        .and_then(|arg| arg.to_str())
                        .ok_or("missing Content-Length value")?;
                    content_length = Some(value.parse::<u64>()?);
                    first += 2;
                }
                "--php-partial-upload" if admit_mode => {
                    if php_partial_upload {
                        return Err("duplicate --php-partial-upload".into());
                    }
                    php_partial_upload = true;
                    first += 1;
                }
                "--php-upload-max-filesize" => {
                    if max_upload_bytes.is_some() {
                        return Err("duplicate --php-upload-max-filesize".into());
                    }
                    let value = args
                        .get(first + 1)
                        .and_then(|arg| arg.to_str())
                        .ok_or("missing upload_max_filesize value")?;
                    max_upload_bytes = Some(value.parse::<u64>()?);
                    first += 2;
                }
                _ => break,
            }
        }
    }
    if args.len() != first + 2 {
        eprintln!(
            "usage: multipart [--post|--files|--admit] [--php-max-file-uploads N] [--php-upload-max-filesize BYTES] [--php-post-max-size BYTES (admit only)] [--content-length BYTES (admit only)] [--php-partial-upload (admit only)] BOUNDARY PRIVATE_SPOOL_DIR < body.bin"
        );
        std::process::exit(2);
    }
    if php_partial_upload && content_length.is_some() {
        return Err("--php-partial-upload cannot be used with --content-length".into());
    }
    let boundary = args[first].to_str().ok_or("boundary must be ASCII")?;
    let mut stdin = io::stdin().lock();
    let parsed = if let Some(declared) = content_length {
        parse_reader_with_php_content_length(
            &mut stdin,
            boundary.as_bytes(),
            Limits::default(),
            Path::new(&args[first + 1]),
            max_upload_bytes,
            max_post_bytes,
            declared,
        )
    } else if php_partial_upload {
        parse_reader_with_php_partial(
            &mut stdin,
            boundary.as_bytes(),
            Limits::default(),
            Path::new(&args[first + 1]),
            max_upload_bytes,
            max_post_bytes,
        )
    } else {
        parse_reader_with_php_limits(
            &mut stdin,
            boundary.as_bytes(),
            Limits::default(),
            Path::new(&args[first + 1]),
            max_upload_bytes,
            max_post_bytes,
        )
    };
    let mut request = match parsed {
        Ok(request) => request,
        Err(error) => {
            eprintln!("{error:?}");
            std::process::exit(1);
        }
    };
    let metadata = rustpon::files::Limits {
        max_file_uploads,
        max_upload_bytes,
        ..rustpon::files::Limits::default()
    };
    if admit_mode {
        let policy = rustpon::admission::Policy {
            metadata,
            ..rustpon::admission::Policy::default()
        };
        let admitted = match max_post_bytes {
            Some(max) => rustpon::admission::admit_with_php_post_max_size(request, policy, max),
            None => rustpon::admission::admit(request, policy),
        }
        .map_err(|error| format!("admission: {error:?}"))?;
        let mut out = io::BufWriter::new(io::stdout().lock());
        write!(out, "{{\"files_tree\":")?;
        admitted.metadata().write_json(&mut out)?;
        write!(out, ",\"files_tree_typed\":")?;
        admitted.metadata().write_php_files_json(&mut out)?;
        write!(out, ",\"accepted_indices\":[")?;
        let mut first_index = true;
        for index in admitted.accepted_indices() {
            if !first_index {
                write!(out, ",")?;
            }
            first_index = false;
            write!(out, "{index}")?;
        }
        writeln!(out, "]}}")?;
        out.flush()?;
        return Ok(());
    }
    if files_mode {
        let files = rustpon::files::project(&request, metadata)
            .map_err(|error| format!("files: {error:?}"))?;
        let mut out = io::BufWriter::new(io::stdout().lock());
        write!(out, "{{\"files_tree\":")?;
        files.write_json(&mut out)?;
        write!(out, ",\"files_tree_typed\":")?;
        files.write_php_files_json(&mut out)?;
        writeln!(out, "}}")?;
        out.flush()?;
        return Ok(());
    }
    if post_mode {
        let post = match request.into_post(post::Limits::default()) {
            Ok(post) => post,
            Err(error) => {
                eprintln!("Post::{error:?}");
                std::process::exit(1);
            }
        };
        let mut out = io::BufWriter::new(io::stdout().lock());
        write!(out, "{{\"post_tree\":")?;
        post.write_json(&mut out)?;
        let stats = post.stats();
        writeln!(
            out,
            ",\"stats\":{{\"nodes\":{},\"key_bytes\":{},\"work\":{},\"memory_capacity\":{}}}}}",
            stats.nodes, stats.key_bytes, stats.work, stats.memory_capacity
        )?;
        out.flush()?;
        return Ok(());
    }
    let mut out = io::BufWriter::new(io::stdout().lock());
    write!(out, "{{\"parts\":[")?;
    let count = request.parts().len();
    for index in 0..count {
        if index != 0 {
            write!(out, ",")?;
        }
        let view = request.parts().nth(index).expect("part exists");
        write!(out, "{{\"name_hex\":\"")?;
        HexWriter(&mut out).write_all(view.name)?;
        write!(out, "\",\"filename_hex\":")?;
        if let Some(filename) = view.filename {
            write!(out, "\"")?;
            HexWriter(&mut out).write_all(filename)?;
            write!(out, "\"")?;
        } else {
            write!(out, "null")?;
        }
        write!(out, ",\"content_type_hex\":")?;
        if let Some(content_type) = view.content_type {
            write!(out, "\"")?;
            HexWriter(&mut out).write_all(content_type)?;
            write!(out, "\"")?;
        } else {
            write!(out, "null")?;
        }
        write!(out, ",\"data_hex\":\"")?;
        let is_file = view.file_bytes.is_some();
        if let Some(value) = view.value {
            HexWriter(&mut out).write_all(value)?;
        }
        if is_file {
            request
                .upload_mut(index)
                .expect("upload")
                .copy_to(&mut HexWriter(&mut out))?;
        }
        write!(out, "\"}}")?;
    }
    writeln!(out, "]}}")?;
    out.flush()?;
    Ok(())
}
