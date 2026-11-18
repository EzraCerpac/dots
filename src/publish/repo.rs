use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::config::RepoSetting;
use crate::process::Cwd;
use crate::run::Run;
use crate::sync::lock::RepoLock;
use crate::{Ctx, Fail};

use super::{AGGREGATE_PREFIX, failure};

pub(super) struct Workspace<'a, 'ctx> {
    ctx: &'ctx Ctx,
    run: &'a Run<'ctx>,
    root: PathBuf,
    live: PathBuf,
    pub(super) slug: String,
}

pub(super) struct Origin {
    preferred_push: String,
}

impl<'a, 'ctx> Workspace<'a, 'ctx> {
    pub(super) fn open(
        ctx: &'ctx Ctx,
        run: &'a Run<'ctx>,
        configured: Option<&RepoSetting>,
    ) -> Result<Self, Fail> {
        let config = ctx.root.path();
        let slug = if let Some(setting) = configured {
            setting.github().map(str::to_owned)
        } else {
            if let Some(origin) = configured_urls(ctx, run, config, "remote.origin.url")? {
                let raw = stdout(&origin)?;
                raw.split('\0')
                    .next()
                    .and_then(github_url)
                    .and_then(|repo| repo.github().map(str::to_owned))
            } else {
                None
            }
        }
        .ok_or_else(|| {
            failure("dots publish requires a GitHub repository; configure a GitHub origin")
        })?;
        let root =
            git_path(ctx, run, ctx.env.cwd(), "--show-toplevel")?.ok_or_else(workspace_required)?;
        let live = git_path(ctx, run, config, "--show-toplevel")?
            .filter(|path| path == config)
            .ok_or_else(|| failure("the configuration repository has no origin remote"))?;
        let workspace = Self {
            ctx,
            run,
            root,
            live,
            slug,
        };
        workspace.validate_origin()?;
        if workspace.root == workspace.live {
            return Err(failure(
                "use a separate colocated JJ workspace to publish from this configuration",
            ));
        }
        if !workspace.root.join(".jj").is_dir() {
            return Err(workspace_required());
        }
        let git_root = workspace.git(
            &["rev-parse", "--absolute-git-dir"],
            "locate the workspace Git directory",
        )?;
        let jj_root = workspace.jj(
            &["--ignore-working-copy", "git", "root"],
            "verify the colocated JJ workspace",
        )?;
        let git_dir =
            fs::canonicalize(stdout_path(&git_root)?).map_err(|_| workspace_required())?;
        let jj_git_dir = fs::canonicalize(workspace.root.join(stdout_path(&jj_root)?))
            .map_err(|_| workspace_required())?;
        if git_dir != jj_git_dir {
            return Err(workspace_required());
        }
        Ok(workspace)
    }

    pub(super) fn lock(&self) -> Result<Vec<RepoLock>, Fail> {
        let mut dirs = Vec::new();
        for root in [&self.root, &self.live] {
            let common = git_path(self.ctx, self.run, root, "--git-common-dir")?
                .ok_or_else(|| failure("could not locate the repository lock directory"))?;
            dirs.push(common);
        }
        dirs.sort_unstable();
        dirs.dedup();
        let mut locks = Vec::with_capacity(dirs.len());
        for dir in dirs {
            let lock = RepoLock::try_acquire(&dir)
                .map_err(|_| failure("could not acquire the source repository lock"))?
                .ok_or_else(|| {
                    failure(
                        "another source operation is running; rerun dots publish after it finishes",
                    )
                })?;
            locks.push(lock);
        }
        Ok(locks)
    }

