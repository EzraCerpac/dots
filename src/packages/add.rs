use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::Command;

use toml_edit::{DocumentMut, InlineTable, Item, Value, value};

use crate::config::Exceptions;
use crate::process::Mise;
use crate::run::{Run, StageId};
use crate::{Args, Ctx, Fail};

use super::edit::{self, Edit, PACKAGES, TOOLS};
use super::{
    adopt, file_request, github, github_url, held, native, scope, selected_path, software_request,
    stage, supervise,
};

pub fn run(ctx: &Ctx, args: &Args, run: &Run<'_>) -> Result<(), Fail> {
    let requests = args
        .get_many::<OsString>("REQUEST")
        .expect("required by grammar");
    let asset = args.get_one::<String>("asset").map(String::as_str);
    let path = selected_path(ctx, &scope(args), true)?.expect("add always selects a file");
    let files = adopt::requests(ctx, Some(&path), requests.clone())?
        .into_iter()
        .map(|file| {
            file.map(|file| adopt::prepare_add(ctx, run, file, &path))
                .transpose()
        })
        .collect::<Result<Vec<_>, Fail>>()?;
    let exceptions = requests
        .clone()
        .any(|r| !file_request(r))
        .then(|| Exceptions::read(&ctx.root))
        .transpose()?;

    // Holds apply to the entire invocation, including packages for other profiles.
    for request in requests.clone().filter(|r| !file_request(r)) {
        let request = software_request(request)?;
        let key = if let Some((key, _)) = native::add_request(request)? {
            key
        } else if let Some(repo) = request.strip_prefix("https://github.com/") {
            let repo = crate::targets::github_repo(repo).ok_or_else(|| {
                Fail::new(
                    1,
                    "use an HTTPS GitHub repository URL: https://github.com/OWNER/REPO",
                )
            })?;
            format!("github:{repo}")
        } else {
            ToolRequest::parse(request)?.key
        };
        if let Some(recipe) = exceptions.as_ref().and_then(|rules| rules.held_by(&key)) {
            return Err(held(&key, recipe));
        }
    }
    for (request, file) in requests.zip(files) {
        stage(run, "Add request", "dots add", |id| {
            if let Some(file) = file {
                file.execute(ctx)
            } else {
                install(
                    ctx,
                    run,
                    id,
                    &path,
                    software_request(request)?,
                    asset,
                    exceptions.as_ref().expect("software request"),
                )
            }
        })?;
    }
    Ok(())
}

