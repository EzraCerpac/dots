mod repo;
mod scan;
mod scanner;

use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};

use nix::libc;
use serde::Deserialize;
use tempfile::NamedTempFile;

use crate::config::Vars;
use crate::report;
use crate::run::{Outcome, Run, Sink};
use crate::{Args, Ctx, Fail};

use repo::{Workspace, jj_commit};

const MERGE_DESCRIPTION: &str = "Merge main into unpublished setup stack";
const AGGREGATE_PREFIX: &str = "wip/dotfiles-all-";
const CLEAN_SCAN: &str = "Secret scan: gitleaks found nothing in the commits to publish.\n";

enum Selection<'a> {
    Working,
    All,
    Bookmark(&'a str),
}

pub fn publish(ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    let selection = if let Some(name) = args.get_one::<String>("bookmark") {
        Selection::Bookmark(name)
    } else if args.get_flag("all") {
        Selection::All
    } else {
        Selection::Working
    };
    let run = Run::begin(ctx, Sink::Report(report::Command::Publish));
    let stage = run.start("Publish source");
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut terminal = Terminal {
        input: (stdin.is_terminal() && stdout.is_terminal()).then(|| {
            BufReader::new(PromptInput {
                run: &run,
                stdin: stdin.as_fd(),
            })
        }),
        output: &mut output,
    };
    let result = publish_source(ctx, &run, selection, &mut terminal);
    let status = result.as_ref().err().map_or(0, |error| error.status);
    run.finish(
        stage,
        match status {
            0 => Outcome::Completed,
            3 => Outcome::Deferred,
            _ => Outcome::Failed,
        },
    );
    let status_after_cleanup = run.end(status);
    match result {
        Err(error) if status_after_cleanup == status => Err(error),
        _ => Ok(status_after_cleanup),
    }
}

