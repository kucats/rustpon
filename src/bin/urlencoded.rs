//! stdin harness for bounded application/x-www-form-urlencoded decoding.
//! Emits the PHP-style ordered tree only after complete parsing succeeds.
use rustpon::{Limits, post, urlencoded};
use std::io::{self, Write};

fn run() -> Result<(), String> {
    if std::env::args_os().len() != 1 {
        return Err("usage: urlencoded < body.bin".to_owned());
    }
    let request = urlencoded::parse_reader(&mut io::stdin().lock(), Limits::default())
        .map_err(|error| format!("wire: {error:?}"))?;
    let post = request
        .into_post(post::Limits::default())
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
