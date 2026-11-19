mod grammar;

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;

use crate::{Env, Fail, config::Root};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Quote {
    Unquoted,
    Quoted,
    Escaped,
}

#[derive(Debug)]
pub struct Word {
    pub text: OsString,
    /// Whether a leading tilde would be expanded by the shell.
    pub quote: Quote,
}

/// Zero-based offsets in the words after `dots` and the decoded current word.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cursor {
    pub word: usize,
    pub byte: usize,
}

pub struct Request<'a> {
    pub words: &'a [Word],
    pub cursor: Cursor,
    pub shell: Shell,
    pub cwd: &'a Path,
    pub root: &'a Root,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Candidate {
    pub value: String,
    pub description: String,
    pub append_space: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Response {
    Path { value_start: usize },
    Dir { value_start: usize },
    Values(Vec<Candidate>),
}

pub fn complete(request: Request<'_>) -> Response {
    // The shell has already decoded its words. Candidate selection is shared.
    let Request {
        words,
        cursor,
        shell: _,
        cwd,
        root,
    } = request;
    grammar::complete(words, cursor, cwd, Some(root))
}

pub fn script(shell: Shell) -> Result<&'static str, Fail> {
    Ok(match shell {
        Shell::Bash => include_str!("dots.bash"),
        Shell::Zsh => include_str!("_dots"),
        Shell::Fish => include_str!("dots.fish"),
    })
}

pub fn query(args: &[OsString]) -> Result<u8, Fail> {
    let (shell, words, raw_fish, path_slot_only) = match args {
        [shell, words @ ..] if shell == "bash" => (Shell::Bash, words, None, false),
        [shell, words @ ..] if shell == "zsh" => (Shell::Zsh, words, None, false),
        [shell, mode, line, words @ ..] if shell == "fish" => {
            (Shell::Fish, words, line.to_str(), mode == "path-slot")
        }
        _ => return Ok(0),
    };
    let Some((current, prior)) = words.split_last() else {
        return Ok(0);
    };

    // Bash and Zsh expand prior unquoted home paths themselves. Fish exposes
    // decoded tokens without their quotes, so recover just the tilde spelling
    // from the raw prefix. Never request Fish's expanded command line.
    let fish_quotes = raw_fish.map_or_else(Vec::new, fish_word_quotes);
    let words: Vec<_> = words
        .iter()
        .enumerate()
        .map(|(i, text)| Word {
            text: text.clone(),
            quote: match fish_quotes.len() {
                n if n == prior.len() + 1 || n == prior.len() + 2 => {
                    fish_quotes.get(i + 1).copied().unwrap_or(Quote::Quoted)
                }
                _ => Quote::Quoted,
            },
        })
        .collect();
    let cursor = Cursor {
        word: prior.len(),
        byte: current.as_encoded_bytes().len(),
    };

    if path_slot_only {
        // Fish tests this before enabling native file completion. No dynamic
        // candidates are needed, including when the grammar has no path slot.
        let response = grammar::complete(&words, cursor, Path::new("."), None);
        if matches!(response, Response::Path { value_start: 0 }) {
            let _ = write_response(io::stdout().lock(), response);
        }
        return Ok(0);
    }
    let env = Env::current().ok();
    let root = env.as_ref().and_then(|env| Root::resolve(env).ok());
    let cwd = env.as_ref().map_or(Path::new("."), Env::cwd);
    let response = if let Some(root) = &root {
        complete(Request {
            words: &words,
            cursor,
            shell,
            cwd,
            root,
        })
    } else {
        grammar::complete(&words, cursor, cwd, None)
    };
    // A shell can abandon completion before consuming the response.
    let _ = write_response(io::stdout().lock(), response);
    Ok(0)
}

fn write_response(mut out: impl Write, response: Response) -> io::Result<()> {
    match response {
        Response::Path { value_start: 0 } => writeln!(out, ":files"),
        Response::Path { .. } => writeln!(out, ":files="),
        Response::Dir { value_start: 0 } => writeln!(out, ":dirs"),
        Response::Dir { .. } => writeln!(out, ":dirs="),
        Response::Values(values) => {
            for candidate in values {
                let suffix = if candidate.append_space {
                    ""
                } else {
                    "\tnospace"
                };
                writeln!(
                    out,
                    "{}\t{}{}",
                    candidate.value, candidate.description, suffix
                )?;
            }
            Ok(())
        }
    }
}

// Locate words in Fish's raw process prefix only to classify leading tildes.
// Fish remains responsible for decoding the words. A mismatch with its
// tokenizer disables tilde expansion instead of guessing.
fn fish_word_quotes(line: &str) -> Vec<Quote> {
    let mut words = Vec::new();
    let mut started = false;
    let mut quote = None;
    let mut chars = line.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if quote.is_none() && ch.is_whitespace() {
            started = false;
            continue;
        }
        if !started {
            started = true;
            words.push(match ch {
                '"' | '\'' => Quote::Quoted,
                '\\' => Quote::Escaped,
                '~' if line[index..].starts_with("~/") => Quote::Unquoted,
                _ => Quote::Quoted,
            });
        }
        match ch {
            '\\' => {
                if let Some(&(_, next)) = chars.peek()
                    && (quote.is_none() || Some(next) == quote || next == '\\')
                {
                    chars.next();
                }
            }
            ch if Some(ch) == quote => quote = None,
            '"' | '\'' if quote.is_none() => quote = Some(ch),
            _ => {}
        }
    }
    words
}
