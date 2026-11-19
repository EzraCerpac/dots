use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

use crate::Fail;
use crate::config::{Root, Vars};
use crate::config_commands::valid_name;
use crate::process::{BundlePipe, Mise, is_executable};
use crate::run::{Change, Issue, Outcome, Policy, Run, StageId, exit_code};

pub mod exceptions;
use exceptions::Mode;

pub enum Dir {
    Update,
    Bootstrap,
}

pub struct Hook {
    pub path: PathBuf,
    pub label: String,
}

pub enum HookEnv<'a> {
    Update {
        tools_ok: bool,
        tools_upgraded: bool,
    },
    Bootstrap {
        private_ready: bool,
        restored: bool,
        bundle_json: &'a [u8],
    },
}

pub fn list(root: &Root, dir: Dir, vars: &Vars, profile: &str) -> Result<Vec<Hook>, Fail> {
    let directory = root.path().join("dots/hooks").join(match dir {
        Dir::Update => "update.d",
        Dir::Bootstrap => "bootstrap.d",
    });
    list_in(&directory, &vars.profiles, profile)
}

fn list_in(directory: &Path, profiles: &[String], profile: &str) -> Result<Vec<Hook>, Fail> {
    let read_error = |error| {
        Fail::new(
            1,
            format!("cannot list hooks in {}: {error}", directory.display()),
        )
    };
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(read_error(error)),
    };
    let mut entries = entries.collect::<Result<Vec<_>, _>>().map_err(read_error)?;
    entries.sort_unstable_by_key(fs::DirEntry::file_name);

    let mut hooks = Vec::new();
    for entry in entries {
        let name = entry.file_name();
        let Some((stem, only)) = name.to_str().and_then(hook_name) else {
            continue;
        };
        let path = entry.path();
        if !is_executable(&path) {
            continue;
        }
        if let Some(only) = only {
            if !profiles.iter().any(|known| known == only) {
                return Err(Fail::usage(format!(
                    "hook {} names unknown profile {only}; use one of {}",
                    path.display(),
                    profiles.join(", "),
                )));
            }
            if only != profile {
                continue;
            }
        }
        hooks.push(Hook {
            path,
            label: hook_label(stem),
        });
    }
    Ok(hooks)
}

fn hook_name(name: &str) -> Option<(&str, Option<&str>)> {
    let (stem, profile) = name
        .split_once('@')
        .map_or((name, None), |(stem, profile)| (stem, Some(profile)));
    if !valid_name(stem)
        || profile.is_some_and(|profile| profile.is_empty() || !profile.bytes().all(name_byte))
    {
        return None;
    }
    Some((stem, profile))
}

fn name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

fn hook_label(stem: &str) -> String {
    let after_number = stem.trim_start_matches(|c: char| c.is_ascii_digit());
    let label = if after_number.len() > 1 {
        after_number.strip_prefix(['-', '_']).unwrap_or(stem)
    } else {
        stem
    };
    let label = label
        .rsplit_once('.')
        .filter(|(stem, extension)| !stem.is_empty() && !extension.is_empty())
        .map_or(label, |(stem, _)| stem);
    label.replace(['-', '_'], " ")
}

enum ChildEnv<'a> {
    Hook(HookEnv<'a>),
    Recipe(Mode),
}

pub fn run(run: &Run<'_>, hook: &Hook, env: HookEnv<'_>) -> Outcome {
    run_stage(
        run,
        hook.label.as_str(),
        ChildEnv::Hook(env),
        |stage, env| run_child(run, stage, &hook.path, &[], env),
    )
}

fn run_stage<'a>(
    run: &Run<'_>,
    label: impl Into<String>,
    env: ChildEnv<'a>,
    execute: impl FnOnce(StageId, ChildEnv<'a>) -> Result<u8, Fail>,
) -> Outcome {
    if run.cancelled() {
        return Outcome::Cancelled;
    }
    let label = label.into();
    let stage = run.start(&label);
    run_started_stage(run, stage, &label, env, execute)
}