    pub(super) fn validate_origin(&self) -> Result<Origin, Fail> {
        let raw = configured_urls(self.ctx, self.run, &self.root, "remote.origin.url")?
            .ok_or_else(|| failure("the JJ workspace has no origin remote"))?;
        self.check_urls(stdout(&raw)?.split_terminator('\0'))?;
        if self.live != self.root {
            let raw = configured_urls(self.ctx, self.run, &self.live, "remote.origin.url")?
                .ok_or_else(|| failure("the configuration repository has no origin remote"))?;
            self.check_urls(stdout(&raw)?.split_terminator('\0'))?;
        }
        if let Some(raw_push) =
            configured_urls(self.ctx, self.run, &self.root, "remote.origin.pushurl")?
            && !stdout(&raw_push)?
                .split_terminator('\0')
                .all(|url| RepoSetting::parse(url).is_ok())
        {
            return Err(failure(
                "origin has an invalid push URL; check Git configuration",
            ));
        }
        let fetch = self.git(&["remote", "get-url", "origin"], "resolve the fetch URL")?;
        self.check_urls(stdout(&fetch)?.split_terminator('\n'))?;
        let push = self.git(
            &["remote", "get-url", "--push", "--all", "origin"],
            "resolve the push URLs",
        )?;
        let mut push_urls = stdout(&push)?.split_terminator('\n');
        self.check_urls(push_urls.clone())?;
        let preferred_push = push_urls
            .next()
            .ok_or_else(|| failure("origin has no push URL"))?
            .to_owned();
        Ok(Origin { preferred_push })
    }

