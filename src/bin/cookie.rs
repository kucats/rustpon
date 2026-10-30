//! Binary-safe PHP Cookie scalar differential harness (one header on stdin).
use rustpon::cookie::{self, Limits};
use std::io::{self, Read, Write};

fn run() -> Result<(), String> {
    let limits = Limits::default();
    let mut header = Vec::new();
    // The extra byte makes oversized input an explicit error, without
    // unbounded buffering from an untrusted stdin.
    io::stdin()
        .lock()
        .take((limits.header_bytes as u64) + 1)
        .read_to_end(&mut header)
        .map_err(|error| error.to_string())?;
    let array_mode = match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => false,
        [flag] if flag == "--php-arrays" => true,
        _ => return Err("usage: cookie [--php-arrays] < header".to_owned()),
    };
    let mut out = io::BufWriter::new(io::stdout().lock());
    if array_mode {
        let tree = cookie::array::parse_one_level_header(&header, limits)
            .map_err(|error| format!("cookie: {error:?}"))?;
        out.write_all(b"{\"cookie_tree\":")
            .map_err(|error| error.to_string())?;
        tree.write_json(&mut out)
            .map_err(|error| error.to_string())?;
    } else {
        let cookies = cookie::parse_scalar_header(&header, limits)
            .map_err(|error| format!("cookie: {error:?}"))?;
        out.write_all(b"{\"cookie_pairs\":")
            .map_err(|error| error.to_string())?;
        cookies
            .write_json(&mut out)
            .map_err(|error| error.to_string())?;
    }
    out.write_all(b"}\n").map_err(|error| error.to_string())?;
    out.flush().map_err(|error| error.to_string())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