fn install(
    ctx: &Ctx,
    run: &Run<'_>,
    id: StageId,
    path: &Path,
    request: &str,
    asset: Option<&str>,
    exceptions: &Exceptions,
) -> Result<(), Fail> {
    let mut edit = Edit::open(path)?;
    let mise = Mise::new(ctx)?;

    if let Some((original, version)) = native::add_request(request)? {
        let mut key = original.clone();
        let (manager, name) = native::split(&original)?;
        if matches!(manager, "brew" | "brew-cask" | "mas") && !cfg!(target_os = "macos") {
            return Err(Fail::usage(format!("{key} is supported on macOS only")));
        }
        native::validate_name(manager, name)?;
        verify_manager_enabled(&mise, manager)?;
        let mut declaration = package_value(&edit.doc.document, &key, version.as_deref())?;
        let dir = tempfile::tempdir()
            .map_err(|error| Fail::new(1, format!("could not stage {key}: {error}")))?;
        let mut staged = package_document(ctx, &edit.doc.document, &key, declaration.clone())?;
        write_staged(dir.path(), &staged)?;
        if manager == "brew" {
            let probe = run
                .probe(isolated(
                    &mise,
                    dir.path(),
                    [
                        "bootstrap",
                        "packages",
                        "apply",
                        "--dry-run",
                        "--manager",
                        manager,
                    ],
                )?)
                .map_err(|error| {
                    Fail::new(1, format!("mise could not check formula {name}: {error}"))
                })?;
            if !probe.status.success() {
                let errors = String::from_utf8_lossy(&probe.stderr);
                if !formula_absent(name, &errors) {
                    let diagnostic = if errors.trim().is_empty() {
                        format!("could not verify formula {name}")
                    } else {
                        errors.trim().to_owned()
                    };
                    return finish_install(
                        run,
                        edit,
                        PACKAGES,
                        &key,
                        declaration,
                        None,
                        Err(Fail::new(1, diagnostic)),
                    );
                }
                key = format!("brew-cask:{name}");
                if let Some(recipe) = exceptions.held_by(&key) {
                    return Err(held(&key, recipe));
                }
                verify_manager_enabled(&mise, "brew-cask")?;
                if let Some(cask) = edit::entry(&edit.doc.document, PACKAGES, &key)? {
                    if edit::entry(&edit.doc.document, PACKAGES, &original)?.is_some()
                        && cask.to_string() != declaration.to_string()
                    {
                        return Err(Fail::usage(format!(
                            "conflicting formula and cask declarations for {name}"
                        )));
                    }
                    declaration = cask.clone();
                }
                staged = package_document(ctx, &edit.doc.document, &key, declaration.clone())?;
                write_staged(dir.path(), &staged)?;
            }
        }
        let (manager, name) = native::split(&key)?;
        let staged = NativeInstall {
            mise: &mise,
            run,
            dir: dir.path(),
            manager,
            name,
        };
        staged.verify(false)?;
        let result = supervise(
            run,
            id,
            isolated(
                &mise,
                dir.path(),
                ["bootstrap", "packages", "apply", "--manager", manager],
            )?,
            &format!("install {key}"),
        )
        .and_then(|()| staged.verify(true));
        let replace = (original != key).then_some(original.as_str());
        return finish_install(run, edit, PACKAGES, &key, declaration, replace, result);
    }

    if github_url(request) {
        let (key, declaration) = github::resolve(ctx, run, request, asset)?;
        if let Some(recipe) = exceptions.held_by(&key) {
            return Err(held(&key, recipe));
        }
        let dir = tempfile::tempdir()
            .map_err(|error| Fail::new(1, format!("could not stage {key}: {error}")))?;
        let mut staged = DocumentMut::new();
        edit::put(&mut staged, TOOLS, &key, declaration.clone())?;
        write_staged(dir.path(), &staged)?;
        let result = supervise(
            run,
            id,
            isolated(&mise, dir.path(), ["install", "--raw", "--", &key])?,
            &format!("install {key}"),
        );
        return finish_install(run, edit, TOOLS, &key, declaration, None, result);
    }

    let tool = ToolRequest::parse(request)?;
    let previous = edit::entry(&edit.doc.document, TOOLS, &tool.key)?;
    let declaration = tool.declaration(previous)?;
    let dir = tempfile::tempdir()
        .map_err(|error| Fail::new(1, format!("could not stage {}: {error}", tool.key)))?;
    let mut staged = DocumentMut::new();
    if let Some(previous) = previous {
        edit::put(&mut staged, TOOLS, &tool.key, previous.clone())?;
    }
    write_staged(dir.path(), &staged)?;
    let path = dir.path().join("mise.toml");
    let mut command = mise.command([
        "use".as_ref(),
        "--fuzzy".as_ref(),
        "--raw".as_ref(),
        "--path".as_ref(),
        path.as_os_str(),
        "--".as_ref(),
        request.as_ref(),
    ])?;
    trust_stage(ctx, &mut command, dir.path())?;
    let result = supervise(run, id, command, &format!("install {}", tool.key));
    if result.is_ok() {
        let raw = std::fs::read_to_string(&path)
            .map_err(|error| super::io_fail("could not read", &path, error))?;
        let result_doc: DocumentMut = raw
            .parse()
            .map_err(|_| Fail::new(1, "mise wrote invalid tool declarations"))?;
        let result_table = edit::table(&result_doc, TOOLS)?
            .ok_or_else(|| Fail::new(1, "mise did not record the requested tool"))?;
        let (key, native_value) = result_table
            .iter()
            .find(|(key, _)| *key == tool.key)
            .or_else(|| {
                let mut candidates = result_table.iter().filter(|(key, _)| *key != tool.key);
                let first = candidates.next()?;
                candidates.next().is_none().then_some(first)
            })
            .ok_or_else(|| Fail::new(1, "mise did not identify the requested tool"))?;
        let recorded = if previous.is_none() && tool.options.is_empty() {
            native_value.clone()
        } else {
            declaration
        };
        edit::put(&mut edit.doc.document, TOOLS, key, recorded)?;
        edit.install()?;
        Ok(())
    } else {
        finish_install(run, edit, TOOLS, &tool.key, declaration, None, result)
    }
}

