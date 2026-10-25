//! Local stdin harness. No listener or network access.
use rustpon::post;
use rustpon::{Limits, parse_reader};
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
    let post_mode = args.len() == 4 && args[1] == "--post";
    let files_mode = args.len() == 4 && args[1] == "--files";
    if args.len() != 3 && !post_mode && !files_mode {
        eprintln!("usage: multipart [--post|--files] BOUNDARY PRIVATE_SPOOL_DIR < body.bin");
        std::process::exit(2);
    }
    let first = if post_mode || files_mode { 2 } else { 1 };
    let boundary = args[first].to_str().ok_or("boundary must be ASCII")?;
    let mut request = match parse_reader(
        &mut io::stdin().lock(),
        boundary.as_bytes(),
        Limits::default(),
        Path::new(&args[first + 1]),
    ) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("{error:?}");
            std::process::exit(1);
        }
    };
    if files_mode {
        let files = rustpon::files::project(&request, rustpon::files::Limits::default())
            .map_err(|error| format!("files: {error:?}"))?;
        let mut out = io::BufWriter::new(io::stdout().lock());
        write!(out, "{{\"files_tree\":")?;
        files.write_json(&mut out)?;
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