    fn check_urls<'s>(&self, mut urls: impl Iterator<Item = &'s str>) -> Result<(), Fail> {
        let matches = |url: &str| {
            github_url(url).is_some_and(|repo| {
                repo.github()
                    .is_some_and(|slug| slug.eq_ignore_ascii_case(&self.slug))
            })
        };
        if urls.next().is_some_and(matches) && urls.all(matches) {
            Ok(())
        } else {
            Err(failure(
                "origin does not match the configuration's GitHub repository; check the fetch and push URLs and Git URL rewrites",
            ))
        }
    }

    pub(super) fn fetch_main(&self) -> Result<String, Fail> {
        // An explicit fetch of main fails when the server has no main, even
        // if a remote-tracking ref from a prior fetch is still present.
        self.git(
            &[
                "fetch",
                "--no-tags",
                "--no-recurse-submodules",
                "--no-write-fetch-head",
                "--refmap=",
                "origin",
                "+refs/heads/main:refs/remotes/origin/main",
            ],
            "fetch main from origin; check that the remote has a main branch",
        )?;
        self.tracking_main()
    }

    fn tracking_main(&self) -> Result<String, Fail> {
        let out = self.git(
            &["rev-parse", "--verify", "refs/remotes/origin/main^{commit}"],
            "resolve the fetched main",
        )?;
        object_id(stdout(&out)?.trim()).map(str::to_owned)
    }

    pub(super) fn refuse_shared_dev(&self, base: &str, head: &str) -> Result<(), Fail> {
        let mut cmd = git_command(self.ctx, &self.root)?;
        cmd.args([
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/heads/dev^{commit}",
        ]);
        let out = self.probe(cmd, "git")?;
        match out.status.code() {
            Some(0) => {
                let dev = object_id(stdout(&out)?.trim())?;
                if !self
                    .revisions(&format!(
                        "({} & ::{}) ~ ::{}",
                        jj_commit(dev),
                        jj_commit(head),
                        jj_commit(base)
                    ))?
                    .is_empty()
                {
                    return Err(failure(
                        "shared dev cannot be published as a pull request; extract a focused change onto a main-based task branch",
                    ));
                }
                Ok(())
            }
            Some(1) => Ok(()),
            _ => Err(failure("git could not read the workspace's dev branch")),
        }
    }

    pub(super) fn require_unchanged(
        &self,
        base: &str,
        selector: &str,
        commit: &str,
    ) -> Result<(), Fail> {
        if self.revisions(selector)?.as_slice() != [commit] || self.tracking_main()? != base {
            return Err(stale());
        }
        Ok(())
    }

    pub(super) fn revisions(&self, revset: &str) -> Result<Vec<String>, Fail> {
        let out = self.jj(
            &[
                "log",
                "--no-graph",
                "-r",
                revset,
                "-T",
                "commit_id ++ \"\\n\"",
            ],
            "read workspace revisions",
        )?;
        stdout(&out)?
            .lines()
            .map(|line| object_id(line).map(str::to_owned))
            .collect()
    }

    pub(super) fn one_revision(&self, revset: &str) -> Result<String, Fail> {
        let commits = self.revisions(revset)?;
        if let [commit] = commits.as_slice() {
            Ok(commit.clone())
        } else {
            Err(failure("expected one unambiguous JJ revision"))
        }
    }

    pub(super) fn description(&self, commit: &str) -> Result<String, Fail> {
        let out = self.jj(
            &[
                "log",
                "--no-graph",
                "-r",
                &jj_commit(commit),
                "-T",
                "description",
            ],
            "read the change description",
        )?;
        Ok(stdout(&out)?.trim().to_owned())
    }

    pub(super) fn stack_head(&self, working: &str) -> Result<String, Fail> {
        let mut head = working.to_owned();
        loop {
            let revision = jj_commit(&head);
            let out = self.jj(
                &[
                    "log",
                    "--no-graph",
                    "-r",
                    &revision,
                    "-T",
                    "if(empty, \"yes\", \"no\") ++ \"\\n\" ++ description",
                ],
                "locate the current stack",
            )?;
            let (empty, description) = stdout(&out)?
                .split_once('\n')
                .ok_or_else(|| failure("JJ returned an invalid stack revision"))?;
            if empty != "yes" || !description.trim().is_empty() {
                break;
            }
            let parents = self.revisions(&format!("{revision}-"))?;
            let [parent] = parents.as_slice() else { break };
            head = parent.clone();
        }
        Ok(head)
    }

    pub(super) fn change_id(&self, commit: &str) -> Result<String, Fail> {
        let out = self.jj(
            &[
                "log",
                "--no-graph",
                "-r",
                &jj_commit(commit),
                "-T",
                "change_id",
            ],
            "read the change ID",
        )?;
        let id = stdout(&out)?.trim();
        if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_lowercase()) {
            Err(failure("JJ returned an invalid change ID"))
        } else {
            Ok(id.to_owned())
        }
    }

    pub(super) fn valid_bookmark(&self, name: &str) -> Result<bool, Fail> {
        if !name.starts_with("wip/") {
            return Ok(false);
        }
        let mut cmd = git_command(self.ctx, &self.root)?;
        cmd.args(["check-ref-format", &format!("refs/heads/{name}")]);
        Ok(self.probe(cmd, "git")?.status.success())
    }

    pub(super) fn bookmark_expression(name: &str) -> String {
        let literal = name.replace('\\', "\\\\").replace('"', "\\\"");
        format!("bookmarks(exact:\"{literal}\")")
    }

    pub(super) fn local_bookmark(&self, name: &str) -> Result<String, Fail> {
        let commits = self.revisions(&Self::bookmark_expression(name))?;
        match commits.as_slice() {
            [commit] => Ok(commit.clone()),
            [] => Err(failure(
                "--bookmark must name an existing private wip/... bookmark",
            )),
            _ => Err(Fail::new(
                3,
                "the bookmark is conflicted; resolve it and rerun dots publish",
            )),
        }
    }

    pub(super) fn aggregate_bookmarks(&self, head: &str) -> Result<Vec<String>, Fail> {
        let out = self.jj(
            &[
                "bookmark",
                "list",
                "-r",
                &format!("::{}", jj_commit(head)),
                "-T",
                "name ++ \"\\n\"",
            ],
            "look up stack bookmarks",
        )?;
        let mut names = Vec::new();
        for name in stdout(&out)?
            .lines()
            .filter(|name| name.starts_with(AGGREGATE_PREFIX))
        {
            if self.valid_bookmark(name)? {
                names.push(name.to_owned());
            }
        }
        names.sort_unstable();
        names.dedup();
        Ok(names)
    }

    pub(super) fn gitleaks(&self, root: &Path, target: &OsStr, rules: &Path) -> Result<(), Fail> {
        let mut cmd = self.ctx.command("gitleaks", Cwd::Dir(root))?;
        cmd.args(["dir", "--no-banner", "--redact=100", "--exit-code=20"]);
        cmd.arg("--config")
            .arg(rules.join(".gitleaks.toml"))
            .arg("--gitleaks-ignore-path")
            .arg(rules)
            .arg("--")
            .arg(target);
        let output = self.probe(cmd, "gitleaks")?;
        match output.status.code() {
            Some(0) => Ok(()),
            Some(20) => Err(failure(
                "gitleaks found potential secrets; remove them from every outgoing commit and retry",
            )),
            _ => Err(failure(
                "gitleaks could not scan the outgoing commits; fix the scanner and retry",
            )),
        }
    }

    pub(super) fn save_bookmark(&self, name: &str, commit: &str) -> Result<(), Fail> {
        let previous = self.revisions(&Self::bookmark_expression(name))?;
        let target = jj_commit(commit);
        match previous.as_slice() {
            [] => self.jj(
                &["bookmark", "create", name, "-r", &target],
                "create the publish bookmark",
            )?,
            [old] if old == commit => return Ok(()),
            [old] => {
                let old = jj_commit(old);
                if !self.revisions(&format!("{old} ~ ::{target}"))?.is_empty() {
                    return Err(stale());
                }
                self.jj(
                    &["bookmark", "move", name, "--from", &old, "--to", &target],
                    "advance the publish bookmark",
                )?
            }
            _ => {
                return Err(Fail::new(
                    3,
                    "the publish bookmark is conflicted; resolve it and retry",
                ));
            }
        };
        Ok(())
    }

    pub(super) fn push(&self, origin: &Origin, name: &str, commit: &str) -> Result<(), Fail> {
        // A command-local remote has one destination and no fetch mapping.
        // Its unique alias spends Git's one rewrite on the verified URL, so
        // a rule matching that effective URL cannot redirect the transport.
        let token = tempfile::Builder::new()
            .prefix("dots-publish-")
            .tempdir()
            .map_err(|_| failure("could not prepare a private push target"))?;
        let remote = token
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| failure("could not name the private push target"))?;
        let alias = format!("https://dots-publish.invalid/{remote}");
        let rewrite = format!("url.{}.insteadOf={alias}", origin.preferred_push);
        let url = format!("remote.{remote}.url={alias}");
        let push_url = format!("remote.{remote}.pushurl={alias}");
        let config = ["-c", &rewrite, "-c", &url, "-c", &push_url];
        let target = self.git(
            &[config.as_slice(), &["ls-remote", "--get-url", remote]].concat(),
            "verify the private push target",
        )?;
        if stdout(&target)?.strip_suffix('\n') != Some(origin.preferred_push.as_str()) {
            return Err(failure("could not verify a single push URL for origin"));
        }
        let reference = format!("refs/heads/{name}");
        // A valid full-hex ref name must not override the approved object.
        let source = git_commit(commit);
        let resolved = self.git(
            &["rev-parse", "--verify", &source],
            "verify the approved commit",
        )?;
        if stdout(&resolved)?.strip_suffix('\n') != Some(commit) {
            return Err(failure(
                "Git did not resolve the approved commit for publication",
            ));
        }
        let refspec = format!("{source}:{reference}");
        self.git(
            &[
                config.as_slice(),
                &[
                    "push",
                    "--no-follow-tags",
                    "--recurse-submodules=no",
                    remote,
                    &refspec,
                ],
            ]
            .concat(),
            "push the scanned commit without force; check origin and the remote bookmark",
        )?;
        let advertised = self.git(
            &[
                config.as_slice(),
                &["ls-remote", "--exit-code", "--refs", remote, &reference],
            ]
            .concat(),
            "verify the published bookmark; check the remote before retrying",
        )?;
        if stdout(&advertised)? != format!("{commit}\t{reference}\n") {
            return Err(failure(
                "the remote bookmark does not match the approved commit; inspect it before retrying",
            ));
        }
        Ok(())
    }

    pub(super) fn gh(&self, operation: &str, args: &[&str], action: &str) -> Result<Output, Fail> {
        let mut cmd = self.ctx.command("gh", Cwd::Dir(&self.root))?;
        cmd.args([
            "pr",
            operation,
            "--repo",
            &format!("github.com/{}", self.slug),
        ])
        .args(args);
        self.checked(cmd, "gh", action)
    }

    pub(super) fn jj(&self, args: &[&str], action: &str) -> Result<Output, Fail> {
        let mut cmd = self.ctx.command("jj", Cwd::Dir(&self.root))?;
        sanitize_git_environment(&mut cmd);
        cmd.args([
            "--no-pager",
            "--color=never",
            "--config",
            "git.abandon-unreachable-commits=false",
        ])
        .args(args);
        self.checked(cmd, "jj", action)
    }

    pub(super) fn git(&self, args: &[&str], action: &str) -> Result<Output, Fail> {
        let mut cmd = git_command(self.ctx, &self.root)?;
        cmd.args(args);
        self.checked(cmd, "git", action)
    }

    fn checked(&self, cmd: Command, program: &str, action: &str) -> Result<Output, Fail> {
        let out = self.probe(cmd, program)?;
        if out.status.success() {
            Ok(out)
        } else {
            // Network diagnostics can contain a credential-bearing URL.
            Err(failure(format!("{program} could not {action}")))
        }
    }

    fn probe(&self, cmd: Command, program: &str) -> Result<Output, Fail> {
        if self.run.cancelled() {
            return Err(Fail::new(130, "publication was interrupted"));
        }
        self.run
            .probe(cmd)
            .map_err(|error| spawn_failure(program, error))
    }
}

