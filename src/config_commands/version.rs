use std::path::{Path, PathBuf};

use crate::{Env, Fail};

pub fn version(env: &Env) -> Result<String, Fail> {
    let executable = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|error| {
            Fail::new(
                1,
                format!("could not locate the running dots executable: {error}"),
            )
        })?;
    Ok(format!(
        "dots {} ({})",
        env!("CARGO_PKG_VERSION"),
        installer(&executable, env)
    ))
}

pub(crate) fn installer(exe: &Path, env: &Env) -> &'static str {
    let components: Vec<_> = exe.iter().collect();
    if components.windows(5).any(|part| {
        part[0] == "Cellar" && part[1] == "dots" && part[3] == "bin" && part[4] == "dots"
    }) {
        return "homebrew";
    }

    let mise_data = env
        .get("MISE_DATA_DIR")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env.get("XDG_DATA_HOME")
                .filter(|path| !path.is_empty())
                .map(|path| Path::new(path).join("mise"))
        })
        .or_else(|| env.home().map(|home| home.join(".local/share/mise")));
    let in_mise_data = mise_data
        .and_then(|dir| dir.join("installs").canonicalize().ok())
        .is_some_and(|dir| exe.starts_with(dir));
    let in_mise_layout = components
        .windows(2)
        .any(|part| part == ["mise", "installs"]);
    if in_mise_data || in_mise_layout {
        return "mise";
    }

    if let Some(bin) = exe.parent()
        && bin.file_name().is_some_and(|name| name == "bin")
    {
        let cargo_home = env
            .get("CARGO_HOME")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .or_else(|| env.home().map(|home| home.join(".cargo")));
        let cargo_roots = cargo_home
            .into_iter()
            .chain(env.get("CARGO_INSTALL_ROOT").map(PathBuf::from));
        if cargo_roots
            .filter_map(|dir| env.cwd().join(dir).join("bin").canonicalize().ok())
            .any(|dir| dir == bin)
            || bin
                .parent()
                .is_some_and(|dir| dir.join(".crates2.json").is_file())
        {
            return "cargo";
        }
    }
    "other"
}