fn run_started_stage<'a>(
    run: &Run<'_>,
    stage: StageId,
    label: &str,
    env: ChildEnv<'a>,
    execute: impl FnOnce(StageId, ChildEnv<'a>) -> Result<u8, Fail>,
) -> Outcome {
    if run.cancelled() {
        run.finish(stage, Outcome::Cancelled);
        return Outcome::Cancelled;
    }
    let next = match env {
        ChildEnv::Hook(HookEnv::Bootstrap { .. }) => {
            "Review the hook output, then rerun dots bootstrap."
        }
        ChildEnv::Hook(_) | ChildEnv::Recipe(Mode::Update) => {
            "Review the stage output, then rerun dots up."
        }
        ChildEnv::Recipe(Mode::InstallOnly) => {
            "Review the recipe output, then rerun dots exceptions."
        }
    };
    let outcome = match execute(stage, env) {
        Ok(status) => {
            let outcome = Outcome::from_hook_status(status);
            if !run.cancelled() && matches!(outcome, Outcome::Deferred | Outcome::Failed) {
                let result = if outcome == Outcome::Deferred {
                    "deferred"
                } else {
                    "failed"
                };
                run.issue(
                    stage,
                    Issue::new(format!("{label} {result} (exit {status})"), next),
                );
            }
            outcome
        }
        Err(error) => {
            if !run.cancelled() {
                eprintln!("dots: {error}");
                run.issue(stage, Issue::new(error.to_string(), next));
            }
            Outcome::Failed
        }
    };
    let outcome = if run.cancelled() {
        Outcome::Cancelled
    } else {
        outcome
    };
    run.finish(stage, outcome);
    outcome
}

fn run_child(
    run: &Run<'_>,
    stage: StageId,
    path: &Path,
    args: &[&str],
    env: ChildEnv<'_>,
) -> Result<u8, Fail> {
    let mise = Mise::new(run.ctx())?;
    let changes = if matches!(
        env,
        ChildEnv::Hook(HookEnv::Update { .. }) | ChildEnv::Recipe(Mode::Update)
    ) {
        Some(NamedTempFile::new().map_err(|error| {
            Fail::new(
                1,
                format!("cannot create a private DOTS_CHANGES file: {error}"),
            )
        })?)
    } else {
        None
    };

    // Keep the pipe's writer alive through the supervised child run.
    let bundle = if let ChildEnv::Hook(HookEnv::Bootstrap {
        private_ready,
        bundle_json,
        ..
    }) = env
    {
        Some(BundlePipe::new(if private_ready {
            bundle_json
        } else {
            b"{}"
        })?)
    } else {
        None
    };

    let child = mise.exec_with_env(path, args.iter().copied(), |child| {
        if let Some(file) = &changes {
            child
                .env("DOTS_CHANGES", file.path())
                .env("HOMEBREW_NO_INSTALL_CLEANUP", "1");
        }
        match env {
            ChildEnv::Hook(HookEnv::Update {
                tools_ok,
                tools_upgraded,
            }) => {
                child.env("DOTS_TOOLS_OK", flag(tools_ok));
                child.env("DOTS_TOOLS_UPGRADED", flag(tools_ok && tools_upgraded));
            }
            ChildEnv::Hook(HookEnv::Bootstrap {
                private_ready,
                restored,
                ..
            }) => {
                child.env("DOTS_PRIVATE_READY", flag(private_ready));
                child.env("DOTS_RESTORED", flag(restored));
            }
            ChildEnv::Recipe(_) => {}
        }
        if let Some(pipe) = &bundle {
            pipe.attach(child)?;
        }
        Ok(())
    })?;

    let result = run.run(stage, child, Policy::Interactive);
    if let Some(file) = changes {
        for change in BufReader::new(file.as_file())
            .split(b'\n')
            .map_while(Result::ok)
            .filter_map(|line| String::from_utf8(line).ok())
            .filter_map(change_row)
        {
            run.change(stage, change);
        }
    }
    result
        .map(exit_code)
        .map_err(|error| Fail::new(1, format!("cannot run {}: {error}", path.display())))
}

fn flag(value: bool) -> &'static str {
    if value { "1" } else { "0" }
}

