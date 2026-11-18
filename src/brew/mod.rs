use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::process::Cwd;
use crate::{Args, Ctx, Fail};

#[derive(Clone, Copy)]
enum Kind {
    Formula,
    Cask,
}

impl Kind {
    fn from_args(args: &Args) -> Self {
        match args.get_one::<String>("KIND").map(String::as_str) {
            Some("formula") => Self::Formula,
            Some("cask") => Self::Cask,
            _ => unreachable!("selected by the Brew CLI"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Formula => "formula",
            Self::Cask => "cask",
        }
    }

    fn flag(self) -> &'static str {
        match self {
            Self::Formula => "--formula",
            Self::Cask => "--cask",
        }
    }
}

pub fn brew(ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    let (operation, args) = args.subcommand().expect("required by the Brew CLI");
    match operation {
        "path" => {
            let Some(brew) = Brew::find(ctx)? else {
                return Ok(1);
            };
            let mut stdout = io::stdout().lock();
            stdout
                .write_all(brew.path.as_os_str().as_encoded_bytes())
                .and_then(|()| stdout.write_all(b"\n"))
                .map_err(|error| Fail::new(1, format!("could not write Homebrew path: {error}")))?;
            Ok(0)
        }
        "admin-available" => Ok(u8::from(!admin_available(ctx)?)),
        "defer-for-admin" => Ok(defer_for_admin(token(args)?)),
        "version" => {
            let token = token(args)?;
            let Some(brew) = Brew::find(ctx)? else {
                return Ok(1);
            };
            let Some(version) = brew.version(Kind::from_args(args), token)? else {
                return Ok(1);
            };
            println!("{version}");
            Ok(0)
        }
        "record" => {
            let token = token(args)?;
            let change = match args.get_one::<String>("ACTION").map(String::as_str) {
                Some("installed") => Change::Installed,
                Some("updated") => Change::Updated(text_arg(args, "BEFORE")?),
                _ => unreachable!("selected by the Brew CLI"),
            };
            if changes_path(ctx).is_some()
                && let Some(brew) = Brew::find(ctx)?
            {
                brew.record(Kind::from_args(args), token, change)?;
            }
            Ok(0)
        }
        "update-cask" | "update-formula" | "hold-cask" | "hold-formula" => {
            let token = token(args)?;
            if !cfg!(target_os = "macos") {
                println!("Skipped: {token} is macOS-only.");
                return Ok(4);
            }
            match operation {
                "update-cask" => update(
                    ctx,
                    Kind::Cask,
                    token,
                    args.get_flag("install-only"),
                    args.get_flag("auto-updates"),
                    args.get_flag("admin"),
                    text_arg(args, "PROCESS")?.filter(|name| !name.is_empty()),
                ),
                "update-formula" => update(
                    ctx,
                    Kind::Formula,
                    token,
                    args.get_flag("install-only"),
                    false,
                    false,
                    None,
                ),
                _ => hold(ctx, operation, args, token),
            }
        }
        _ => unreachable!("selected by the Brew CLI"),
    }
}

fn text_arg<'a>(args: &'a Args, key: &str) -> Result<Option<&'a str>, Fail> {
    args.get_one::<OsString>(key)
        .map(|value| {
            value
                .to_str()
                .ok_or_else(|| Fail::usage(format!("{key} must be valid UTF-8")))
        })
        .transpose()
}

fn token(args: &Args) -> Result<&str, Fail> {
    let token = text_arg(args, "TOKEN")?.expect("required by the Brew CLI");
    if named_package(token) {
        Ok(token)
    } else {
        Err(Fail::usage("TOKEN must name one Homebrew package"))
    }
}

fn named_package(token: &str) -> bool {
    // Accept a bare token or a tap-qualified owner/tap/token, never an option or path.
    matches!(token.bytes().filter(|&byte| byte == b'/').count(), 0 | 2)
        && token.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('-')
                && !matches!(part, "." | "..")
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"@._+-".contains(&byte))
        })
}

struct Brew<'a> {
    ctx: &'a Ctx,
    path: PathBuf,
}