fn finish_install(
    run: &Run<'_>,
    mut edit: Edit,
    section: &[&str],
    key: &str,
    declaration: Item,
    replace: Option<&str>,
    result: Result<(), Fail>,
) -> Result<(), Fail> {
    if let Err(error) = result {
        if !run.cancelled() && ask_to_record(&edit.path, section, key, &declaration)? {
            edit::put(&mut edit.doc.document, section, key, declaration)?;
            edit.install()?;
        }
        return Err(error);
    }
    if let Some(old) = replace {
        edit::delete(&mut edit.doc.document, section, old)?;
    }
    edit::put(&mut edit.doc.document, section, key, declaration)?;
    if edit.install()?.is_some() && section == PACKAGES {
        println!("Recorded {key}");
    }
    Ok(())
}

fn ask_to_record(
    path: &Path,
    section: &[&str],
    key: &str,
    declaration: &Item,
) -> Result<bool, Fail> {
    let mut pending = DocumentMut::new();
    edit::put(&mut pending, section, key, declaration.clone())?;
    eprintln!(
        "Install failed. Declaration for {}:\n{pending}",
        path.display()
    );
    if !io::stdin().is_terminal() {
        eprintln!("dots: request was not recorded (noninteractive run)");
        return Ok(false);
    }
    eprint!("Record it for a later retry? [y/N] ");
    io::stderr()
        .flush()
        .map_err(|error| Fail::new(1, error.to_string()))?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|error| Fail::new(1, error.to_string()))?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

fn verify_manager_enabled(mise: &Mise<'_>, manager: &str) -> Result<(), Fail> {
    let settings = mise.json(["settings", "ls", "--all", "--json", "system_packages"])?;
    if let Some(allowed) = settings["system_packages"]["managers"].as_array()
        && !allowed.iter().any(|value| value.as_str() == Some(manager))
    {
        return Err(Fail::new(
            1,
            format!("{manager} is disabled by mise's system_packages.managers"),
        ));
    }
    Ok(())
}

struct NativeInstall<'a> {
    mise: &'a Mise<'a>,
    run: &'a Run<'a>,
    dir: &'a Path,
    manager: &'a str,
    name: &'a str,
}

impl NativeInstall<'_> {
    fn verify(&self, installed: bool) -> Result<(), Fail> {
        let Self {
            mise,
            run,
            dir,
            manager,
            name,
        } = *self;
        let mut command = isolated(mise, dir, ["bootstrap", "packages", "status", "--json"])?;
        // The preflight needs only host-filtered declarations. Only the
        // post-install check asks the selected manager for installed state.
        command.env(
            "MISE_SYSTEM_PACKAGES_MANAGERS",
            if installed { manager } else { "" },
        );
        let failure = || {
            let reason = if installed {
                "did not confirm installation of"
            } else {
                "did not select this host for"
            };
            Fail::new(1, format!("mise {reason} {manager}:{name}"))
        };
        let output = run.probe(command).map_err(|_| failure())?;
        if !output.status.success() {
            return Err(failure());
        }
        let inventory: serde_json::Value =
            serde_json::from_slice(&output.stdout).map_err(|_| failure())?;
        let status = &inventory[manager];
        let found = status["packages"].as_array().is_some_and(|rows| {
            rows.iter().any(|row| {
                row["package"].as_str() == Some(name)
                    && (!installed || row["state"].as_str() == Some("installed"))
            })
        });
        if found && (!installed || status["available"].as_bool() == Some(true)) {
            Ok(())
        } else {
            Err(failure())
        }
    }
}

fn package_value(doc: &DocumentMut, key: &str, version: Option<&str>) -> Result<Item, Fail> {
    let previous = edit::entry(doc, PACKAGES, key)?;
    if let Some(previous) = previous {
        return Ok(previous.clone());
    }
    if matches!(native::split(key)?.0, "brew" | "brew-cask" | "mas") {
        let mut opts = InlineTable::new();
        opts.insert("os", Value::from("macos"));
        return Ok(value(opts));
    }
    update_tool(previous, version.unwrap_or("latest"), &[])
}

