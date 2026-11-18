mod app;
mod archive;
mod release;

use glob::Pattern;
use std::fmt::Write;
use toml_edit::{Array, InlineTable, Item, Value};

use crate::{Ctx, Fail, run::Run, targets::github_repo as repo_name};

pub use app::{install_release_app, validate_owned_app};

pub fn resolve(
    ctx: &Ctx,
    run: &Run<'_>,
    request: &str,
    asset: Option<&str>,
) -> Result<(String, Item), Fail> {
    let repo = request
        .strip_prefix("https://github.com/")
        .and_then(repo_name)
        .ok_or_else(|| {
            failure("use an HTTPS GitHub repository URL: https://github.com/OWNER/REPO")
        })?;
    let override_pattern = asset
        .map(Pattern::new)
        .transpose()
        .map_err(|err| Fail::usage(format!("invalid --asset glob: {err}")))?;
    let platform = Platform {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
    };
    let github = release::Client::new(ctx);
    let (repo, release) = github.latest(&repo, run)?;
    let selected = release.select(override_pattern.as_ref(), platform)?;
    let format = archive::Format::detect(&selected.name)?;
    let contains_app = if let Some(format) = format {
        let file = github.download(&repo, selected, run)?;
        archive::contains_app(file, format)?
    } else {
        false
    };

    let postinstall = if contains_app {
        if platform.os != "macos" {
            return Err(failure(
                "this release contains a macOS app; install it on a Mac",
            ));
        }
        let exe =
            std::env::current_exe().map_err(|err| failure(format!("cannot locate dots: {err}")))?;
        Some(app_hook(&exe, &repo)?)
    } else {
        None
    };
    Ok((
        format!("github:{repo}"),
        release.declaration(selected, asset, platform, postinstall),
    ))
}

pub(super) fn failure(diagnostic: impl Into<String>) -> Fail {
    Fail::new(1, diagnostic)
}

fn digest_hex(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(result, "{byte:02x}").expect("writing to a string is infallible");
    }
    result
}

#[derive(Clone, Copy)]
struct Platform {
    os: &'static str,
    arch: &'static str,
}

impl Platform {
    fn asset_option(self) -> String {
        let arch = match self.arch {
            "aarch64" => "arm64",
            "x86_64" => "x64",
            other => other,
        };
        format!("platform_{}_{arch}_asset_pattern", self.os)
    }
}

fn tool_options(
    pattern: &str,
    version_prefix: &str,
    platform_specific: bool,
    platform: Platform,
    postinstall: Option<String>,
) -> Item {
    let mut tool = InlineTable::new();
    tool.insert("version", Value::from("latest"));
    let key = if platform_specific || postinstall.is_some() {
        platform.asset_option()
    } else {
        String::from("asset_pattern")
    };
    tool.insert(&key, Value::from(pattern));
    if !matches!(version_prefix, "" | "v") {
        tool.insert("version_prefix", Value::from(version_prefix));
    }
    if let Some(command) = postinstall {
        tool.insert("strip_components", Value::from(0));
        tool.insert("bin_path", Value::from(".dots-no-bin"));
        let mut os = Array::new();
        os.push("macos");
        tool.insert("os", Value::Array(os));
        let mut hook = InlineTable::new();
        hook.insert("run", Value::from(command));
        hook.insert("when", Value::from("always"));
        tool.insert("postinstall", Value::InlineTable(hook));
    }
    Item::Value(Value::InlineTable(tool))
}

fn app_hook(exe: &std::path::Path, repo: &str) -> Result<String, Fail> {
    let command = if let Some(cellar) = exe.ancestors().find(|path| path.ends_with("Cellar/dots")) {
        let prefix = cellar
            .parent()
            .and_then(std::path::Path::parent)
            .ok_or_else(|| failure("cannot locate the Homebrew prefix for dots"))?;
        shell_path(&prefix.join("bin/dots"))?
    } else if exe.ancestors().any(|path| {
        path.ends_with("installs/github-ezra-cerpac-dots") || path.ends_with("installs/dots")
    }) {
        r#""${DOTS_MISE_BIN:-$(command -v mise || printf '%s\n' "$HOME/.local/bin/mise")}" x github:EzraCerpac/dots -- dots"#.to_owned()
    } else {
        shell_path(exe)?
    };
    Ok(format!("{command} install-release-app {repo}"))
}

fn shell_path(path: &std::path::Path) -> Result<String, Fail> {
    let path = path.to_str().ok_or_else(|| {
        failure("the dots executable path must be UTF-8 to record a release app hook")
    })?;
    // mise templates the hook before invoking the shell.
    if path.contains("{{") || path.contains("{%") || path.contains("{#") {
        return Err(failure(
            "the dots executable path contains mise template syntax",
        ));
    }
    Ok(format!("'{}'", path.replace('\'', "'\\''")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::Path,
        process::Command,
    };

    #[test]
    fn validates_and_canonicalizes_repository_operands() {
        assert_eq!(
            repo_name("Owner/Tool.name").as_deref(),
            Some("owner/tool.name")
        );
        for invalid in [
            "owner",
            "owner/../tool",
            "-owner/tool",
            "owner/.",
            "owner/app?x=y",
        ] {
            assert_eq!(repo_name(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn generates_stable_paths_for_each_installation_kind() {
        let repo = "owner/tool";
        for (exe, command) in [
            ("/work/dots/bin/dots", "'/work/dots/bin/dots'"),
            ("/opt/a'b c/dots", "'/opt/a'\\''b c/dots'"),
            ("/brew/Cellar/dots/1.0/bin/dots", "'/brew/bin/dots'"),
            (
                "/mise/installs/github-ezra-cerpac-dots/1.0/dots",
                r#""${DOTS_MISE_BIN:-$(command -v mise || printf '%s\n' "$HOME/.local/bin/mise")}" x github:EzraCerpac/dots -- dots"#,
            ),
        ] {
            assert_eq!(
                app_hook(Path::new(exe), repo).unwrap(),
                format!("{command} install-release-app {repo}")
            );
        }
    }

    #[test]
    fn homebrew_hook_uses_prefix_link_with_no_dots_on_path() {
        let fixture = tempfile::tempdir().unwrap();
        let version = fixture.path().join("Cellar/dots/1.0/bin");
        fs::create_dir_all(&version).unwrap();
        fs::create_dir(fixture.path().join("bin")).unwrap();
        let exe = version.join("dots");
        fs::write(&exe, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&exe, fixture.path().join("bin/dots")).unwrap();
        let result = Command::new("/bin/sh")
            .args(["-c", &app_hook(&exe, "owner/tool").unwrap()])
            .env_clear()
            .env("PATH", "")
            .output()
            .unwrap();
        assert!(result.status.success());
        assert_eq!(result.stdout, b"install-release-app\nowner/tool\n");
    }
}
