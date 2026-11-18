use std::ffi::{OsStr, OsString};
use std::io::{self, Write as _};

use clap::builder::{OsStringValueParser, PossibleValue, PossibleValuesParser};
use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Arg, ArgAction, ArgGroup, ArgMatches, ColorChoice, Command, ValueHint};

use crate::completion::Shell;
use crate::config::Root;
use crate::config_commands::Invocation;
use crate::{Ctx, Env, Fail, Flags, bootstrap, brew, completion, config_commands, exceptions};
use crate::{history, init, inspect, packages, publish, receipt, skill, sync, ui, update};

const HELP: &str = include_str!("cli-help.txt");

const PARTS: [&str; 18] = [
    "plugins",
    "packages",
    "accounts",
    "files",
    "services",
    "firewall",
    "compose",
    "repos",
    "dotfiles",
    "mise-shell-activate",
    "macos-defaults",
    "settings",
    "macos-launchd-agents",
    "linux-systemd-units",
    "user",
    "tools",
    "task",
    "final-hook",
];

fn flag(name: &'static str, help: &'static str) -> Arg {
    Arg::new(name)
        .long(name)
        .action(ArgAction::SetTrue)
        .help(help)
}

fn value(name: &'static str, label: &'static str, help: &'static str) -> Arg {
    Arg::new(name)
        .long(name)
        .value_name(label)
        .help(help)
        .value_parser(clap::builder::NonEmptyStringValueParser::new())
}

fn file(name: &'static str, label: &'static str, help: &'static str) -> Arg {
    value(name, label, help)
        .value_hint(ValueHint::FilePath)
        .allow_hyphen_values(true)
        .value_parser(OsStringValueParser::new())
}

fn positional(name: &'static str) -> Arg {
    Arg::new(name).value_parser(OsStringValueParser::new())
}

fn positionals(name: &'static str) -> Arg {
    positional(name).num_args(0..).action(ArgAction::Append)
}

fn required(name: &'static str) -> Arg {
    positional(name).required(true)
}

fn globals(command: Command) -> Command {
    command
        .arg(flag("plain", "Print concise progress lines").conflicts_with("verbose"))
        .arg(flag("verbose", "Show full output of every step"))
}

fn sub(name: &'static str, about: &'static str) -> Command {
    Command::new(name).about(about)
}

fn scopes(
    command: Command,
    base: &'static str,
    profile: &'static str,
    path: &'static str,
) -> Command {
    command
        .arg(flag("base", base))
        .arg(value("profile", "PROFILE", profile))
        .arg(file("path", "FILE", path))
        .group(ArgGroup::new("scope").args(["base", "profile", "path"]))
}

fn machine_id(id: &str) -> Result<String, &'static str> {
    let mut bytes = id.bytes();
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if bytes.next().is_some_and(alnum) && bytes.all(|b| alnum(b) || b == b'-') {
        Ok(id.to_owned())
    } else {
        Err("use lowercase letters, digits and -")
    }
}

fn profile() -> Arg {
    value(
        "profile",
        "PROFILE",
        "Profile to select, one of dots_profiles",
    )
}

fn machine() -> Arg {
    value("machine-id", "ID", "This machine's ID").value_parser(machine_id)
}

fn origin() -> Arg {
    value("origin", "URL", "Settings history origin to record")
}

fn recipient() -> Arg {
    value("recipient", "KEY", "age recipient, repeat for more").action(ArgAction::Append)
}

fn pass_through(name: &'static str, about: &'static str) -> Command {
    sub(name, about).disable_help_flag(true).arg(
        positionals("ARGS")
            .trailing_var_arg(true)
            .allow_hyphen_values(true),
    )
}

