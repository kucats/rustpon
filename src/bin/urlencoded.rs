//! stdin harness for bounded application/x-www-form-urlencoded decoding.
//! Emits the PHP-style ordered tree only after complete parsing succeeds.
use rustpon::{Limits, post, urlencoded};
use std::io::{self, Write};

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut max_input_vars = None;
    let mut max_input_nesting_level = None;
    let mut php_semicolon_separator = false;
    let usage =
        "usage: urlencoded [--php-max-input-vars N] [--php-max-input-nesting-level N] [--php-semicolon-separator] < body.bin";
    while let Some(flag) = args.next() {
        if flag == "--php-semicolon-separator" && !php_semicolon_separator {
            php_semicolon_separator = true;
            continue;
        }
        let value = args.next().ok_or(usage)?;
        match flag.as_str() {
            "--php-max-input-vars" if max_input_vars.is_none() => {
                max_input_vars = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| "invalid max_input_vars".to_owned())?,
                );
            }
            "--php-max-input-nesting-level" if max_input_nesting_level.is_none() => {
                max_input_nesting_level = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| "invalid max_input_nesting_level".to_owned())?,
                );
            }
            _ => return Err(usage.to_owned()),
        }
    }
    let request = if php_semicolon_separator {
        urlencoded::parse_reader_with_php_semicolon_separator(
            &mut io::stdin().lock(),
            Limits::default(),
        )
    } else {
        urlencoded::parse_reader(&mut io::stdin().lock(), Limits::default())
    }
    .map_err(|error| format!("wire: {error:?}"))?;
    let post = match (max_input_vars, max_input_nesting_level) {
        (vars, Some(depth)) => request.into_post_with_php_limits(
            post::Limits::default(),
            vars.unwrap_or(usize::MAX),
            depth,
        ),
        (Some(vars), None) => request.into_post_with_max_input_vars(post::Limits::default(), vars),
        (None, None) => request.into_post(post::Limits::default()),
    }
    .map_err(|error| format!("post: {error:?}"))?;
    let mut output = io::BufWriter::new(io::stdout().lock());
    output
        .write_all(b"{\"post_tree\":")
        .map_err(|error| error.to_string())?;
    post.write_json(&mut output)
        .map_err(|error| error.to_string())?;
    let stats = post.stats();
    writeln!(
        output,
        ",\"stats\":{{\"nodes\":{},\"key_bytes\":{},\"work\":{},\"memory_capacity\":{}}}}}",
        stats.nodes, stats.key_bytes, stats.work, stats.memory_capacity
    )
    .map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
