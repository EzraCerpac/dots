use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use clap::{Arg, ArgAction, Command, ValueHint};
use unicode_general_category::{GeneralCategory, get_general_category};

use super::{Candidate, Cursor, Quote, Response, Word};
use crate::{
    cli,
    config::{Root, passive},
    config_commands, report,
    targets::{Scope, Targets},
};

const GITHUB_RELEASE_PREFIX: &str = "https://github.com/";

pub(super) fn complete(
    words: &[Word],
    cursor: Cursor,
    cwd: &Path,
    root: Option<&Root>,
) -> Response {
    let mut tree = cli::tree();
    tree.build();
    let Some(prior) = words.get(..cursor.word) else {
        return empty();
    };
    let current = words
        .get(cursor.word)
        .map_or(&[][..], |word| word.text.as_bytes());
    let Some(prefix) = current.get(..cursor.byte).map(OsStr::from_bytes) else {
        return empty();
    };
    let mut state = State::new(&tree);
    for word in prior {
        if !state.advance(word) {
            return empty();
        }
    }

    if let Some(arg) = state.pending {
        return option_value(state.command, arg, root, prefix, 0);
    }
    if !state.end_options && prefix.as_bytes().starts_with(b"-") {
        if let Some(parsed) = option_word(state.command, prefix)
            && let Some((arg, start)) = parsed.value
        {
            if parsed
                .flags
                .into_iter()
                .all(|flag| state.record(flag, None))
                && state.available(arg)
            {
                return option_value(state.command, arg, root, prefix, start);
            }
            return empty();
        }
        return prefix
            .to_str()
            .map_or_else(empty, |prefix| values(state.options(prefix)));
    }

    // Some slots have shell-native completion even for an empty prefix.
    let positional = state.positional();
    if let Some(arg) = positional
        && let Some(response) = path_response(arg, 0)
    {
        return response;
    }
    let Some(prefix) = prefix.to_str().filter(|text| safe_text(text)) else {
        return empty();
    };

    let name = state.command.get_name();
    let mut result = Vec::new();
    if state.can_select_child() {
        for child in state
            .command
            .get_subcommands()
            .filter(|child| !child.is_hide_set())
        {
            command_names(&mut result, child, prefix);
        }
        if name == tree.get_name() {
            if state.seen.is_empty()
                && let Some(root) = root
            {
                for name in config_commands::list(root) {
                    push_match(&mut result, prefix, &name, "Configuration command", true);
                }
            }
            if prefix.is_empty() {
                result.extend(state.options(prefix));
            }
        }
    }
    if let Some(arg) = positional {
        match name {
            "reports" => {
                for report in report::Command::ALL {
                    if let Some(child) = tree.find_subcommand(report.name()) {
                        command_names(&mut result, child, prefix);
                    }
                }
            }
            "test" => {
                for (suite, description) in [
                    ("python", "Python tests"),
                    ("node", "Node.js tests"),
                    ("shell", "Shell tests"),
                ] {
                    if !state.positionals.contains(&OsStr::new(suite)) {
                        push_match(&mut result, prefix, suite, description, true);
                    }
                }
            }
            "add" => add_prefixes(&mut result, prefix, &state.positionals, state.used("asset")),
            "remove" => {
                if let Some(root) = root {
                    let scope = state.removal_scope();
                    let used: Vec<_> = state
                        .positionals
                        .iter()
                        .filter_map(|arg| arg.to_str().map(str::to_owned))
                        .collect();
                    let keep_installed = state.used("keep-installed");
                    result.extend(
                        Targets::read_for_completion(root, scope, cwd)
                            .available(prefix, &used, keep_installed)
                            .into_iter()
                            .map(|entry| Candidate {
                                value: entry.value,
                                description: entry.description,
                                append_space: true,
                            }),
                    );
                }
            }
            "completions" => possible_values(&mut result, arg, prefix, "", None),
            _ => {}
        }
    }
    values(result)
}

struct Seen<'a> {
    command: &'a Command,
    arg: &'a Arg,
    value: Option<(&'a Word, usize)>,
}

struct State<'a> {
    command: &'a Command,
    seen: Vec<Seen<'a>>,
    pending: Option<&'a Arg>,
    positionals: Vec<&'a OsStr>,
    end_options: bool,
}

impl<'a> State<'a> {
    fn new(command: &'a Command) -> Self {
        Self {
            command,
            seen: Vec::new(),
            pending: None,
            positionals: Vec::new(),
            end_options: false,
        }
    }