fn publish_source(
    ctx: &Ctx,
    run: &Run<'_>,
    selection: Selection<'_>,
    terminal: &mut Terminal<'_>,
) -> Result<(), Fail> {
    let vars = Vars::read(&ctx.root)?;
    let workspace = Workspace::open(ctx, run, vars.source_repo.as_ref())?;
    let _locks = workspace.lock()?;
    workspace.validate_origin()?;
    let base = workspace.fetch_main()?;
    let stack = (matches!(selection, Selection::All) && workspace.stack_needs_merge(&base)?)
        .then(|| workspace.stack_snapshot())
        .transpose()?;
    let selected = stack.as_ref().map_or(&workspace, |stack| &stack.view);
    let base_symbol = selected.revisions("main@origin")?;
    if base_symbol.as_slice() != [base.as_str()] {
        return Err(failure(
            "JJ could not resolve the newly fetched main@origin",
        ));
    }

    if let Selection::Bookmark(name) = selection
        && !workspace.valid_bookmark(name)?
    {
        return Err(failure(
            "--bookmark requires an existing private wip/... bookmark",
        ));
    }
    let working = if let Some(stack) = &stack {
        stack.working.clone()
    } else {
        workspace.one_revision("@")?
    };
    let dev_target = match selection {
        Selection::Working | Selection::All => working.clone(),
        Selection::Bookmark(name) => workspace.local_bookmark(name)?,
    };
    selected.refuse_shared_dev(&base, &dev_target)?;
    let mut revision = if matches!(selection, Selection::All) {
        selected.stack_head(&working)?
    } else {
        dev_target.clone()
    };
    let mut outgoing = outgoing_commits(selected, &base, &revision)?;
    if matches!(selection, Selection::Working)
        && (outgoing.len() != 1
            || workspace
                .revisions(&format!("{}-", jj_commit(&working)))?
                .as_slice()
                != [base.as_str()])
    {
        return Err(failure(
            "default publishing requires exactly one unpublished working change directly above main@origin; use --all or --bookmark",
        ));
    }
    if outgoing.is_empty() {
        return Err(failure("there are no unpublished changes to publish"));
    }

    let merge = if matches!(selection, Selection::All)
        && !selected
            .revisions(&format!(
                "{} ~ ::{}",
                jj_commit(&base),
                jj_commit(&revision)
            ))?
            .is_empty()
    {
        if stack.is_none() {
            return Err(repo::stale());
        }
        let merge = selected.prepare_merge(&revision, &base)?;
        revision.clone_from(&merge.commit);
        outgoing =
            merge
                .view
                .revisions(&format!("{}..{}", jj_commit(&base), jj_commit(&revision)))?;
        Some(merge)
    } else {
        None
    };
    let plan = merge.as_ref().map_or(selected, |merge| &merge.view);
    let conflicts = merge.is_some()
        && !plan
            .revisions(&format!("{} & conflicts()", jj_commit(&revision)))?
            .is_empty();
    let screening = scan::outgoing(plan, &base, &revision, conflicts)?;
    let preview = if conflicts {
        None
    } else {
        let note = disclosure(
            vars.publish_on_behalf_of.as_deref(),
            ctx.env
                .get("DOTS_PUBLISH_MODEL")
                .and_then(|model| model.to_str()),
        );
        prepare_preview(
            plan, &selection, &base, &revision, &outgoing, &note, terminal,
        )?
    };
    if let Some(preview) = &preview {
        screening.screen_preview(plan, &preview.bytes)?;
    }
    drop(screening);

    let merged_state = if let (Some(stack), Some(merge)) = (&stack, &merge) {
        terminal.message(CLEAN_SCAN)?;
        terminal.confirm("Merge main@origin into the current unpublished stack?")?;
        workspace.validate_origin()?;
        if workspace.fetch_main()? != base {
            return Err(repo::stale());
        }
        workspace.apply_stack(stack, &base, Some(merge))?;
        if conflicts {
            return Err(Fail::new(
                3,
                "the changes to publish have conflicts; resolve them and rerun dots publish",
            ));
        }
        Some(workspace.stack_state()?)
    } else {
        None
    };

    let preview = preview
        .ok_or_else(|| failure("there are no file changes to publish against main@origin"))?;
    if merged_state.is_none() {
        terminal.message(CLEAN_SCAN)?;
    }
    let (name, existing) = match selection {
        Selection::Bookmark(name) => (name.to_owned(), open_request(&workspace, name)?),
        Selection::All => aggregate_request(plan, &revision)?,
        Selection::Working => {
            let name = format!("wip/dotfiles-{}", workspace.change_id(&revision)?);
            let request = open_request(&workspace, &name)?;
            (name, request)
        }
    };
    terminal
        .output
        .write_all(&preview.bytes)
        .map_err(output_error)?;
    terminal.confirm(&format!("Publish {name} to {}?", workspace.slug))?;

    match selection {
        Selection::Working => workspace.require_unchanged(&base, "@", &working)?,
        Selection::All if stack.is_none() => workspace.require_unchanged(&base, "@", &working)?,
        Selection::All => (),
        Selection::Bookmark(name) => {
            workspace.require_unchanged(&base, &Workspace::bookmark_expression(name), &revision)?;
        }
    }
    workspace.validate_origin()?;
    // The remote can advance or lose main while approval is pending.
    if workspace.fetch_main()? != base {
        return Err(repo::stale());
    }
    if dev_target != revision {
        selected.refuse_shared_dev(&base, &dev_target)?;
    }
    plan.refuse_shared_dev(&base, &revision)?;
    let origin = workspace.validate_origin()?;
    if let Some(state) = &merged_state {
        workspace.require_stack_unchanged(state, &base, &revision)?;
    }
    if let Some(request) = &existing
        && !find_request(&workspace, &name)?
            .is_some_and(|current| current.number == request.number && current.is_open())
    {
        return Err(failure(format!(
            "pull request #{} is no longer open for this bookmark; reopen it or choose another bookmark",
            request.number
        )));
    }
    if merged_state.is_none()
        && let Some(stack) = &stack
    {
        workspace.apply_stack(stack, &base, None)?;
    }
    if !matches!(selection, Selection::Bookmark(_)) {
        workspace.save_bookmark(&name, &revision)?;
    }
    workspace.push(&origin, &name, &revision)?;

    let url = write_pull_request(
        &workspace,
        &name,
        &preview.title,
        &preview.body,
        existing.as_ref(),
    )
    .map_err(|error| {
        if run.cancelled() {
            error
        } else {
            let recovery = match &existing {
                Some(request) => {
                    format!("retry editing pull request #{} on GitHub", request.number)
                }
                None => {
                    "use the pushed branch to open a pull request against main on GitHub".to_owned()
                }
            };
            failure(format!("{}; pushed {name}; {recovery}", error.diagnostic))
        }
    })?;
    terminal.message(&format!("Published {name}: {url}\n"))?;
    if !matches!(selection, Selection::Bookmark(_)) && workspace.one_revision("@")? == revision {
        workspace.jj(
            &["new", &jj_commit(&revision)],
            "create an empty working change",
        )?;
    }
    Ok(())
}