pub fn tree() -> Command {
    globals(Command::new("dots"))
        .about("dots keeps your machines in the state your mise configuration declares.")
        .override_help(HELP)
        .disable_help_subcommand(true)
        .allow_external_subcommands(true)
        .external_subcommand_value_parser(OsStringValueParser::new())
        .color(ColorChoice::Never)
        .arg(flag("version", "Print the version and how dots was installed"))
        .subcommand(sub("help", "List commands and usage"))
        .subcommand(sub("completions", "Print a shell completion script")
            .override_usage("dots completions bash|zsh|fish")
            .arg(Arg::new("SHELL").required(true).value_parser(["bash", "zsh", "fish"]))
            .after_help(concat!(
                "Add one line to your shell's startup file:\n",
                "  fish  ~/.config/fish/config.fish    dots completions fish | source\n",
                "  zsh   ~/.zshrc, after compinit      source <(dots completions zsh)\n",
                "  bash  ~/.bashrc                     eval \"$(dots completions bash)\"\n",
                "Homebrew installs the scripts in its standard completion locations."
            )))
        .subcommand(sub("reports", "List saved reports")
            .arg(Arg::new("COMMAND").value_parser(PossibleValuesParser::new(crate::report::Command::ALL.map(|command| {
                use crate::report::Command::{Remove, Up};
                let value = PossibleValue::new(command.name());
                match command {
                    Up => value.alias("update"),
                    Remove => value.alias("uninstall"),
                    _ => value,
                }
            })))))
        .subcommand(sub("status", "Show programs, profile and this machine's state"))
        .subcommand(sub("plan", "Preview mise's bootstrap plan for this machine"))
        .subcommand(globals(sub("up", "Checkpoint settings, sync, and update packages, tools, hooks and mise"))
            .visible_alias("update")
            .arg(flag("mas", "Include Mac App Store updates (macOS)").conflicts_with("last"))
            .arg(flag("last", "Print the last update report")))
        .subcommand(sub("sync", "Share configuration edits and apply others' changes")
            .arg(flag("last", "Replay the latest receipt without the network"))
            .after_help("A sync with valid arguments always exits 0."))
        .subcommand(sub("apply", "Apply declared dotfiles and templates")
            .arg(flag("force", "Overwrite conflicting files").short('f'))
            .arg(flag("dry-run", "Show what would change").short('n'))
            .arg(flag("yes", "Skip confirmation").short('y'))
            .arg(flag("prune", "Remove stale dotfiles"))
            .arg(flag("prompt-secrets", "Prompt for missing template secrets"))
            .arg(positionals("TARGET"))
            .after_help("Options are passed to mise."))
        .subcommand(pass_through("test", "Run the configuration's tests/run"))
        .subcommand(scopes(sub("add", "Install software or adopt a dotfile, and record it"),
            "Record in config.toml, for every profile", "Record in config.PROFILE.toml", "Record in FILE")
            .arg(value("asset", "GLOB", "Pick the release asset for one GitHub URL"))
            .arg(positionals("REQUEST").num_args(1..).required(true))
            .after_help(concat!(
                "A request starting with /, ~/, ./ or ../ is a file. Other requests name software,\n",
                "such as ripgrep, npm:prettier or a GitHub URL. Without a selector, add records\n",
                "in the selected profile's file.\n\n",
                "Examples:\n  dots add ripgrep\n  dots add npm:prettier cargo:hexyl\n",
                "  dots add brew-cask:firefox\n  dots add https://github.com/OWNER/NAME\n",
                "  dots add --base ~/.gitconfig"
            )))
        .subcommand(scopes(sub("remove", "Uninstall software or restore an adopted dotfile"),
            "Search config.toml", "Search config.PROFILE.toml", "Search FILE")
            .visible_alias("uninstall")
            .arg(flag("dry-run", "Print the plan and change nothing"))
            .arg(flag("keep-installed", "Delete only the declaration and keep the software"))
            .arg(positionals("TARGET").num_args(1..).required(true)))
        .subcommand(sub("init", "Create a configuration and its Git repository")
            .arg(positional("DIR").value_hint(ValueHint::DirPath))
            .after_help("DIR defaults to the configuration root. Existing files are kept."))
        .subcommand(bootstrap_tree())
        .subcommand(sub("remote", "Set up another machine over SSH")
            .arg(required("HOST"))
            .arg(value("profile", "PROFILE", "Profile for HOST").required(true))
            .arg(flag("dry-run", "Show the plan and change nothing"))
            .arg(flag("yes", "Skip confirmation"))
            .arg(file("identity-file", "FILE", "SSH identity file"))
            .arg(value("port", "N", "SSH port").value_parser(clap::value_parser!(u16).range(1..)))
            .arg(value("connect-timeout", "S", "SSH connection timeout in seconds")
                .value_parser(clap::value_parser!(u64)))
            .arg(value("github-relay-repo", "REPO", "GitHub relay repository"))
            .arg(flag("github-relay-read-only", "Use the GitHub relay read-only")))
        .subcommand(sub("bundle", "Encrypt or check private bootstrap inputs").subcommand_required(true)
            .subcommand(sub("create", "Encrypt private bootstrap inputs with age")
                .arg(file("input", "FILE", "Private inputs as JSON").required(true))
                .arg(file("output", "FILE", "New bundle, created with mode 0600").required(true))
                .arg(recipient().required(true)))
            .subcommand(sub("check", "Decrypt a bundle in memory and validate it")
                .arg(file("bundle", "FILE", "Encrypted bundle").required(true))
                .arg(file("identity", "KEY", "age identity").required(true))))
        .subcommand(sub("backup", "Save and publish encrypted app settings"))
        .subcommand(sub("restore", "Fetch and restore encrypted app settings")
            .arg(flag("initialize-history", "Replace a history store connected elsewhere and keep the old one")))
        .subcommand(sub("publish", "Open a pull request from a JJ workspace")
            .arg(flag("all", "Publish the whole current stack").conflicts_with("bookmark"))
            .arg(value("bookmark", "NAME", "Publish this wip/... bookmark"))
            .after_help("Without options, publish the one change above main@origin."))
        .subcommand(sub("root", "Print the physical configuration root"))
        .subcommand(sub("profile", "Print or select this machine's profile")
            .arg(profile().requires("machine-id"))
            .arg(machine().requires("profile"))
            .arg(origin().requires("profile")))
        .subcommand(pass_through("mise", "Run mise in the configuration's environment"))
        .subcommand(sub("retire-dotfiles", "Delete dangling links to retired dotfiles"))
        .subcommand(sub("exceptions", "Install missing installer exceptions"))
        .subcommand(sub("enroll-history", "Prepare this machine for encrypted settings history")
            .arg(profile().required(true)).arg(machine().required(true))
            .arg(file("identity", "PATH", "This machine's age identity").required(true))
            .arg(origin()).arg(recipient()))
        .subcommand(sub("install-release-app", "Install a macOS app from a GitHub release")
            .arg(required("OWNER/REPO"))
            .after_help("Run by mise as a post-install hook on macOS."))
        .subcommand(brew_tree())
}

