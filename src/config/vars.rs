use std::path::Path;

use toml_edit::{DocumentMut, Item, TableLike};

use super::{RepoSetting, Root, file_error, passive, valid_id};
use crate::Fail;

#[derive(Debug)]
pub struct Vars {
    pub profiles: Vec<String>,
    pub source_repo: Option<RepoSetting>,
    pub state_repo: Option<RepoSetting>,
    pub self_updating_casks: Vec<String>,
    pub publish_on_behalf_of: Option<String>,
}

impl Vars {
    pub fn read(root: &Root) -> Result<Self, Fail> {
        let path = root.path().join("config.toml");
        let document = passive::toml(&path)?;
        let profiles = profiles(&document, &path)?;
        let vars = vars(&document);
        let read = |key: &str| {
            vars.and_then(|table| table.get(key))
                .map(|item| {
                    item.as_str().ok_or_else(|| {
                        file_error(&path, format_args!("[vars] {key} must be a string"))
                    })
                })
                .transpose()
        };
        let repo = |key: &str| {
            read(key)?
                .filter(|value| !value.is_empty())
                .map(|value| {
                    RepoSetting::parse(value).map_err(|error| {
                        Fail::usage(format!("{key} in {}: {error}", path.display()))
                    })
                })
                .transpose()
        };
        Ok(Self {
            profiles,
            source_repo: repo("dots_source_repo")?,
            state_repo: repo("dots_state_repo")?,
            self_updating_casks: read("dots_self_updating_casks")?
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_owned)
                .collect(),
            publish_on_behalf_of: read("dots_publish_on_behalf_of")?.map(str::to_owned),
        })
    }
}

fn vars(document: &DocumentMut) -> Option<&dyn TableLike> {
    document.get("vars").and_then(Item::as_table_like)
}

pub(super) fn profiles(document: &DocumentMut, path: &Path) -> Result<Vec<String>, Fail> {
    let missing = || {
        Fail::usage(format!(
            "missing setting: add dots_profiles to [vars] in {}",
            path.display()
        ))
    };
    let item = vars(document)
        .and_then(|table| table.get("dots_profiles"))
        .ok_or_else(missing)?;
    let value = item
        .as_str()
        .ok_or_else(|| file_error(path, "[vars] dots_profiles must be a string"))?;
    let mut names = Vec::new();
    for name in value.split_whitespace() {
        if !valid_id(name) || matches!(name, "base" | "local") || name.starts_with("host-") {
            return Err(Fail::usage(format!(
                "invalid setting dots_profiles in {}: '{name}' is not a profile name (lowercase letters, digits and -; not base, local or host-*)",
                path.display()
            )));
        }
        if !names.iter().any(|profile| profile == name) {
            names.push(name.to_owned());
        }
    }
    if names.is_empty() {
        return Err(missing());
    }
    Ok(names)
}
