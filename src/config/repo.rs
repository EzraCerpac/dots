use crate::Fail;

#[derive(Debug)]
pub struct RepoSetting {
    github: Option<String>,
    value: String,
}

impl RepoSetting {
    pub fn parse(value: &str) -> Result<Self, Fail> {
        if value.starts_with('-') {
            return Err(Fail::usage("a repository may not start with -"));
        }
        if value
            .split_once(':')
            .is_some_and(|(_, rest)| rest.starts_with(':'))
        {
            return Err(Fail::usage(
                "refusing a remote-helper address (TRANSPORT::ADDRESS)",
            ));
        }
        let invalid = || Fail::usage("expected OWNER/NAME or a Git URL");
        if value.is_empty() || value.chars().any(char::is_control) {
            return Err(invalid());
        }
        let credential = || Fail::usage("refusing a URL that carries a password or token");
        let github = if let Some((scheme, rest)) = value.split_once("://") {
            if !matches!(scheme, "https" | "http" | "ssh" | "git" | "file") {
                return Err(invalid());
            }
            let (authority, path) = rest.split_once('/').ok_or_else(invalid)?;
            if let Some((user, _)) = authority.rsplit_once('@')
                && (scheme != "ssh" || user.contains([':', '%', '@']))
            {
                return Err(credential());
            }
            if path.split_once('?').is_some_and(|(_, query)| {
                query.split('&').any(|entry| {
                    let key = entry
                        .split('=')
                        .next()
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    key.contains("token") || key.contains("password") || key == "key"
                })
            }) {
                return Err(credential());
            }
            if path.is_empty()
                || authority.chars().any(char::is_whitespace)
                || (authority.is_empty() && scheme != "file")
            {
                return Err(invalid());
            }
            match (scheme, authority) {
                ("https", "github.com") | ("ssh", "git@github.com") => {
                    Some(github_name(path).ok_or_else(invalid)?)
                }
                _ => None,
            }
        } else if let Some((host, path)) = value.split_once(':') {
            if host.is_empty()
                || host.contains('/')
                || host.chars().any(char::is_whitespace)
                || path.is_empty()
            {
                return Err(invalid());
            }
            if host == "git@github.com" {
                Some(github_name(path).ok_or_else(invalid)?)
            } else {
                None
            }
        } else {
            Some(repository_name(value).ok_or_else(invalid)?)
        };
        Ok(Self {
            github,
            value: value.to_owned(),
        })
    }

    pub fn matches(&self, url: &str) -> bool {
        let Ok(other) = Self::parse(url) else {
            return false;
        };
        match (&self.github, &other.github) {
            (Some(expected), Some(actual)) => expected.eq_ignore_ascii_case(actual),
            (None, None) => self.value == other.value,
            _ => false,
        }
    }
}

fn github_name(path: &str) -> Option<String> {
    repository_name(path.strip_suffix(".git").unwrap_or(path))
}

fn repository_name(path: &str) -> Option<String> {
    let (owner, repo) = path.split_once('/')?;
    (!owner.is_empty()
        && owner
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        && !repo.is_empty()
        && repo
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)))
    .then(|| path.to_owned())
}