fn bootstrap_tree() -> Command {
    sub("bootstrap", "Set up this machine from the configuration")
        .args_conflicts_with_subcommands(true)
        .subcommand(sub(
            "persist-selected",
            "Save the profile and machine ID (pre-dotfiles hook)",
        ))
        .subcommand(sub(
            "complete",
            "Run the dots stages (final bootstrap task)",
        ))
        .arg(file(
            "bundle",
            "FILE",
            "Read private inputs from this encrypted bundle",
        ))
        .arg(file(
            "identity",
            "KEY",
            "age identity for the bundle and recovery",
        ))
        .arg(
            value(
                "restore",
                "ID",
                "Restore the settings history of machine ID",
            )
            .value_parser(machine_id)
            .conflicts_with("only"),
        )
        .arg(value("from", "REPO", "Bootstrap from this repository"))
        .arg(value("adopt", "REPO", "Adopt this repository"))
        .arg(file("from-dir", "DIR", "Use this checkout directory").value_hint(ValueHint::DirPath))
        .arg(value(
            "config-dir",
            "DIR",
            "Configuration directory inside the checkout",
        ))
        .arg(flag(
            "replace-history",
            "Replace unrelated settings history",
        ))
        .arg(flag("take-remote-all", "Take every remote setting"))
        .arg(flag("dry-run", "Show what would happen").short('n'))
        .arg(flag("yes", "Skip confirmation").short('y'))
        .arg(flag("skip-dirty", "Skip targets with local changes"))
        .arg(flag("force-dotfiles", "Overwrite conflicting dotfiles"))
        .arg(flag("prune-dotfiles", "Remove stale dotfiles"))
        .arg(
            value("only", "PART", "Run only these parts")
                .action(ArgAction::Append)
                .value_delimiter(',')
                .value_parser(PARTS)
                .conflicts_with("skip"),
        )
        .arg(
            value("skip", "PART", "Skip these parts")
                .action(ArgAction::Append)
                .value_delimiter(',')
                .value_parser(PARTS),
        )
        .arg(flag(
            "prompt-secrets",
            "Prompt for missing template secrets",
        ))
        .arg(flag("prompt-vars", "Prompt for configuration variables"))
        .arg(flag("save-env", "Save the selected environments"))
        .arg(
            value("env", "PROFILE", "Select mise environments")
                .short('E')
                .value_delimiter(',')
                .action(ArgAction::Append),
        )
        .arg(flag("update", "Refresh metadata and update repositories"))
        .arg(flag("upgrade", "Upgrade installed packages and tools"))
}

