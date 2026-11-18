use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;
use unicode_general_category::{GeneralCategory, get_general_category};

pub(super) use caseless::default_case_fold_str as fold;

pub(super) fn canonical(key: &str) -> Option<Cow<'_, str>> {
    if !valid(key) {
        return None;
    }
    if let Some(repo) = key
        .strip_prefix("github:")
        .or_else(|| key.strip_prefix("https://github.com/"))
    {
        return github_repo(repo).map(|repo| format!("github:{repo}").into());
    }
    if key.contains("://") {
        return None;
    }
    if key.ends_with(':') {
        return None;
    }
    for (prefix, manager) in [
        ("brew:homebrew/core/", "brew"),
        ("brew-cask:homebrew/cask/", "brew-cask"),
    ] {
        if let Some(name) = key.strip_prefix(prefix) {
            return valid(name).then(|| format!("{manager}:{name}").into());
        }
    }
    Some(key.into())
}

pub(super) fn github_repo(repo: &str) -> Option<String> {
    let repo = repo.strip_suffix('/').unwrap_or(repo);
    let (owner, name) = repo.split_once('/')?;
    let name = name
        .strip_suffix(".git")
        .filter(|name| !name.is_empty())
        .unwrap_or(name);
    (starts_with_alnum(owner)
        && owner
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        && !matches!(name, "" | "." | "..")
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c)))
    .then(|| format!("{owner}/{name}").to_ascii_lowercase())
}

pub(super) fn package(key: &str) -> bool {
    let Some((manager, name)) = key.split_once(':') else {
        return false;
    };
    if !starts_with_alnum(name) {
        return false;
    }
    match manager {
        "brew" | "brew-cask" => {
            name.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_.+@/-".contains(&c))
                && name.split('/').all(|part| !matches!(part, "" | "." | ".."))
        }
        "apt" | "dnf" | "pacman" | "apk" => name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.+:-".contains(&c)),
        "mas" => name.bytes().all(|c| c.is_ascii_digit()),
        _ => false,
    }
}

fn starts_with_alnum(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
}

pub(super) fn aliases(key: &str) -> impl Iterator<Item = String> + '_ {
    let tail = key
        .strip_prefix("https://github.com/")
        .or_else(|| key.split_once(':').map(|(_, tail)| tail))
        .unwrap_or(key);
    let name = tail.rsplit('/').next().unwrap_or(tail);
    [tail, name]
        .into_iter()
        .filter(|name| valid(name))
        .map(fold)
}

pub(super) fn valid(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with(['-', ':', '/'])
        && !["~/", "./", "../"]
            .iter()
            .any(|prefix| text.starts_with(prefix))
        && safe_text(text)
}

pub(super) fn safe_text(text: &str) -> bool {
    use GeneralCategory::*;
    text.chars().all(|c| {
        c == ' '
            || !matches!(
                get_general_category(c),
                Control
                    | Format
                    | LineSeparator
                    | ParagraphSeparator
                    | PrivateUse
                    | SpaceSeparator
                    | Surrogate
                    | Unassigned
            )
    })
}

// Match difflib.get_close_matches: count ordered contiguous blocks, preferring
// the earliest block in the candidate and then the query when lengths tie.
pub(super) struct Close {
    query: Vec<char>,
    positions: HashMap<char, Vec<usize>>,
}

impl Close {
    pub(super) fn new(query: &str) -> Self {
        let query: Vec<_> = query.chars().collect();
        let mut positions = HashMap::<_, Vec<_>>::new();
        for (index, &letter) in query.iter().enumerate() {
            positions.entry(letter).or_default().push(index);
        }
        if query.len() >= 200 {
            let limit = query.len() / 100 + 1;
            positions.retain(|_, offsets| offsets.len() <= limit);
        }
        Self { query, positions }
    }

    pub(super) fn matches(&self, alias: &str) -> bool {
        let alias: Vec<_> = alias.chars().collect();
        let needed = (2 * (alias.len() + self.query.len())).div_ceil(5);
        if alias.len().min(self.query.len()) < needed {
            return false;
        }
        let mut ranges = vec![(&alias[..], 0..self.query.len())];
        let mut matched = 0;
        while let Some((alias, query)) = ranges.pop() {
            let (a, q, size) = self.longest_block(alias, &query);
            if size == 0 {
                continue;
            }
            matched += size;
            if matched >= needed {
                return true;
            }
            if a > 0 && q > query.start {
                ranges.push((&alias[..a], query.start..q));
            }
            if a + size < alias.len() && q + size < query.end {
                ranges.push((&alias[a + size..], q + size..query.end));
            }
        }
        false
    }
    fn longest_block(&self, alias: &[char], query: &Range<usize>) -> (usize, usize, usize) {
        let mut current = HashMap::new();
        let mut previous = HashMap::new();
        let (mut a, mut q, mut size) = (0, query.start, 0);
        for (i, letter) in alias.iter().enumerate() {
            current.clear();
            if let Some(positions) = self.positions.get(letter) {
                let start = positions.partition_point(|&j| j < query.start);
                for &j in positions[start..].iter().take_while(|&&j| j < query.end) {
                    let length = previous.get(&j).copied().unwrap_or(0) + 1;
                    current.insert(j + 1, length);
                    if length > size {
                        (a, q, size) = (i + 1 - length, j + 1 - length, length);
                    }
                }
            }
            std::mem::swap(&mut current, &mut previous);
        }
        // Popular query characters may extend a block but never seed one.
        while a > 0 && q > query.start && alias[a - 1] == self.query[q - 1] {
            (a, q, size) = (a - 1, q - 1, size + 1);
        }
        while a + size < alias.len()
            && q + size < query.end
            && alias[a + size] == self.query[q + size]
        {
            size += 1;
        }
        (a, q, size)
    }
}
