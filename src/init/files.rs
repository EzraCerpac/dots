use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use toml_edit::{Document, Item};

use crate::Fail;

const MODE: u32 = 0o644;
const DIRECTORIES: &[&str] = &[
    "dots",
    "dots/hooks",
    "dots/hooks/update.d",
    "dots/hooks/bootstrap.d",
    "dots/commands",
    "dots/exceptions",
];

pub struct Template {
    pub path: &'static str,
    pub bytes: &'static [u8],
    pub mode: u32,
}

macro_rules! template {
    ($path:literal, $file:literal) => {
        Template {
            path: $path,
            bytes: include_bytes!($file),
            mode: MODE,
        }
    };
}

const CONFIG: Template = template!("config.toml", "config.toml");

pub const TEMPLATES: &[Template] = &[
    CONFIG,
    template!("dots/README.md", "README.md"),
    template!("dots/exceptions.tsv", "../../init/exceptions.tsv"),
    template!("dots/retired-dotfiles", "../../init/retired-dotfiles"),
    template!(
        "dots/retired-declarations",
        "../../init/retired-declarations"
    ),
    template!(".gitleaks.toml", "../../init/gitleaks.toml"),
    template!(".gitignore", "../../init/gitignore"),
];

pub(super) fn scaffold(root: &Path) -> Result<(), Fail> {
    let path = root.join(CONFIG.path);
    if create(&path, &CONFIG)? {
        println!("created config.toml");
    } else {
        let action = if ensure_profiles(&path)? {
            "added"
        } else {
            "kept"
        };
        println!("{action} config.toml [vars] dots_profiles");
    }
    for directory in DIRECTORIES {
        mkdir(root, directory)?;
    }
    for template in &TEMPLATES[1..] {
        let action = if create(&root.join(template.path), template)? {
            "created"
        } else {
            "kept"
        };
        println!("{action} {}", template.path);
    }
    Ok(())
}

fn mkdir(root: &Path, name: &str) -> Result<(), Fail> {
    let path = root.join(name);
    match fs::create_dir(&path) {
        Ok(()) => println!("created {name}/"),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
                return Err(Fail::usage(format!(
                    "{} must be a directory",
                    path.display()
                )));
            }
            println!("kept {name}/");
        }
        Err(err) => return Err(super::file_error(&path, err)),
    }
    Ok(())
}

fn create(path: &Path, template: &Template) -> Result<bool, Fail> {
    let create = || -> io::Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(_) => return Ok(false),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        let file = candidate(path, template.bytes, template.mode)?;
        match file.persist_noclobber(path) {
            Ok(_) => Ok(true),
            Err(err) if err.error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(err) => Err(err.error),
        }
    };
    create().map_err(|err| super::file_error(path, err))
}

fn ensure_profiles(path: &Path) -> Result<bool, Fail> {
    let target = path
        .canonicalize()
        .map_err(|err| super::file_error(path, err))?;
    let meta = fs::metadata(&target).map_err(|err| super::file_error(path, err))?;
    if !meta.is_file() {
        return Err(Fail::usage(format!(
            "{} must be a regular file",
            path.display()
        )));
    }
    let invalid = |why: &str| Fail::usage(format!("{}: {why}", path.display()));
    let original = fs::read_to_string(&target).map_err(|err| {
        if err.kind() == io::ErrorKind::InvalidData {
            invalid("config.toml must be UTF-8 TOML")
        } else {
            super::file_error(path, err)
        }
    })?;
    let Some(changed) = insert_profiles(&original).map_err(invalid)? else {
        return Ok(false);
    };
    let temp = candidate(&target, changed.as_bytes(), meta.permissions().mode())
        .map_err(|err| super::file_error(path, err))?;
    temp.persist(&target)
        .map_err(|err| super::file_error(path, err.error))?;
    Ok(true)
}

fn candidate(path: &Path, contents: &[u8], mode: u32) -> io::Result<tempfile::NamedTempFile> {
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().expect("absolute file path"))?;
    temp.write_all(contents)?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    Ok(temp)
}

fn insert_profiles(original: &str) -> Result<Option<String>, &'static str> {
    const ASSIGNMENT: &str = "dots_profiles = \"default\"";
    let doc = Document::parse(original).map_err(|_| "invalid TOML")?;
    let newline = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut changed = original.to_owned();
    match doc.get("vars") {
        Some(item) if item.get("dots_profiles").is_some() => return Ok(None),
        Some(Item::Table(table)) if table.is_dotted() => {
            let start = if original.starts_with('\u{feff}') {
                3
            } else {
                0
            };
            changed.insert_str(start, &format!("vars.{ASSIGNMENT}{newline}"));
        }
        Some(Item::Table(table)) if !table.is_implicit() => {
            let start = table.span().expect("parsed explicit TOML table").start;
            if let Some(header_end) = original[start..].find('\n') {
                changed.insert_str(start + header_end + 1, &format!("{ASSIGNMENT}{newline}"));
            } else {
                changed.push_str(&format!("{newline}{ASSIGNMENT}{newline}"));
            }
        }
        Some(Item::Value(toml_edit::Value::InlineTable(table))) => {
            let start = table.span().expect("parsed TOML inline table").start + 1;
            let separator = if table.is_empty() { " " } else { "," };
            changed.insert_str(start, &format!(" {ASSIGNMENT}{separator}"));
        }
        None | Some(Item::Table(_)) => {
            if !original.is_empty() {
                if !original.ends_with('\n') {
                    changed.push_str(newline);
                }
                changed.push_str(newline);
            }
            changed.push_str(&format!("[vars]{newline}{ASSIGNMENT}{newline}"));
        }
        Some(_) => return Err("[vars] must be a table to add dots_profiles"),
    }
    Document::parse(changed.as_str()).map_err(|_| "could not add [vars] dots_profiles")?;
    Ok(Some(changed))
}

#[cfg(test)]
mod tests {
    use super::insert_profiles;

    #[test]
    fn adds_profiles_without_changing_existing_bytes() {
        for (before, insertion) in [
            (
                "[\"vars\"] # vars\r\nname = 'x'\r\n[tools]\r\njq = '1'",
                "dots_profiles = \"default\"\r\n",
            ),
            ("vars.name = 'x'", "vars.dots_profiles = \"default\"\n"),
            ("vars = { name = 'x' }", " dots_profiles = \"default\","),
            ("vars = { }", " dots_profiles = \"default\" "),
            (
                "[vars.nested]\na = 'b'\n",
                "\n[vars]\ndots_profiles = \"default\"\n",
            ),
            (
                "[tools]\njq = '1'",
                "\n\n[vars]\ndots_profiles = \"default\"\n",
            ),
        ] {
            let after = insert_profiles(before).unwrap().unwrap();
            assert_eq!(after.replacen(insertion, "", 1), before);
            assert_eq!(insert_profiles(&after).unwrap(), None);
        }
    }
}