fn package_document(
    ctx: &Ctx,
    selected: &DocumentMut,
    key: &str,
    declaration: Item,
) -> Result<DocumentMut, Fail> {
    let mut staged = DocumentMut::new();
    if matches!(native::split(key)?.0, "brew" | "brew-cask") {
        let base = Edit::open(&ctx.root.path().join("config.toml"))?;
        for doc in [&base.doc.document, selected] {
            if let Some(taps) = edit::table(doc, &["bootstrap", "brew", "taps"])? {
                for (name, tap) in taps.iter() {
                    edit::put(
                        &mut staged,
                        &["bootstrap", "brew", "taps"],
                        name,
                        tap.clone(),
                    )?;
                }
            }
        }
    }
    edit::put(&mut staged, PACKAGES, key, declaration)?;
    Ok(staged)
}

fn write_staged(dir: &Path, doc: &DocumentMut) -> Result<(), Fail> {
    let path = dir.join("mise.toml");
    std::fs::write(&path, doc.to_string())
        .map_err(|error| super::io_fail("could not write", &path, error))
}

pub(super) fn trust_stage(ctx: &Ctx, command: &mut Command, dir: &Path) -> Result<(), Fail> {
    let parents = ctx
        .env
        .get("MISE_TRUSTED_CONFIG_PATHS")
        .into_iter()
        .flat_map(std::env::split_paths);
    let trusted = std::env::join_paths(std::iter::once(dir.to_path_buf()).chain(parents))
        .map_err(|error| Fail::usage(format!("could not trust the staged mise file: {error}")))?;
    command.env("MISE_TRUSTED_CONFIG_PATHS", trusted);
    Ok(())
}

fn isolated<const N: usize>(mise: &Mise<'_>, dir: &Path, args: [&str; N]) -> Result<Command, Fail> {
    let mut command = mise.command(args)?;
    command
        .current_dir(dir)
        .env("MISE_CONFIG_DIR", dir)
        .env("MISE_TRUSTED_CONFIG_PATHS", dir)
        .env("MISE_CEILING_PATHS", dir)
        .env("MISE_ENV", "");
    Ok(command)
}

fn formula_absent(name: &str, output: &str) -> bool {
    if let Some((_, short)) = name.rsplit_once('/') {
        output.contains(&format!(
            "mise ERROR tap has no formula named '{short}' in "
        ))
    } else {
        output.lines().any(|line| {
            line == format!("mise ERROR HTTP status client error (404 Not Found) for url (https://formulae.brew.sh/api/formula/{name}.json)")
        })
    }
}

struct ToolRequest {
    key: String,
    version: String,
    options: Vec<(String, Value)>,
}

impl ToolRequest {
    fn parse(request: &str) -> Result<Self, Fail> {
        let bad = || {
            Fail::usage(
                "invalid tool request; use NAME[@VERSION] with optional [key=value] options",
            )
        };
        let mut base = String::new();
        let mut options = Vec::new();
        let mut rest = request;
        while let Some((head, tail)) = rest.split_once('[') {
            base.push_str(head);
            let mut start = 0;
            let mut depth = 0_u32;
            let mut quote = None;
            let mut escaped = false;
            let mut end = None;
            for (index, ch) in tail.char_indices() {
                if let Some(q) = quote {
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' && q == '"' {
                        escaped = true;
                    } else if ch == q {
                        quote = None;
                    }
                    continue;
                }
                match ch {
                    '"' | '\'' => quote = Some(ch),
                    '[' | '{' | '(' => depth += 1,
                    ']' if depth == 0 => {
                        end = Some(index);
                        parse_option(&tail[start..index], &mut options)?;
                        break;
                    }
                    ']' | '}' | ')' => depth = depth.checked_sub(1).ok_or_else(bad)?,
                    ',' if depth == 0 => {
                        parse_option(&tail[start..index], &mut options)?;
                        start = index + 1;
                    }
                    _ => {}
                }
            }
            rest = &tail[end.ok_or_else(bad)? + 1..];
        }
        base.push_str(rest);
        let (key, version) = match base.rsplit_once('@') {
            Some(("npm:", _)) | None => (base.as_str(), "latest"),
            Some(pair) => pair,
        };
        if key.is_empty()
            || key.ends_with(':')
            || version.is_empty()
            || base.contains(char::is_whitespace)
        {
            return Err(bad());
        }
        Ok(Self {
            key: key.into(),
            version: version.into(),
            options,
        })
    }

    fn declaration(&self, previous: Option<&Item>) -> Result<Item, Fail> {
        update_tool(previous, &self.version, &self.options)
    }
}