fn brew_tree() -> Command {
    let kind = || {
        Arg::new("KIND")
            .required(true)
            .value_parser(["formula", "cask"])
    };
    let install_only = || flag("install-only", "Only install when missing");

    sub("brew", "Homebrew steps for installer exception recipes")
        .subcommand_required(true)
        .subcommand(sub("path", "Print the Homebrew executable"))
        .subcommand(
            sub("version", "Print a package's installed version")
                .arg(kind())
                .arg(required("TOKEN")),
        )
        .subcommand(sub(
            "admin-available",
            "Succeed when administrator access is available",
        ))
        .subcommand(
            sub(
                "defer-for-admin",
                "Defer a package that needs admin access (exit 3)",
            )
            .arg(required("TOKEN")),
        )
        .subcommand(
            sub("record", "Record a package change in DOTS_CHANGES")
                .arg(kind())
                .arg(required("TOKEN"))
                .arg(
                    Arg::new("ACTION")
                        .required(true)
                        .value_parser(["installed", "updated"]),
                )
                .arg(positional("BEFORE")),
        )
        .subcommand(
            sub("update-cask", "Install or upgrade one cask")
                .arg(flag(
                    "auto-updates",
                    "Also upgrade a cask that updates itself",
                ))
                .arg(flag("admin", "Defer without administrator access"))
                .arg(install_only())
                .arg(required("TOKEN"))
                .arg(positional("PROCESS")),
        )
        .subcommand(
            sub("update-formula", "Install or upgrade one formula")
                .arg(install_only())
                .arg(required("TOKEN")),
        )
        .subcommand(
            sub("hold-formula", "Check that a formula is held at a version")
                .arg(required("TOKEN"))
                .arg(required("VERSION"))
                .arg(positional("REASON")),
        )
        .subcommand(
            sub("hold-cask", "Check that a cask is held at a version")
                .arg(required("TOKEN"))
                .arg(required("VERSION"))
                .arg(required("APP_PATH").value_hint(ValueHint::DirPath))
                .arg(positional("REASON")),
        )
}

fn flags(matches: &ArgMatches) -> Flags {
    Flags {
        plain: matches.get_flag("plain"),
        verbose: matches.get_flag("verbose"),
    }
}

fn display_flags(root: Flags, command: Option<(&str, &ArgMatches)>) -> Result<Flags, Fail> {
    let after = command
        .filter(|(name, _)| *name == "up")
        .map(|(_, m)| flags(m))
        .unwrap_or_default();
    for (a, b, name) in [
        (root.plain, after.plain, "--plain"),
        (root.verbose, after.verbose, "--verbose"),
    ] {
        if a && b {
            return Err(Fail::usage(format!(
                "{name} may be given only once; use dots help"
            )));
        }
    }
    let flags = Flags {
        plain: root.plain || after.plain,
        verbose: root.verbose || after.verbose,
    };
    if flags.plain && flags.verbose {
        return Err(display_conflict());
    }
    Ok(flags)
}

fn display_conflict() -> Fail {
    Fail::usage("--plain and --verbose cannot be combined; use dots help")
}

fn print_help() -> Result<u8, Fail> {
    let mut output = HELP.to_owned();
    // Config discovery is optional for help, and never evaluates configuration.
    if let Ok(ctx) = context(Flags::default()) {
        let grammar = tree();
        let mut commands = config_commands::list(&ctx.root);
        commands.retain(|name| grammar.find_subcommand(name).is_none() && name != "__complete");
        commands.sort_unstable();
        if !commands.is_empty() {
            output.push_str(&format!(
                "\nConfiguration commands ({}):\n",
                ctx.root.path().join("dots/commands").display()
            ));
            for name in commands {
                output.push_str(&format!("  {name} [ARGS...]\n"));
            }
        }
    }
    print(&output)
}

fn print(output: &str) -> Result<u8, Fail> {
    io::stdout()
        .lock()
        .write_all(output.as_bytes())
        .map_err(|error| Fail::new(1, format!("could not write output: {error}")))?;
    Ok(0)
}