impl<'a> Brew<'a> {
    fn find(ctx: &'a Ctx) -> Result<Option<Self>, Fail> {
        let on_path = ctx.env.get("PATH").and_then(|path| {
            env::split_paths(path)
                .map(|dir| ctx.env.cwd().join(dir).join("brew"))
                .find(|path| {
                    fs::metadata(path).is_ok_and(|metadata| {
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    })
                })
        });
        let defaults = ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"];
        for path in on_path.into_iter().chain(defaults.map(PathBuf::from)) {
            let brew = Self { ctx, path };
            let output = brew.command()?.arg("--prefix").output();
            if output.is_ok_and(|output| output.status.success() && standard_prefix(&output.stdout))
            {
                return Ok(Some(brew));
            }
        }
        Ok(None)
    }

    fn command(&self) -> Result<Command, Fail> {
        let mut command = recipe_command(self.ctx, &self.path)?;
        // Keep every Homebrew invocation scoped, including any implicit work inside install.
        command
            .env("HOMEBREW_NO_AUTO_UPDATE", "1")
            .env("HOMEBREW_NO_AUTOREMOVE", "1")
            .env("HOMEBREW_NO_INSTALL_CLEANUP", "1");
        Ok(command)
    }

    fn installed(&self, kind: Kind, token: &str) -> Result<bool, Fail> {
        let status = self
            .command()?
            .args(["list", kind.flag(), token])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| {
                Fail::new(
                    1,
                    format!("could not inspect {token} with Homebrew: {error}"),
                )
            })?;
        match status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(Fail::new(
                1,
                format!("Homebrew could not inspect {token} ({status})"),
            )),
        }
    }

    fn version(&self, kind: Kind, token: &str) -> Result<Option<String>, Fail> {
        let output = self
            .command()?
            .args(["list", "--versions", kind.flag(), token])
            .output()
            .map_err(|error| {
                Fail::new(
                    1,
                    format!("could not read Homebrew version for {token}: {error}"),
                )
            })?;
        Ok(output
            .status
            .success()
            .then(|| listed_version(&output.stdout))
            .flatten()
            .map(str::to_owned))
    }

    fn outdated(&self, kind: Kind, token: &str, greedy: bool) -> Result<bool, Fail> {
        let mut command = self.command()?;
        command.args(["outdated", kind.flag(), "--quiet"]);
        if greedy {
            command.arg("--greedy");
        }
        let output = command
            .arg(token)
            .stderr(Stdio::inherit())
            .output()
            .map_err(|error| {
                Fail::new(
                    1,
                    format!("could not check Homebrew updates for {token}: {error}"),
                )
            })?;
        let found = output.stdout.iter().any(|byte| !byte.is_ascii_whitespace());
        // Homebrew can report a named outdated package with status 1 and stdout.
        if output.status.success() || (output.status.code() == Some(1) && found) {
            Ok(found)
        } else {
            Err(Fail::new(
                1,
                format!(
                    "Homebrew could not check updates for {token} ({})",
                    output.status
                ),
            ))
        }
    }

    fn record(&self, kind: Kind, token: &str, change: Change<'_>) -> Result<(), Fail> {
        let Some(path) = changes_path(self.ctx) else {
            return Ok(());
        };
        if !self.installed(kind, token)? {
            return Ok(());
        }
        let after = self.version(kind, token)?;
        let before = match change {
            Change::Installed => "not installed",
            Change::Updated(Some(before))
                if known_version(before)
                    && after.as_deref().is_some_and(|after| before != after) =>
            {
                before
            }
            Change::Updated(_) => return Ok(()),
        };
        let row = format!(
            "brew-{}:{token}\t{before}\t{}\n",
            kind.name(),
            after.as_deref().unwrap_or_default()
        );
        OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(path)
            .and_then(|mut file| file.write_all(row.as_bytes()))
            .map_err(|error| {
                Fail::new(
                    1,
                    format!(
                        "could not record the Homebrew change for {token} in DOTS_CHANGES: {error}"
                    ),
                )
            })
    }
}