fn parse_option(option: &str, options: &mut Vec<(String, Value)>) -> Result<(), Fail> {
    let (name, raw) = option
        .split_once('=')
        .ok_or_else(|| Fail::usage("tool options need key=value"))?;
    let (name, raw) = (name.trim(), raw.trim());
    if name.is_empty()
        || name == "version"
        || raw.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        || options.iter().any(|(key, _)| key == name)
    {
        return Err(Fail::usage("invalid or duplicate tool option"));
    }
    let value = match raw.parse::<Value>() {
        Ok(value) => value,
        Err(_) if raw.starts_with(['"', '\'']) => {
            return Err(Fail::usage("invalid quoted tool option"));
        }
        Err(_) => Value::from(raw),
    };
    options.push((name.into(), value));
    Ok(())
}

fn update_tool(
    previous: Option<&Item>,
    version: &str,
    options: &[(String, Value)],
) -> Result<Item, Fail> {
    fn update(table: &mut dyn toml_edit::TableLike, version: &str, options: &[(String, Value)]) {
        let values = std::iter::once(("version", Value::from(version))).chain(
            options
                .iter()
                .map(|(key, value)| (key.as_str(), value.clone())),
        );
        for (key, mut value) in values {
            if let Some(old) = table.get(key).and_then(Item::as_value) {
                *value.decor_mut() = old.decor().clone();
            }
            if let Some(slot) = table.get_mut(key) {
                *slot = Item::Value(value);
            } else {
                table.insert(key, Item::Value(value));
            }
        }
    }

    let mut item = previous.cloned().unwrap_or_else(|| value(version));
    if let Some(table) = item.as_table_like_mut() {
        update(table, version, options);
    } else if let Some(versions) = item.as_array_mut() {
        if let Some(table) = versions
            .iter_mut()
            .filter_map(Value::as_inline_table_mut)
            .find(|table| table.get("version").and_then(Value::as_str) == Some(version))
        {
            update(table, version, options);
        } else {
            let mut table = InlineTable::new();
            update(&mut table, version, options);
            versions.push(table);
        }
    } else if options.is_empty() {
        item = value(version);
    } else {
        let mut table = InlineTable::new();
        update(&mut table, version, options);
        item = value(table);
    }
    Ok(item)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_and_inline_options_keep_native_types() {
        let request = ToolRequest::parse(
            r#"npm:@scope/tool@4[flag=true, csv="one,two", tags=["a","b"]][features=full]"#,
        )
        .unwrap();
        assert_eq!(request.key, "npm:@scope/tool");
        assert_eq!(request.version, "4");
        let old: Value = r#"{ version = "3", keep = [1, 2], csv = "old" }"#.parse().unwrap();
        let result = request.declaration(Some(&Item::Value(old))).unwrap();
        let table = result.as_inline_table().unwrap();
        assert_eq!(table.get("version").and_then(Value::as_str), Some("4"));
        assert_eq!(table.get("csv").and_then(Value::as_str), Some("one,two"));
        assert_eq!(table.get("flag").and_then(Value::as_bool), Some(true));
        assert_eq!(
            table.get("tags").and_then(Value::as_array).unwrap().len(),
            2
        );
        assert_eq!(
            table.get("keep").and_then(Value::as_array).unwrap().len(),
            2
        );
        assert_eq!(table.get("features").and_then(Value::as_str), Some("full"));
        let scoped = ToolRequest::parse("npm:@scope/tool").unwrap();
        assert_eq!(scoped.key, "npm:@scope/tool");
        assert_eq!(scoped.version, "latest");
    }
}