fn parse_error(error: &clap::Error, name: Option<&str>) -> Fail {
    // Never use clap's full diagnostic here. It can contain argument values,
    // including a repository URL with credentials, and a multi-line usage dump.
    let option = error
        .get(ContextKind::InvalidArg)
        .and_then(|context| match context {
            ContextValue::String(value) => value.split([' ', '=']).next(),
            _ => None,
        })
        .filter(|s| {
            s.starts_with('-') && s.bytes().all(|b| b == b'-' || b.is_ascii_alphanumeric())
        });

    let diagnostic = match (error.kind(), name, option) {
        (_, Some("completions"), _) => "usage: dots completions bash|zsh|fish".to_owned(),
        (_, Some("help"), _) => "dots help takes no arguments".to_owned(),
        (ErrorKind::UnknownArgument, _, Some(option)) => format!("unknown option '{option}'"),
        (ErrorKind::MissingRequiredArgument, _, _) => "missing required argument".to_owned(),
        (ErrorKind::ArgumentConflict, _, _) => "conflicting or repeated options".to_owned(),
        (ErrorKind::InvalidSubcommand, _, _) => "unknown subcommand".to_owned(),
        (ErrorKind::InvalidValue | ErrorKind::ValueValidation, _, _) => {
            "invalid argument value".to_owned()
        }
        _ => "unexpected argument".to_owned(),
    };
    let usage = match name {
        Some("completions") => diagnostic,
        Some("help") => format!("{diagnostic}; use dots COMMAND --help"),
        Some(name) => format!("{diagnostic} for dots {name}; use dots help"),
        None => format!("{diagnostic}; use dots help"),
    };
    Fail::usage(usage)
}

fn command_index(argv: &[OsString]) -> Option<usize> {
    argv.iter()
        .enumerate()
        .skip(1)
        .find(|(_, word)| !matches!(word.to_str(), Some("--plain" | "--verbose")))
        .map(|(index, _)| index)
}

fn is_file_request(request: &OsStr) -> bool {
    let bytes = request.as_encoded_bytes();
    [b"/".as_slice(), b"~/", b"./", b"../"]
        .iter()
        .any(|prefix| bytes.starts_with(prefix))
}

fn validate_requests(name: &str, args: &ArgMatches) -> Result<(), Fail> {
    if name == "add" && args.contains_id("asset") {
        let requests = args
            .get_many::<OsString>("REQUEST")
            .expect("required by grammar");
        let mut requests = requests.map(OsString::as_os_str);
        let is_url = requests.next().is_some_and(|request| {
            request
                .as_encoded_bytes()
                .starts_with(b"https://github.com/")
        });
        if !is_url || requests.next().is_some() {
            return Err(Fail::usage(
                "--asset requires exactly one GitHub repository URL; use dots help",
            ));
        }
    }
    if name == "remove" && args.get_flag("keep-installed") {
        let mut targets = args
            .get_many::<OsString>("TARGET")
            .expect("required by grammar");
        if targets.any(|target| is_file_request(target)) {
            return Err(Fail::usage(
                "--keep-installed cannot be used with a file request; use dots help",
            ));
        }
    }
    Ok(())
}

fn context(flags: Flags) -> Result<Ctx, Fail> {
    let env = Env::current()
        .map_err(|error| Fail::new(1, format!("could not read the current directory: {error}")))?;
    let root = Root::resolve(&env)?;
    let ctx = Ctx { env, root, flags };
    if let Err(error) = skill::refresh(&ctx)
        && !error.is_unported()
    {
        eprintln!("dots: {error}");
    }
    Ok(ctx)
}

fn require_config(root: &Root) -> Result<(), Fail> {
    if !root.path().join("config.toml").is_file() {
        return Err(Fail::usage(format!(
            "no configuration at {} (config.toml not found); create one with dots init or set DOTS_ROOT",
            root.path().display(),
        )));
    }
    Ok(())
}