    fn positional(&self) -> Option<&'a Arg> {
        let mut index = self.positionals.len();
        for arg in self.command.get_positionals() {
            let count = arg.get_num_args()?.max_values();
            if index < count && (!arg.is_last_set() || self.end_options) {
                return Some(arg);
            }
            index = index.saturating_sub(count);
        }
        None
    }

    fn can_select_child(&self) -> bool {
        !self.end_options
            && self.positionals.is_empty()
            && (!self.command.is_args_conflicts_with_subcommands_set()
                || !self
                    .seen
                    .iter()
                    .any(|seen| std::ptr::eq(seen.command, self.command)))
    }

    fn used(&self, long: &str) -> bool {
        self.seen
            .iter()
            .any(|seen| seen.arg.get_long() == Some(long))
    }

    fn advance(&mut self, word: &'a Word) -> bool {
        if let Some(arg) = self.pending.take() {
            let value = word.text.as_os_str();
            if !arg.is_allow_hyphen_values_set()
                && path_response(arg, 0).is_none()
                && value.as_encoded_bytes().starts_with(b"-")
                && value != "-"
            {
                return false;
            }
            self.seen
                .last_mut()
                .expect("pending option was recorded")
                .value = Some((word, 0));
            return true;
        }
        if !self.end_options {
            if word.text == "--" {
                self.end_options = true;
                return true;
            }
            if let Some(child) = self.command.find_subcommand(&word.text)
                && self.can_select_child()
                && !child.is_hide_set()
            {
                self.command = child;
                return true;
            }
            if let Some(parsed) = option_word(self.command, &word.text) {
                for arg in parsed.flags {
                    if !self.record(arg, None) {
                        return false;
                    }
                }
                if let Some((arg, start)) = parsed.value {
                    return self.record(arg, Some((word, start))) && start < word.text.len();
                }
                return true;
            }
            if word.text.as_bytes().starts_with(b"-")
                && word.text != "-"
                && !self
                    .positional()
                    .is_some_and(Arg::is_allow_hyphen_values_set)
            {
                return false;
            }
        }
        let Some(positional) = self.positional() else {
            return false;
        };
        self.positionals.push(&word.text);
        self.end_options |= positional.is_trailing_var_arg_set();
        true
    }

    fn record(&mut self, arg: &'a Arg, value: Option<(&'a Word, usize)>) -> bool {
        if !self.available(arg)
            || arg.get_long() == Some("version")
            || matches!(
                arg.get_action(),
                ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
            )
        {
            return false;
        }
        if arg.get_action().takes_values() && value.is_none() {
            if arg.is_require_equals_set() {
                return false;
            }
            self.pending = Some(arg);
        }
        self.seen.push(Seen {
            command: self.command,
            arg,
            value,
        });
        true
    }

    fn available(&self, arg: &Arg) -> bool {
        if arg.get_long() == Some("asset") {
            match self.positionals.as_slice() {
                [] => {}
                [request]
                    if request
                        .as_bytes()
                        .starts_with(GITHUB_RELEASE_PREFIX.as_bytes()) => {}
                _ => return false,
            }
        }
        let id = arg.get_id();
        self.seen.iter().all(|seen| {
            if seen.arg.get_id() == id {
                return matches!(arg.get_action(), ArgAction::Append | ArgAction::Count);
            }
            !self
                .command
                .get_arg_conflicts_with(arg)
                .iter()
                .any(|other| other.get_id() == seen.arg.get_id())
                && !seen
                    .command
                    .get_arg_conflicts_with(seen.arg)
                    .iter()
                    .any(|other| other.get_id() == id)
                && !self.command.get_groups().any(|group| {
                    group.get_args().any(|member| member == id)
                        && group.get_args().any(|member| member == seen.arg.get_id())
                        && !group.clone().is_multiple()
                })
        })
    }

    fn options(&self, prefix: &str) -> Vec<Candidate> {
        let mut result = Vec::new();
        let command = self.command.get_name();
        for arg in self
            .command
            .get_arguments()
            .filter(|arg| !arg.is_hide_set() && !arg.is_positional())
        {
            let long = arg.get_long();
            if (command == "dots"
                && !self.seen.is_empty()
                && matches!(long, Some("help" | "version")))
                || !self.available(arg)
            {
                continue;
            }
            let description = match long {
                Some("plain") => "Print concise progress".into(),
                Some("verbose") => "Show full output".into(),
                Some("help") => "Show help".into(),
                Some("version") => "Print the version".into(),
                _ => arg
                    .get_help()
                    .map_or_else(|| "Option".into(), |help| title(&help.to_string())),
            };
            for long in long
                .into_iter()
                .chain(arg.get_visible_aliases().into_iter().flatten())
            {
                push_match(
                    &mut result,
                    prefix,
                    &format!("--{long}"),
                    &description,
                    true,
                );
            }
            for short in arg
                .get_short()
                .into_iter()
                .chain(arg.get_visible_short_aliases().into_iter().flatten())
            {
                push_match(
                    &mut result,
                    prefix,
                    &format!("-{short}"),
                    &description,
                    true,
                );
            }
        }
        result
    }

    fn removal_scope(&self) -> Scope {
        for seen in &self.seen {
            match seen.arg.get_long() {
                Some("base") => return Scope::Base,
                Some("profile") => {
                    if let Some((word, start)) = seen.value {
                        let (text, _) = selector_value(word, start);
                        return Scope::Profile(text.to_string_lossy().into_owned());
                    }
                }
                Some("path") => {
                    if let Some((word, start)) = seen.value {
                        let (text, quote) = selector_value(word, start);
                        let path = Path::new(text);
                        return Scope::Path(match quote {
                            Quote::Unquoted if text.as_encoded_bytes().starts_with(b"~/") => {
                                std::env::var_os("HOME")
                                    .filter(|home| !home.is_empty())
                                    .and_then(|home| {
                                        path.strip_prefix("~")
                                            .ok()
                                            .map(|tail| Path::new(&home).join(tail))
                                    })
                                    .unwrap_or_else(|| path.to_owned())
                            }
                            _ => path.to_owned(),
                        });
                    }
                }
                _ => {}
            }
        }
        Scope::Active
    }
}

