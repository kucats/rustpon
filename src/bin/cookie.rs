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
    let usage = "usage: cookie [--php-arrays] [--php-max-input-vars N] [--php-max-input-nesting-level N] < header";
    let mut array_mode = false;
    let mut php_max_input_vars = None;
    let mut php_max_input_nesting_level = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--php-arrays" if !array_mode => array_mode = true,
            "--php-max-input-vars" if php_max_input_vars.is_none() => {
                let value = arguments.next().ok_or_else(|| usage.to_owned())?;
                php_max_input_vars = Some(value.parse::<usize>().map_err(|_| usage.to_owned())?);
            }
            "--php-max-input-nesting-level" if php_max_input_nesting_level.is_none() => {
                let value = arguments.next().ok_or_else(|| usage.to_owned())?;
                php_max_input_nesting_level =
                    Some(value.parse::<usize>().map_err(|_| usage.to_owned())?);
            }
            _ => return Err(usage.to_owned()),
        }
    }
    if php_max_input_nesting_level.is_some() && !array_mode {
        return Err(usage.to_owned());
    }
    let mut out = io::BufWriter::new(io::stdout().lock());
    if array_mode {
        let tree = cookie::array::parse_one_level_header_with_php_ini_limits(
            &header,
            limits,
            php_max_input_vars,
            php_max_input_nesting_level.unwrap_or(64),
        )
        .map_err(|error| format!("cookie: {error:?}"))?;
        out.write_all(b"{\"cookie_tree\":")
            .map_err(|error| error.to_string())?;
        tree.write_json(&mut out)
            .map_err(|error| error.to_string())?;
    } else {
        let cookies = match php_max_input_vars {
            Some(max) => cookie::parse_scalar_header_with_php_max_input_vars(&header, limits, max),
            None => cookie::parse_scalar_header(&header, limits),
        }
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