pub fn run(argv: Vec<OsString>) -> Result<u8, Fail> {
    if argv.get(1).is_some_and(|word| word == "__complete") {
        return completion::query(&argv[2..]);
    }

    if argv.get(1).is_some_and(|word| word == "__exec") {
        return crate::process::exec_pinned(&argv[2..]);
    }

    let grammar = tree();
    let index = command_index(&argv);
    let spelling = index.and_then(|index| argv[index].to_str());
    if spelling == Some("--") {
        return Err(Fail::usage(
            "unexpected '--' before the command; use dots help",
        ));
    }
    let known = spelling.and_then(|word| grammar.find_subcommand(word));
    let pass_through = known.is_some_and(|c| matches!(c.get_name(), "test" | "mise"));
    let parsed = if pass_through {
        let index = index.expect("known command");
        // Preserve the original tail, including a literal --.
        grammar.clone().try_get_matches_from(&argv[..=index])
    } else {
        grammar.clone().try_get_matches_from(&argv)
    };
    let matches = match parsed {
        Ok(matches) => matches,
        Err(error) if error.kind() == ErrorKind::DisplayHelp => {
            if let Some(index) = index {
                let before = argv[1..index].iter();
                let after = if known.is_some_and(|command| command.get_name() == "up") {
                    &argv[index + 1..]
                } else {
                    &[]
                };
                let flags = before.chain(after).take_while(|word| *word != "--");
                let mut plain = false;
                let mut verbose = false;
                for word in flags {
                    plain |= word == "--plain";
                    verbose |= word == "--verbose";
                }
                if plain && verbose {
                    return Err(display_conflict());
                }
            }
            if known.is_none() {
                return print_help();
            }
            if !matches!(known.map(Command::get_name), Some("completions")) {
                let _ = context(Flags::default());
            }
            return print(&error.to_string());
        }
        Err(error) => return Err(parse_error(&error, known.map(Command::get_name))),
    };
    let command = matches.subcommand();
    let flags = display_flags(flags(&matches), command)?;

    if matches.get_flag("version") {
        if argv.len() != 2 {
            return Err(Fail::usage("usage: dots --version; use dots help"));
        }
        let env = Env::current().map_err(|error| {
            Fail::new(1, format!("could not read the current directory: {error}"))
        })?;
        let version = config_commands::version(&env)?;
        let _ = context(flags);
        return print(&format!("{version}\n"));
    }

    let Some((name, args)) = command else {
        return print_help();
    };
    if name == "help" {
        return print_help();
    }
    if name == "completions" {
        let shell = match args.get_one::<String>("SHELL").map(String::as_str) {
            Some("bash") => Shell::Bash,
            Some("zsh") => Shell::Zsh,
            Some("fish") => Shell::Fish,
            _ => unreachable!("selected by grammar"),
        };
        return print(completion::script(shell)?);
    }
    validate_requests(name, args)?;

    let tail = index.map_or(&[][..], |index| &argv[index + 1..]);
    if name == "mise" && tail.is_empty() {
        return Err(Fail::usage("usage: dots mise ARGS...; use dots help"));
    }
    if known.is_none() {
        let ctx = context(flags)?;
        if config_commands::is_external(&ctx.root, name) {
            if flags != Flags::default() {
                return Err(Fail::usage(
                    "global flags apply only to built-in commands; use dots help",
                ));
            }
            require_config(&ctx.root)?;
        }
        return config_commands::dispatch(&ctx, Invocation::External { name, args: tail });
    }
    let ctx = context(flags)?;
    if !matches!(
        name,
        "init" | "bundle" | "brew" | "install-release-app" | "status" | "sync"
    ) {
        require_config(&ctx.root)?;
    }
    match name {
        "root" => config_commands::dispatch(&ctx, Invocation::Root),
        "profile" => config_commands::dispatch(&ctx, Invocation::Profile(args)),
        "mise" => config_commands::dispatch(&ctx, Invocation::Mise(tail)),
        "reports" => ui::reports(&ctx, args),
        "up" if args.get_flag("last") => ui::last(&ctx, args),
        "up" => update::up(&ctx, args),
        "status" => inspect::status(&ctx, args),
        "plan" => inspect::plan(&ctx, args),
        "apply" => inspect::apply(&ctx, args),
        "test" => inspect::test(&ctx, tail),
        "retire-dotfiles" => inspect::retire_dotfiles(&ctx, args),
        "exceptions" => exceptions::install(&ctx, args),
        "sync" if args.get_flag("last") => receipt::last(&ctx, args),
        "sync" => sync::sync(&ctx, args),
        "add" => packages::add(&ctx, args),
        "remove" => packages::remove(&ctx, args),
        "install-release-app" => packages::install_release_app(&ctx, args),
        "brew" => brew::brew(&ctx, args),
        "init" => init::init(&ctx, args),
        "bootstrap" => bootstrap::bootstrap(&ctx, args),
        "remote" => bootstrap::remote(&ctx, args),
        "bundle" => bootstrap::bundle(&ctx, args),
        "backup" => history::backup(&ctx, args),
        "restore" => history::restore_command(&ctx, args),
        "enroll-history" => history::enroll_command(&ctx, args),
        "publish" => publish::publish(&ctx, args),
        _ => unreachable!("covered by command grammar"),
    }
}