fn selector_value(word: &Word, start: usize) -> (&OsStr, Quote) {
    let quote = if start == 0 {
        word.quote
    } else {
        Quote::Quoted
    };
    (OsStr::from_bytes(&word.text.as_bytes()[start..]), quote)
}

struct OptionWord<'a> {
    flags: Vec<&'a Arg>,
    value: Option<(&'a Arg, usize)>,
}

fn option_word<'a>(command: &'a Command, word: &OsStr) -> Option<OptionWord<'a>> {
    let option = |test: &dyn Fn(&Arg) -> bool| {
        command
            .get_arguments()
            .find(|arg| !arg.is_hide_set() && test(arg))
    };
    if let Some(long) = word.as_bytes().strip_prefix(b"--") {
        let equals = long.iter().position(|byte| *byte == b'=');
        let key = str::from_utf8(&long[..equals.unwrap_or(long.len())]).ok()?;
        let arg = option(&|arg| {
            arg.get_long() == Some(key)
                || arg
                    .get_all_aliases()
                    .is_some_and(|aliases| aliases.contains(&key))
        })?;
        return if equals.is_some() {
            arg.get_action().takes_values().then(|| OptionWord {
                flags: Vec::new(),
                value: Some((arg, key.len() + 3)),
            })
        } else {
            Some(OptionWord {
                flags: vec![arg],
                value: None,
            })
        };
    }
    let word = word.to_str()?;
    let short = word.strip_prefix('-').filter(|word| !word.is_empty())?;
    let mut flags = Vec::new();
    for (index, letter) in short.char_indices() {
        let arg = option(&|arg| {
            arg.get_short() == Some(letter)
                || arg
                    .get_all_short_aliases()
                    .is_some_and(|aliases| aliases.contains(&letter))
        })?;
        let start = 1 + index + letter.len_utf8();
        if arg.get_action().takes_values() && start < word.len() {
            let equals = usize::from(word.as_bytes()[start] == b'=');
            if arg.is_require_equals_set() && equals == 0 {
                return None;
            }
            return Some(OptionWord {
                flags,
                value: Some((arg, start + equals)),
            });
        }
        flags.push(arg);
    }
    Some(OptionWord { flags, value: None })
}

fn path_response(arg: &Arg, value_start: usize) -> Option<Response> {
    match (arg.get_value_hint(), arg.get_long()) {
        (
            ValueHint::FilePath,
            Some("path" | "bundle" | "identity" | "identity-file" | "input" | "output"),
        ) => Some(Response::Path { value_start }),
        (ValueHint::DirPath, Some("from-dir")) => Some(Response::Dir { value_start }),
        (ValueHint::DirPath, None) if arg.get_id() == "DIR" => Some(Response::Dir { value_start }),
        _ => None,
    }
}