struct PublicationPreview {
    bytes: Vec<u8>,
    title: String,
    body: String,
}

fn prepare_preview(
    plan: &Workspace<'_, '_>,
    selection: &Selection<'_>,
    base: &str,
    revision: &str,
    outgoing: &[String],
    note: &str,
    terminal: &mut Terminal<'_>,
) -> Result<Option<PublicationPreview>, Fail> {
    let diff = plan.jj(
        &[
            "diff",
            "--git",
            "--from",
            &jj_commit(base),
            "--to",
            &jj_commit(revision),
        ],
        "prepare the pull request diff",
    )?;
    if diff.stdout.is_empty() && !matches!(selection, Selection::Bookmark(_)) {
        return Ok(None);
    }

    let (title, body) = if matches!(selection, Selection::All) {
        stack_pull_request(plan, outgoing, note)?
    } else {
        let mut description = plan.description(revision)?;
        if description.is_empty() && matches!(selection, Selection::Working) {
            description = terminal.ask("Enter a one-line description for the pull request")?;
        }
        if description.is_empty() {
            return Err(failure("publication requires a one-line description"));
        }
        (title_for(&description), format!("{note}{description}"))
    };

    // The scanner and terminal both consume this rendered preview.
    let mut bytes = Vec::new();
    if terminal.input.is_some() {
        writeln!(
            bytes,
            "Proposed changes, commit by commit (from main@origin):"
        )
        .map_err(output_error)?;
        for commit in outgoing {
            let description = plan.description(commit)?;
            let patch = plan.jj(
                &["diff", "--git", "-r", &jj_commit(commit)],
                "prepare an outgoing commit's preview",
            )?;
            writeln!(bytes, "\nCommit {}\n{description}", &commit[..12]).map_err(output_error)?;
            bytes.write_all(&patch.stdout).map_err(output_error)?;
        }
        writeln!(bytes, "\nFinal pull request diff against main@origin:").map_err(output_error)?;
        bytes.write_all(&diff.stdout).map_err(output_error)?;
    }
    writeln!(
        bytes,
        "Proposed pull request title: {title}\nProposed pull request description:\n{body}"
    )
    .map_err(output_error)?;
    Ok(Some(PublicationPreview {
        bytes: escape_preview(bytes),
        title,
        body,
    }))
}

fn escape_preview(bytes: Vec<u8>) -> Vec<u8> {
    let text = String::from_utf8_lossy(&bytes);
    let mut visible = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() && !matches!(character, '\n' | '\t') {
            visible.extend(character.escape_default());
        } else {
            visible.push(character);
        }
    }
    visible.into_bytes()
}

fn outgoing_commits(
    workspace: &Workspace<'_, '_>,
    base: &str,
    head: &str,
) -> Result<Vec<String>, Fail> {
    let head = jj_commit(head);
    let range = format!("{}..{head}", jj_commit(base));
    if !workspace
        .revisions(&format!("({range} | {head}) & conflicts()"))?
        .is_empty()
    {
        return Err(Fail::new(
            3,
            "the changes to publish have conflicts; resolve them and rerun dots publish",
        ));
    }
    workspace.revisions(&range)
}