fn recipe_command(ctx: &Ctx, program: impl AsRef<OsStr>) -> Result<Command, Fail> {
    // Brew needs no configuration. A recipe may already have a selected profile.
    let profile = ctx.env.get("DOTS_PROFILE").unwrap_or_default();
    ctx.command_with_profile(program, Cwd::Caller, profile)
}

fn standard_prefix(output: &[u8]) -> bool {
    std::str::from_utf8(output).is_ok_and(|prefix| {
        matches!(
            prefix.trim_end_matches('\n'),
            "/opt/homebrew" | "/usr/local"
        )
    })
}

fn listed_version(output: &[u8]) -> Option<&str> {
    let (name, version) = std::str::from_utf8(output)
        .ok()?
        .trim_end_matches('\n')
        .split_once(' ')?;
    let version = version.trim_matches(' ');
    (named_package(name) && known_version(version)).then_some(version)
}

fn known_version(version: &str) -> bool {
    !version.trim().is_empty() && !version.chars().any(char::is_control)
}

fn changes_path(ctx: &Ctx) -> Option<&Path> {
    ctx.env
        .get("DOTS_CHANGES")
        .filter(|path| !path.is_empty())
        .map(Path::new)
}

enum Change<'a> {
    Installed,
    Updated(Option<&'a str>),
}

fn update(
    ctx: &Ctx,
    kind: Kind,
    token: &str,
    install_only: bool,
    greedy: bool,
    admin: bool,
    process: Option<&str>,
) -> Result<u8, Fail> {
    let brew = Brew::find(ctx)?.ok_or_else(|| {
        Fail::new(
            1,
            format!("a standard-prefix Homebrew installation is required for {token}"),
        )
    })?;
    let installed = brew.installed(kind, token)?;
    let (action, activity) = if installed {
        if install_only {
            println!("Already installed: {token}");
            return Ok(0);
        }
        if !brew.outdated(kind, token, greedy)? {
            println!("Unchanged: {token}");
            return Ok(0);
        }
        ("upgrade", "upgrading")
    } else {
        ("install", "installing")
    };

    if let Some(name) = process
        && process_running(ctx, name)?
    {
        println!("Deferred: {name} is running. Close it before {activity} {token}.");
        return Ok(3);
    }
    if admin && !admin_available(ctx)? {
        return Ok(defer_for_admin(token));
    }

    let before = if installed && changes_path(ctx).is_some() {
        brew.version(kind, token)?
    } else {
        None
    };
    let mut command = brew.command()?;
    command.args([action, kind.flag()]);
    if installed && greedy {
        command.arg("--greedy");
    }
    let status = command.arg(token).status().map_err(|error| {
        Fail::new(
            1,
            format!("could not {action} {token} with Homebrew: {error}"),
        )
    })?;
    if !status.success() {
        return Err(Fail::new(
            1,
            format!("Homebrew could not {action} {token} ({status})"),
        ));
    }

    let change = if installed {
        Change::Updated(before.as_deref())
    } else {
        Change::Installed
    };
    brew.record(kind, token, change)?;
    Ok(0)
}

fn process_running(ctx: &Ctx, name: &str) -> Result<bool, Fail> {
    let status = recipe_command(ctx, "pgrep")?
        .args(["-x", "--", &literal_pattern(name)])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .map_err(|error| {
            Fail::new(
                1,
                format!("could not check whether {name} is running: {error}"),
            )
        })?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(Fail::new(
            1,
            format!("could not check whether {name} is running ({status})"),
        )),
    }
}