fn git_path(ctx: &Ctx, run: &Run<'_>, cwd: &Path, option: &str) -> Result<Option<PathBuf>, Fail> {
    let mut cmd = git_command(ctx, cwd)?;
    cmd.args(["rev-parse", "--path-format=absolute", option]);
    let out = run
        .probe(cmd)
        .map_err(|error| spawn_failure("git", error))?;
    if !out.status.success() {
        return Ok(None);
    }
    fs::canonicalize(stdout_path(&out)?)
        .map(Some)
        .map_err(|_| workspace_required())
}

fn configured_urls(
    ctx: &Ctx,
    run: &Run<'_>,
    cwd: &Path,
    key: &str,
) -> Result<Option<Output>, Fail> {
    let mut cmd = git_command(ctx, cwd)?;
    cmd.args(["config", "--null", "--get-all", key]);
    let out = run
        .probe(cmd)
        .map_err(|error| spawn_failure("git", error))?;
    match out.status.code() {
        Some(0) => Ok(Some(out)),
        Some(1) => Ok(None),
        _ => Err(failure("could not read the configured origin URLs")),
    }
}

fn git_command(ctx: &Ctx, cwd: &Path) -> Result<Command, Fail> {
    let mut cmd = ctx.command("git", Cwd::Dir(cwd))?;
    sanitize_git_environment(&mut cmd);
    cmd.arg("--no-pager");
    Ok(cmd)
}