fn stack_pull_request(
    workspace: &Workspace<'_, '_>,
    commits: &[String],
    note: &str,
) -> Result<(String, String), Fail> {
    let mut descriptions = Vec::new();
    for commit in commits.iter().rev() {
        let description = workspace.description(commit)?;
        if description != MERGE_DESCRIPTION {
            descriptions.push(if description.is_empty() {
                format!("Undescribed change {}", &commit[..12])
            } else {
                title_for(&description)
            });
        }
    }
    let title = match descriptions.as_slice() {
        [title] => title.clone(),
        descriptions => format!("Publish {} setup changes", descriptions.len()),
    };
    let mut body = format!("{note}Changes:\n");
    for description in &descriptions {
        body.push_str(&format!("- {description}\n"));
    }
    Ok((title, body))
}

#[derive(Deserialize)]
struct PullRequest {
    number: u64,
    state: String,
    #[serde(rename = "html_url")]
    url: String,
    head: PullRequestHead,
}

#[derive(Deserialize)]
struct PullRequestHead {
    #[serde(rename = "ref")]
    branch: String,
    repo: Option<PullRequestRepository>,
}

#[derive(Deserialize)]
struct PullRequestRepository {
    full_name: String,
}

impl PullRequest {
    fn is_open(&self) -> bool {
        self.state.eq_ignore_ascii_case("open")
    }
}

fn find_request(workspace: &Workspace<'_, '_>, name: &str) -> Result<Option<PullRequest>, Fail> {
    let result = workspace.gh_pull_requests(name)?;
    let pages: Vec<Vec<PullRequest>> = serde_json::from_slice(&result.stdout)
        .map_err(|_| failure("gh returned an invalid pull request lookup"))?;
    let mut own = pages.into_iter().flatten().filter(|request| {
        request.head.branch == name
            && request
                .head
                .repo
                .as_ref()
                .is_some_and(|repo| repo.full_name.eq_ignore_ascii_case(&workspace.slug))
    });
    let first = own.next();
    if own.next().is_some() {
        return Err(failure(
            "multiple pull requests use this bookmark; choose another bookmark",
        ));
    }
    if let Some(request) = &first
        && pull_number(&request.url, &workspace.slug) != Some(request.number)
    {
        return Err(failure("gh returned an invalid pull request URL"));
    }
    Ok(first)
}

fn open_request(workspace: &Workspace<'_, '_>, name: &str) -> Result<Option<PullRequest>, Fail> {
    let request = find_request(workspace, name)?;
    if let Some(pr) = &request
        && !pr.is_open()
    {
        return Err(failure(
            "the bookmark's pull request is closed; choose another bookmark",
        ));
    }
    Ok(request)
}

fn aggregate_request(
    workspace: &Workspace<'_, '_>,
    head: &str,
) -> Result<(String, Option<PullRequest>), Fail> {
    let mut prior = None;
    for name in workspace.aggregate_bookmarks(head)? {
        if let Some(request) = find_request(workspace, &name)?
            && request.is_open()
            && prior.replace((name, Some(request))).is_some()
        {
            return Err(failure(
                "multiple open stack pull requests found; choose one with --bookmark",
            ));
        }
    }
    if let Some(prior) = prior {
        return Ok(prior);
    }
    let name = format!("{AGGREGATE_PREFIX}{}", workspace.change_id(head)?);
    let request = open_request(workspace, &name)?;
    Ok((name, request))
}

fn write_pull_request(
    workspace: &Workspace<'_, '_>,
    name: &str,
    title: &str,
    body: &str,
    request: Option<&PullRequest>,
) -> Result<String, Fail> {
    let mut file = NamedTempFile::new()
        .map_err(|_| failure("could not create a private pull request body file"))?;
    file.write_all(body.as_bytes())
        .map_err(|_| failure("could not write the pull request body"))?;
    let path = file
        .path()
        .to_str()
        .ok_or_else(|| failure("could not pass the pull request body path to gh"))?;
    let common = ["--title", title, "--body-file", path];
    if let Some(request) = request {
        let number = request.number.to_string();
        workspace.gh(
            "edit",
            &[&[number.as_str()][..], &common].concat(),
            "update the pull request",
        )?;
        Ok(request.url.clone())
    } else {
        let result = workspace.gh(
            "create",
            &[&["--head", name, "--base", "main"][..], &common].concat(),
            "create the pull request",
        )?;
        let stdout = String::from_utf8_lossy(&result.stdout);
        stdout
            .split_ascii_whitespace()
            .find(|url| pull_number(url, &workspace.slug).is_some())
            .map(str::to_owned)
            .ok_or_else(|| failure("gh created the pull request without returning its URL"))
    }
}