fn option_value(
    command: &Command,
    arg: &Arg,
    root: Option<&Root>,
    prefix: &OsStr,
    start: usize,
) -> Response {
    if let Some(response) = path_response(arg, start) {
        return response;
    }

    let Some(prefix) = prefix.to_str() else {
        return empty();
    };
    let (lead, fragment) = prefix.split_at(start);
    let delimiter = arg.get_value_delimiter();
    let (lead, fragment) = if let Some(delimiter) = delimiter
        && let Some(index) = fragment.rfind(delimiter)
    {
        let (previous, fragment) = fragment.split_at(index + delimiter.len_utf8());
        (format!("{lead}{previous}"), fragment)
    } else {
        (lead.to_owned(), fragment)
    };
    let mut result = Vec::new();
    match (command.get_name(), arg.get_long()) {
        (_, Some("profile")) | ("bootstrap", Some("env")) => {
            if let Some(names) = root.and_then(|root| passive::profiles(root).ok()) {
                for name in names {
                    push_value(&mut result, &lead, fragment, &name, "Profile");
                }
            }
        }
        ("bootstrap", Some("only" | "skip")) => {
            possible_values(&mut result, arg, fragment, &lead, Some("Bootstrap part"));
        }
        _ => possible_values(&mut result, arg, fragment, &lead, None),
    }
    values(result)
}

fn possible_values(
    result: &mut Vec<Candidate>,
    arg: &Arg,
    prefix: &str,
    lead: &str,
    description: Option<&str>,
) {
    for value in arg
        .get_possible_values()
        .into_iter()
        .filter(|value| !value.is_hide_set())
    {
        let name = value.get_name();
        let description = match (description, name) {
            (Some(description), _) => description.into(),
            (_, "bash") => "Bash script".into(),
            (_, "fish") => "Fish script".into(),
            (_, "zsh") => "Zsh script".into(),
            _ => value
                .get_help()
                .map_or_else(|| "Value".into(), |help| title(&help.to_string())),
        };
        push_value(result, lead, prefix, name, &description);
    }
}

fn command_names(result: &mut Vec<Candidate>, command: &Command, prefix: &str) {
    let name = command.get_name();
    let description = command
        .get_about()
        .map_or_else(|| "Command".into(), |about| title(&about.to_string()));
    push_match(result, prefix, name, &description, true);
    for alias in command.get_visible_aliases() {
        let description = match alias {
            "update" => "Same as up",
            "uninstall" => "Same as remove",
            _ => &description,
        };
        push_match(result, prefix, alias, description, true);
    }
}

fn add_prefixes(result: &mut Vec<Candidate>, prefix: &str, used: &[&OsStr], asset: bool) {
    let github = GITHUB_RELEASE_PREFIX;
    if (!asset || used.is_empty()) && !used.contains(&OsStr::new(github)) {
        push_match(result, prefix, github, "GitHub release", false);
    }
    if asset {
        return;
    }
    for (value, description) in [
        ("apk:", "apk package"),
        ("apt:", "apt package"),
        ("brew:", "Homebrew formula, else cask"),
        ("brew-cask:", "Homebrew cask"),
        ("dnf:", "dnf package"),
        ("mas:", "Mac App Store app"),
        ("pacman:", "pacman package"),
    ] {
        if !used.contains(&OsStr::new(value)) {
            push_match(result, prefix, value, description, false);
        }
    }
    for name in [
        "aqua", "asdf", "cargo", "conda", "dotnet", "gem", "github", "go", "npm", "pipx", "spm",
        "ubi", "vfox",
    ] {
        let value = format!("{name}:");
        if !used.contains(&OsStr::new(&value)) {
            push_match(result, prefix, &value, "mise backend", false);
        }
    }
}

fn push_match(
    result: &mut Vec<Candidate>,
    prefix: &str,
    value: &str,
    description: &str,
    append_space: bool,
) {
    if value.starts_with(prefix) {
        result.push(Candidate {
            value: value.into(),
            description: description.into(),
            append_space,
        });
    }
}

fn push_value(
    result: &mut Vec<Candidate>,
    lead: &str,
    prefix: &str,
    value: &str,
    description: &str,
) {
    if value.starts_with(prefix) {
        push_match(result, "", &format!("{lead}{value}"), description, true);
    }
}

fn title(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_owned()
}

fn safe_text(text: &str) -> bool {
    text.chars().all(|ch| {
        ch == ' '
            || !matches!(
                get_general_category(ch),
                GeneralCategory::Control
                    | GeneralCategory::Format
                    | GeneralCategory::LineSeparator
                    | GeneralCategory::ParagraphSeparator
                    | GeneralCategory::PrivateUse
                    | GeneralCategory::SpaceSeparator
                    | GeneralCategory::Surrogate
                    | GeneralCategory::Unassigned
            )
    })
}

fn values(mut result: Vec<Candidate>) -> Response {
    result.retain(|candidate| {
        !candidate.value.is_empty()
            && !candidate.value.starts_with(':')
            && safe_text(&candidate.value)
            && safe_text(&candidate.description)
    });
    result.sort_by(|left, right| left.value.cmp(&right.value));
    result.dedup_by(|left, right| left.value == right.value);
    Response::Values(result)
}

fn empty() -> Response {
    Response::Values(Vec::new())
}
