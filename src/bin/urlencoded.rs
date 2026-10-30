//! stdin harness for bounded application/x-www-form-urlencoded decoding.
//! Emits the PHP-style ordered tree only after complete parsing succeeds.
use rustpon::{Limits, post, urlencoded};
use std::io::{self, Write};

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut max_input_vars = None;
    let mut php_content_length: Option<u64> = None;
    let mut php_post_max_size: Option<u64> = None;
    let mut max_input_nesting_level = None;
    let mut php_semicolon_separator = false;
    let mut php_query_string = false;
    let mut php_nul_name_truncation = false;
    let mut php_arg_separators: Option<String> = None;
    let usage = "usage: urlencoded [--php-query-string] [--php-max-input-vars N] [--php-max-input-nesting-level N] [--php-semicolon-separator | --php-arg-separators BYTES] [--php-nul-name-truncation] [--php-content-length BYTES [--php-post-max-size BYTES]] < body.bin";
    while let Some(flag) = args.next() {
        if flag == "--php-semicolon-separator"
            && !php_semicolon_separator
            && php_arg_separators.is_none()
        {
            php_semicolon_separator = true;
            continue;
        }
        if flag == "--php-nul-name-truncation" && !php_nul_name_truncation {
            php_nul_name_truncation = true;
            continue;
        }
        if flag == "--php-query-string" && !php_query_string {
            php_query_string = true;
            continue;
        }
        let value = args.next().ok_or(usage)?;
        match flag.as_str() {
            "--php-arg-separators" if php_arg_separators.is_none() && !php_semicolon_separator => {
                php_arg_separators = Some(value);
            }
            "--php-content-length" if php_content_length.is_none() => {
                php_content_length = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| "invalid content length".to_owned())?,
                );
            }
            "--php-post-max-size" if php_post_max_size.is_none() => {
                php_post_max_size = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| "invalid post_max_size".to_owned())?,
                );
            }
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
    if php_post_max_size.is_some() && php_content_length.is_none() {
        return Err("--php-post-max-size requires --php-content-length".to_owned());
    }
    if php_content_length.is_some() && (php_semicolon_separator || php_arg_separators.is_some()) {
        return Err(
            "--php-content-length currently supports only the default '&' separator".to_owned(),
        );
    }
    if php_query_string && php_content_length.is_some() {
        return Err("--php-query-string cannot be combined with --php-content-length".to_owned());
    }
    let separators: &[u8] = if let Some(ref set) = php_arg_separators {
        set.as_bytes()
    } else if php_semicolon_separator {
        b"&;"
    } else {
        b"&"
    };
    // PHP counts explicit empty-name pairs and NUL-truncated names toward
    // max_input_vars, even though they do not appear in the published tree.
    // Preserve their wire positions only when this PHP quota is requested.
    let request = if php_query_string {
        if max_input_vars.is_some() {
            urlencoded::parse_reader_with_php_query_string_counting_vars(
                &mut io::stdin().lock(),
                Limits::default(),
                separators,
            )
        } else {
            urlencoded::parse_reader_with_php_query_string(
                &mut io::stdin().lock(),
                Limits::default(),
                separators,
            )
        }
    } else if let Some(length) = php_content_length {
        if max_input_vars.is_some() {
            urlencoded::parse_reader_with_php_content_length_sapi_vars(
                &mut io::stdin().lock(),
                Limits::default(),
                length,
                php_post_max_size,
            )
        } else {
            urlencoded::parse_reader_with_php_content_length(
                &mut io::stdin().lock(),
                Limits::default(),
                length,
                php_post_max_size,
            )
        }
    } else if max_input_vars.is_some() {
        urlencoded::parse_reader_with_php_input_var_counting(
            &mut io::stdin().lock(),
            Limits::default(),
            separators,
            php_nul_name_truncation,
        )
    } else if php_nul_name_truncation {
        urlencoded::parse_reader_with_php_nul_name_truncation(
            &mut io::stdin().lock(),
            Limits::default(),
            separators,
        )
    } else if php_arg_separators.is_some() || php_semicolon_separator {
        urlencoded::parse_reader_with_php_arg_separators(
            &mut io::stdin().lock(),
            Limits::default(),
            separators,
        )
    } else {
        urlencoded::parse_reader(&mut io::stdin().lock(), Limits::default())
    }
    .map_err(|error| format!("wire: {error:?}"))?;
    // PHP 8.4 HTTP SAPI counts blank pairs and admits N+1 tokens with a
    // configured max_input_vars=N. CLI parse_str() uses exactly N instead.
    let max_input_vars = if php_content_length.is_some() {
        max_input_vars.map(|limit| limit.saturating_add(1))
    } else {
        max_input_vars
    };
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
    let tree_key: &[u8] = if php_query_string {
        b"{\"get_tree\":"
    } else {
        b"{\"post_tree\":"
    };
    output
        .write_all(tree_key)
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