fn pull_number(url: &str, slug: &str) -> Option<u64> {
    let (repository, number) = url
        .strip_prefix("https://github.com/")?
        .split_once("/pull/")?;
    if !repository.eq_ignore_ascii_case(slug) {
        return None;
    }
    number.parse::<u64>().ok().filter(|number| *number > 0)
}

struct Terminal<'a> {
    input: Option<BufReader<PromptInput<'a>>>,
    output: &'a mut dyn Write,
}

impl Terminal<'_> {
    fn message(&mut self, text: &str) -> Result<(), Fail> {
        self.output.write_all(text.as_bytes()).map_err(output_error)
    }

    fn ask(&mut self, label: &str) -> Result<String, Fail> {
        let input = self
            .input
            .as_mut()
            .ok_or_else(|| failure("publishing requires confirmation in a terminal"))?;
        input.get_ref().check_cancelled().map_err(input_error)?;
        write!(self.output, "{label} ").map_err(output_error)?;
        self.output.flush().map_err(output_error)?;
        let mut answer = String::new();
        input.read_line(&mut answer).map_err(input_error)?;
        input.get_ref().check_cancelled().map_err(input_error)?;
        Ok(answer.trim().to_owned())
    }

    fn confirm(&mut self, label: &str) -> Result<(), Fail> {
        let answer = self.ask(&format!("{label} [y/N]"))?;
        if matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes") {
            Ok(())
        } else {
            Err(failure("publication cancelled; nothing was pushed"))
        }
    }
}

struct PromptInput<'a> {
    run: &'a Run<'a>,
    stdin: BorrowedFd<'a>,
}

impl PromptInput<'_> {
    fn check_cancelled(&self) -> io::Result<()> {
        if self.run.cancelled() {
            // BufRead::read_line retries Interrupted, including when a
            // partial line was already read.
            return Err(io::Error::other("publication was interrupted"));
        }
        Ok(())
    }
}

impl Read for PromptInput<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let fd = self.stdin.as_raw_fd();
        if tty_flags(fd)? & libc::O_ACCMODE == libc::O_WRONLY {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        let mut input = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            self.check_cancelled()?;
            // A bounded wait covers signals delivered just before poll starts.
            let ready = unsafe { libc::poll(&mut input, 1, 100) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            if ready <= 0 {
                continue;
            }
            self.check_cancelled()?;
            match read_prompt_bytes(fd, bytes) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) => {}
                result => return result,
            }
        }
    }
}

fn read_prompt_bytes(fd: RawFd, bytes: &mut [u8]) -> io::Result<usize> {
    let flags = tty_flags(fd)?;
    let restore = flags & libc::O_NONBLOCK == 0;
    // Ctrl-C can flush the input after poll. Temporarily use a nonblocking
    // read and restore the caller's flags before returning to the wait loop.
    if restore {
        set_tty_flags(fd, flags | libc::O_NONBLOCK)?;
    }
    let count = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
    let result = if count < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(count as usize)
    };
    if restore {
        set_tty_flags(fd, flags)?;
    }
    result
}

fn tty_flags(fd: RawFd) -> io::Result<libc::c_int> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(flags)
    }
}