fn literal_pattern(name: &str) -> String {
    let mut pattern = String::new();
    for character in name.chars() {
        if "\\.^$|?*+()[]{}".contains(character) {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    pattern
}

fn admin_available(ctx: &Ctx) -> Result<bool, Fail> {
    if nix::unistd::geteuid().is_root() || (io::stdin().is_terminal() && io::stdout().is_terminal())
    {
        return Ok(true);
    }
    Ok(recipe_command(ctx, "sudo")?
        .args(["-n", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success()))
}

fn defer_for_admin(token: &str) -> u8 {
    println!(
        "Deferred: {token} needs interactive administrator access. Run sudo -v && dots up in a terminal."
    );
    3
}

fn hold(ctx: &Ctx, operation: &str, args: &Args, token: &str) -> Result<u8, Fail> {
    let required = text_arg(args, "VERSION")?
        .filter(|version| known_version(version))
        .ok_or_else(|| Fail::usage("VERSION must be a nonempty version"))?;
    let reason = text_arg(args, "REASON")?
        .filter(|reason| !reason.is_empty())
        .unwrap_or("held version");

    let formula = operation == "hold-formula";
    let installed = if formula {
        let brew = Brew::find(ctx)?.ok_or_else(|| {
            Fail::new(1, format!("a standard-prefix Homebrew installation is required to inspect held formula {token}"))
        })?;
        brew.version(Kind::Formula, token)?
    } else {
        let app = args
            .get_one::<OsString>("APP_PATH")
            .expect("required by the Brew CLI");
        bundle_version(ctx, Path::new(app))?
    };
    match installed.as_deref() {
        Some(versions) if formula && versions.chars().any(char::is_whitespace) => Err(Fail::new(
            1,
            format!(
                "Held: {token} has multiple installed versions ({versions}). Refusing to change it."
            ),
        )),
        Some(version) if version == required => {
            println!("Held: {token} {required} ({reason}).");
            Ok(0)
        }
        Some(version) => Err(Fail::new(
            1,
            format!(
                "Held: {token} is {version}, but this setup requires {required}. Refusing to change it."
            ),
        )),
        None => Err(Fail::new(
            1,
            format!(
                "Manual action required: install {token} {required} ({reason}). Refusing an unverified Homebrew version."
            ),
        )),
    }
}

fn bundle_version(ctx: &Ctx, app: &Path) -> Result<Option<String>, Fail> {
    let plist = app.join("Contents/Info.plist");
    if !plist.is_file() {
        return Ok(None);
    }
    let output = recipe_command(ctx, "/usr/libexec/PlistBuddy")?
        .args(["-c", "Print :CFBundleShortVersionString"])
        .arg(plist)
        .output()
        .map_err(|error| Fail::new(1, format!("could not read the app bundle version: {error}")))?;
    let version = std::str::from_utf8(&output.stdout)
        .unwrap_or_default()
        .trim_end_matches('\n');
    Ok((output.status.success() && known_version(version)).then(|| version.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_standard_prefixes() {
        assert!(standard_prefix(b"/opt/homebrew\n"));
        assert!(standard_prefix(b"/usr/local\n"));
        assert!(!standard_prefix(b"/home/linuxbrew/.linuxbrew\n"));
        assert!(!standard_prefix(b"/usr/local/\n"));
        assert!(!standard_prefix(b"/usr/local\nwarning\n"));
    }

    #[test]
    fn parses_one_package_with_known_versions() {
        assert_eq!(listed_version(b"widget 1.2_1 1.3\n"), Some("1.2_1 1.3"));
        assert_eq!(
            listed_version(b"test/tap/widget 3.0,123\n"),
            Some("3.0,123")
        );
        assert_eq!(listed_version(b"widget\n"), None);
        assert_eq!(listed_version(b"widget  \n"), None);
        assert_eq!(listed_version(b"widget 1\nother 2\n"), None);
        assert_eq!(listed_version(b"widget 1\t2\n"), None);
    }

    #[test]
    fn restricts_brew_arguments_to_named_packages() {
        assert!(named_package("test/tap/widget+kit@1.2"));
        assert!(named_package("c++"));
        for bad in [
            "",
            "--greedy",
            "/tmp/widget.rb",
            "../../widget",
            "two tokens",
            "some\tthing",
        ] {
            assert!(!named_package(bad), "{bad:?}");
        }
    }

    #[test]
    fn escapes_process_names_for_exact_matching() {
        assert_eq!(
            literal_pattern("Widget.Helper+[2]"),
            r"Widget\.Helper\+\[2\]"
        );
        assert_eq!(literal_pattern("Widget Helper"), "Widget Helper");
    }
}