fn change_row(line: String) -> Option<Change> {
    let mut fields = line.split('\t');
    let item = fields.next().filter(|item| !item.is_empty())?;
    let before = fields.next()?;
    let after = fields.next()?;
    if fields.next().is_some() {
        return None;
    }
    Some(Change {
        item: item.into(),
        before: before.into(),
        after: after.into(),
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn fixture_hook(dir: &Path, name: &str, mode: u32) {
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn filters_and_orders_executable_hooks_by_filename() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "11-Login-shell.sh@laptop",
            "10_check.sh",
            "20-skip@server",
            "bad+name",
            ".hidden",
        ] {
            fixture_hook(dir.path(), name, 0o755);
        }
        fixture_hook(dir.path(), "0_ignored@unknown", 0o644);
        fs::create_dir(dir.path().join("10-directory")).unwrap();
        let profiles = ["laptop".to_owned(), "server".to_owned()];
        let hooks = list_in(dir.path(), &profiles, "laptop").unwrap();
        assert_eq!(
            hooks
                .iter()
                .map(|hook| hook.label.as_str())
                .collect::<Vec<_>>(),
            ["check", "Login shell"]
        );

        fixture_hook(dir.path(), "90-error@unknown", 0o755);
        let error = list_in(dir.path(), &profiles, "laptop").err().unwrap();
        assert_eq!(error.status, 2);
        assert!(error.diagnostic.contains("90-error@unknown"));
    }

    #[test]
    fn cancellation_at_stage_admission_preserves_only_admitted_stages() {
        use crate::run::Sink;
        use crate::{Ctx, Env, Flags};

        const CASE: &str = "DOTS_TEST_HOOK_ADMISSION";
        let Some(case) = std::env::var_os(CASE) else {
            let fixture = tempfile::tempdir_in("/tmp").unwrap();
            let names = [
                "HOME",
                "XDG_CONFIG_HOME",
                "XDG_CACHE_HOME",
                "XDG_DATA_HOME",
                "XDG_STATE_HOME",
                "CODEX_HOME",
                "CLAUDE_CONFIG_DIR",
                "TMPDIR",
                "DOTS_ROOT",
                "MISE_CONFIG_DIR",
                "MISE_DATA_DIR",
                "MISE_CACHE_DIR",
                "MISE_STATE_DIR",
                "MISE_TMP_DIR",
                "MISE_INSTALLS_DIR",
                "MISE_INSTALL_STORE_DIR",
                "MISE_DOWNLOADS_DIR",
                "MISE_PLUGINS_DIR",
                "MISE_SHIMS_DIR",
                "MISE_SYSTEM_CONFIG_DIR",
                "MISE_SYSTEM_DATA_DIR",
                "MISE_SYSTEM_PLUGINS_DIRS",
                "MISE_SYSTEM_INSTALLS_DIR",
                "MISE_SYSTEM_SHIMS_DIR",
                "MISE_SHARED_INSTALL_DIRS",
            ];
            for case in [
                "update hook",
                "bootstrap hook",
                "recipe",
                "admitted",
                "after admission",
            ] {
                // Isolate the process-wide signal handler from other tests.
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .env_clear()
                    .envs(names.map(|name| (name, fixture.path())))
                    .env(CASE, case)
                    .current_dir(fixture.path())
                    .args([
                        "--exact",
                        "hooks::tests::cancellation_at_stage_admission_preserves_only_admitted_stages",
                    ])
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{case}: {output:?}");
                assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            }
            return;
        };

        fn interrupt() {
            assert_eq!(unsafe { nix::libc::raise(nix::libc::SIGINT) }, 0);
        }

        struct InterruptOnAdmission;
        impl From<InterruptOnAdmission> for String {
            fn from(_: InterruptOnAdmission) -> Self {
                interrupt();
                "Interrupted stage".into()
            }
        }

        let env = Env::current().unwrap();
        let ctx = Ctx {
            root: Root::resolve(&env).unwrap(),
            env,
            flags: Flags::default(),
        };
        let run = Run::begin(&ctx, Sink::Unreported);
        if case == "admitted" || case == "after admission" {
            let label = "Admitted stage";
            let env = ChildEnv::Recipe(Mode::Update);
            let outcome = if case == "after admission" {
                let stage = run.start(label);
                interrupt();
                run_started_stage(&run, stage, label, env, |_, _| {
                    panic!("cancelled admitted stage ran setup");
                })
            } else {
                run_stage(&run, label, env, |_, _| {
                    interrupt();
                    Ok(0)
                })
            };
            assert_eq!(outcome, Outcome::Cancelled);
            let snapshot = run.snapshot();
            let [stage] = snapshot.stages.as_slice() else {
                panic!("expected one admitted stage");
            };
            assert_eq!(stage.label, label);
            assert_eq!(stage.outcome, Some(Outcome::Cancelled));
            assert!(stage.issues.is_empty());
        } else {
            let env = match case.to_str().unwrap() {
                "update hook" => ChildEnv::Hook(HookEnv::Update {
                    tools_ok: true,
                    tools_upgraded: true,
                }),
                "bootstrap hook" => ChildEnv::Hook(HookEnv::Bootstrap {
                    private_ready: true,
                    restored: false,
                    bundle_json: b"{}",
                }),
                "recipe" => ChildEnv::Recipe(Mode::Update),
                _ => panic!("unexpected case: {case:?}"),
            };
            let outcome = run_stage(&run, InterruptOnAdmission, env, |_, _| {
                panic!("denied stage ran setup");
            });
            assert_eq!(outcome, Outcome::Cancelled);
            assert!(run.snapshot().stages.is_empty());
        }
        assert_eq!(run.end(0), 130);
    }

    #[test]
    fn change_rows_keep_all_three_values_and_reject_extra_fields() {
        let row = change_row("app\tnot installed\t1.2".into()).unwrap();
        assert_eq!(
            (row.item.as_str(), row.before.as_str(), row.after.as_str()),
            ("app", "not installed", "1.2")
        );
        assert!(change_row("app\t1".into()).is_none());
        assert!(change_row("app\t1\t2\textra".into()).is_none());
    }
}
