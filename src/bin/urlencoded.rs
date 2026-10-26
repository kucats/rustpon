//! stdin harness for bounded application/x-www-form-urlencoded decoding.
//! Emits the PHP-style ordered tree only after complete parsing succeeds.
use rustpon::{Limits, post, urlencoded};
use std::io::{self, Write};

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let max_input_vars = match (args.next(), args.next(), args.next()) {
        (None, None, None) => None,
        (Some(flag), Some(value), None) if flag == "--php-max-input-vars" => {
            Some(value.parse::<usize>().map_err(|_| "invalid max_input_vars".to_owned())?)
        }
        _ => return Err("usage: urlencoded [--php-max-input-vars N] < body.bin".to_owned()),
    };
    let request = urlencoded::parse_reader(&mut io::stdin().lock(), Limits::default())
        .map_err(|error| format!("wire: {error:?}"))?;
    let post = match max_input_vars {
        Some(max) => request.into_post_with_max_input_vars(post::Limits::default(), max),
        None => request.into_post(post::Limits::default()),
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