fn set_tty_flags(fd: RawFd, flags: libc::c_int) -> io::Result<()> {
    loop {
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } != -1 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn title_for(description: &str) -> String {
    description
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(72)
        .collect()
}

fn disclosure(person: Option<&str>, model: Option<&str>) -> String {
    let person = person.unwrap_or_default().trim();
    if person.is_empty() {
        return String::new();
    }
    let model = model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or("model unverified");
    let model = model.replace(['\r', '\n'], " ");
    let person = person.replace(['\r', '\n'], " ");
    format!("> [!NOTE]\n> **{model}** is writing on behalf of {person}.\n\n")
}

fn failure(message: impl Into<String>) -> Fail {
    Fail::new(1, message)
}

fn output_error(_: io::Error) -> Fail {
    failure("could not write publication output")
}

fn input_error(_: io::Error) -> Fail {
    failure("could not read publication confirmation")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{CStr, OsStr};
    use std::fs::{File, OpenOptions};
    use std::os::fd::FromRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::path::PathBuf;

    #[test]
    fn new_publish_cr120_preview_renders_control_characters_as_text() {
        let input = "first\tline\nnext\r\u{000b}\u{007f}\u{0085} café";
        assert_eq!(
            escape_preview(input.as_bytes().to_vec()),
            b"first\tline\nnext\\r\\u{b}\\u{7f}\\u{85} caf\xc3\xa9"
        );
        assert_eq!(escape_preview(b"a\xffb".to_vec()), "a\u{fffd}b".as_bytes());
    }

    fn open_pty() -> (File, File, PathBuf) {
        let (mut master, mut slave) = (-1, -1);
        let mut name = [0; libc::PATH_MAX as usize];
        unsafe {
            assert_eq!(
                libc::openpty(
                    &mut master,
                    &mut slave,
                    name.as_mut_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ),
                0
            );
            let path = OsStr::from_bytes(CStr::from_ptr(name.as_ptr()).to_bytes()).into();
            (File::from_raw_fd(master), File::from_raw_fd(slave), path)
        }
    }

    #[test]
    fn prompts_use_only_the_inherited_terminal() {
        let (mut master, _slave, path) = open_pty();
        let mut opts = OpenOptions::new();
        opts.custom_flags(libc::O_NOCTTY);
        let stdin = opts.clone().read(true).open(&path).unwrap();
        let write_only = opts.clone().write(true).open(&path).unwrap();
        let fd = stdin.as_raw_fd();
        let flags = tty_flags(fd).unwrap();
        assert_eq!(flags & (libc::O_NONBLOCK | libc::O_ACCMODE), libc::O_RDONLY);
        #[cfg(target_os = "linux")]
        {
            assert_eq!(unsafe { libc::ioctl(fd, libc::TIOCEXCL) }, 0);
            assert_eq!(
                opts.read(true).open(&path).unwrap_err().raw_os_error(),
                Some(libc::EBUSY)
            );
        }
        let (mut other_master, mut other_slave, _) = open_pty();
        other_master.write_all(b"yes\n").unwrap();

        let private = tempfile::tempdir().unwrap();
        let env = crate::Env {
            vars: [("XDG_STATE_HOME".into(), private.path().as_os_str().into())].into(),
            cwd: private.path().into(),
        };
        let ctx = Ctx {
            root: crate::config::Root::from_dir(&env, private.path()).unwrap(),
            env,
            flags: crate::Flags::default(),
        };
        let run = Run::begin(&ctx, Sink::Unreported);
        let mut terminal = Terminal {
            input: Some(BufReader::new(PromptInput {
                run: &run,
                stdin: stdin.as_fd(),
            })),
            output: &mut other_slave,
        };
        master.write_all("Résumé\nno\nyes\n".as_bytes()).unwrap();
        assert_eq!(terminal.ask("Description").unwrap(), "Résumé");
        let decline = terminal.confirm("Publish?").unwrap_err();
        assert_eq!(
            decline.diagnostic,
            "publication cancelled; nothing was pushed"
        );
        terminal.confirm("Publish?").unwrap();
        assert_eq!(tty_flags(fd).unwrap(), flags);
        master.write_all(b"\x04").unwrap();
        assert_eq!(terminal.ask("Description").unwrap(), "");
        assert_eq!(tty_flags(fd).unwrap(), flags);

        terminal.input = Some(BufReader::new(PromptInput {
            run: &run,
            stdin: write_only.as_fd(),
        }));
        master.write_all(b"yes\n").unwrap();
        let refusal = terminal.confirm("Publish?").unwrap_err();
        assert_eq!(
            refusal.diagnostic,
            "could not read publication confirmation"
        );
    }
}