fn sanitize_git_environment(cmd: &mut Command) {
    for name in [
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
    ] {
        cmd.env_remove(name);
    }
    cmd.env("GIT_NO_REPLACE_OBJECTS", "1");
    cmd.env("GIT_NO_LAZY_FETCH", "1");
}

pub(super) fn stdout(output: &Output) -> Result<&str, Fail> {
    std::str::from_utf8(&output.stdout).map_err(|_| failure("received invalid command output"))
}

fn stdout_path(output: &Output) -> Result<&Path, Fail> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let bytes = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    if bytes.is_empty() {
        return Err(workspace_required());
    }
    Ok(Path::new(OsStr::from_bytes(bytes)))
}

pub(super) fn jj_commit(commit: &str) -> String {
    format!("commit_id({commit})")
}

pub(super) fn git_commit(commit: &str) -> String {
    format!("{commit}^{{commit}}")
}

pub(super) fn object_id(value: &str) -> Result<&str, Fail> {
    if matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(value)
    } else {
        Err(failure("received an invalid commit ID"))
    }
}

fn github_url(url: &str) -> Option<RepoSetting> {
    [
        "https://github.com/",
        "ssh://git@github.com/",
        "git@github.com:",
    ]
    .into_iter()
    .any(|prefix| url.starts_with(prefix))
    .then(|| RepoSetting::parse(url).ok())
    .flatten()
}

fn spawn_failure(program: &str, error: std::io::Error) -> Fail {
    if error.kind() == std::io::ErrorKind::NotFound {
        failure(format!(
            "{program} is required to publish; install {program} and try again"
        ))
    } else {
        failure(format!("could not run {program} for publication"))
    }
}

fn workspace_required() -> Fail {
    failure("run dots publish from a colocated JJ workspace of the configuration repository")
}

pub(super) fn stale() -> Fail {
    failure(
        "the source or remote main changed while awaiting approval; review the new diff and try again",
    )
}
