mod repo;
mod scan;

use std::io::{self, BufRead, IsTerminal, Write};

use serde::Deserialize;
use tempfile::NamedTempFile;

use crate::config::Vars;
use crate::report;
use crate::run::{Outcome, Run, Sink};
use crate::{Args, Ctx, Fail};

use repo::Workspace;

const MERGE_DESCRIPTION: &str = "Merge main into unpublished setup stack";
const AGGREGATE_PREFIX: &str = "wip/dotfiles-all-";

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
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    let mut terminal = Terminal {
        input: (stdin.is_terminal() && stdout.is_terminal()).then_some(&mut input),
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
    let base_symbol = workspace.revisions("main@origin")?;
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
    let mut working = workspace.one_revision("@")?;
    let mut revision = match selection {
        Selection::Working => working.clone(),
        Selection::All => workspace.stack_head(&working)?,
        Selection::Bookmark(name) => workspace.local_bookmark(name)?,
    };
    workspace.refuse_shared_dev(&base, &revision)?;
    let mut outgoing = outgoing_commits(&workspace, &base, &revision)?;
    if matches!(selection, Selection::Working)
        && (outgoing.len() != 1
            || workspace.revisions(&format!("{working}-"))?.as_slice() != [base.as_str()])
    {
        return Err(failure(
            "default publishing requires exactly one unpublished working change directly above main@origin; use --all or --bookmark",
        ));
    }
    if outgoing.is_empty() {
        return Err(failure("there are no unpublished changes to publish"));
    }

    if matches!(selection, Selection::All)
        && !workspace
            .revisions(&format!("{base} ~ ::{revision}"))?
            .is_empty()
    {
        terminal.confirm("Merge main@origin into the current unpublished stack?")?;
        workspace.require_unchanged(&base, "@", &working)?;
        workspace.jj(
            &["new", "-m", MERGE_DESCRIPTION, &revision, &base],
            "merge main into the stack",
        )?;
        working = workspace.one_revision("@")?;
        revision = working.clone();
        outgoing = outgoing_commits(&workspace, &base, &revision)?;
    }

    scan::outgoing(&workspace, &base, &revision)?;
    let diff = workspace.jj(
        &["diff", "--git", "--from", &base, "--to", &revision],
        "preview changes",
    )?;
    if diff.stdout.is_empty() && matches!(selection, Selection::Working | Selection::All) {
        return Err(failure(
            "there are no file changes to publish against main@origin",
        ));
    }
    terminal.message("Secret scan: gitleaks found nothing in the commits to publish.\n")?;
    if terminal.input.is_some() {
        terminal.message("Proposed diff against main@origin:\n")?;
        terminal
            .output
            .write_all(&diff.stdout)
            .map_err(output_error)?;
    }

    let note = disclosure(
        vars.publish_on_behalf_of.as_deref(),
        ctx.env
            .get("DOTS_PUBLISH_MODEL")
            .and_then(|model| model.to_str()),
    );
    let (title, body) = if matches!(selection, Selection::All) {
        stack_pull_request(&workspace, &outgoing, &note)?
    } else {
        let description = workspace.description(&revision)?;
        let description = if description.is_empty() && matches!(selection, Selection::Working) {
            terminal.ask("Enter a one-line description for the pull request")?
        } else {
            description
        };
        if description.is_empty() {
            return Err(failure("publication requires a one-line description"));
        }
        (title_for(&description), format!("{note}{description}"))
    };

    let (name, existing) = match selection {
        Selection::Bookmark(name) => (name.to_owned(), open_request(&workspace, name)?),
        Selection::All => aggregate_request(&workspace, &base, &revision)?,
        Selection::Working => {
            let name = format!("wip/dotfiles-{}", workspace.change_id(&revision)?);
            let request = open_request(&workspace, &name)?;
            (name, request)
        }
    };
    terminal.message(&format!(
        "Proposed pull request title: {title}\nProposed pull request description:\n{body}\n"
    ))?;
    terminal.confirm(&format!("Publish {name} to {}?", workspace.slug))?;

    match selection {
        Selection::Working | Selection::All => workspace.require_unchanged(&base, "@", &working)?,
        Selection::Bookmark(name) => {
            workspace.require_unchanged(&base, &Workspace::bookmark_expression(name), &revision)?;
        }
    }
    workspace.validate_origin()?;
    // The remote can advance or lose main while approval is pending.
    if workspace.fetch_main()? != base {
        return Err(repo::stale());
    }
    workspace.refuse_shared_dev(&base, &revision)?;
    let origin = workspace.validate_origin()?;
    if !matches!(selection, Selection::Bookmark(_)) {
        workspace.save_bookmark(&name, &revision)?;
    }
    workspace.push(&origin, &name, &revision)?;

    let url = write_pull_request(&workspace, &name, &title, &body, existing.as_ref()).map_err(
        |error| {
            if run.cancelled() {
                error
            } else {
                failure(format!(
                    "{}; pushed {name}; rerun the same dots publish command to retry the pull request",
                    error.diagnostic
                ))
            }
        },
    )?;
    terminal.message(&format!("Published {name}: {url}\n"))?;
    if !matches!(selection, Selection::Bookmark(_)) && workspace.one_revision("@")? == revision {
        workspace.jj(&["new", &revision], "create an empty working change")?;
    }
    Ok(())
}

fn outgoing_commits(
    workspace: &Workspace<'_, '_>,
    base: &str,
    head: &str,
) -> Result<Vec<String>, Fail> {
    let range = format!("{base}..{head}");
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
#[serde(rename_all = "camelCase")]
struct PullRequest {
    number: u64,
    state: String,
    url: String,
    is_cross_repository: bool,
}

fn find_request(workspace: &Workspace<'_, '_>, name: &str) -> Result<Option<PullRequest>, Fail> {
    let result = workspace.gh(
        "list",
        &[
            "--head",
            name,
            "--state",
            "all",
            "--json",
            "number,url,state,isCrossRepository",
        ],
        "look up the pull request",
    )?;
    let requests: Vec<PullRequest> = serde_json::from_slice(&result.stdout)
        .map_err(|_| failure("gh returned an invalid pull request lookup"))?;
    let mut own = requests
        .into_iter()
        .filter(|request| !request.is_cross_repository);
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
        && pr.state != "OPEN"
    {
        return Err(failure(
            "the bookmark's pull request is closed; choose another bookmark",
        ));
    }
    Ok(request)
}

fn aggregate_request(
    workspace: &Workspace<'_, '_>,
    base: &str,
    head: &str,
) -> Result<(String, Option<PullRequest>), Fail> {
    let mut prior = None;
    for name in workspace.aggregate_bookmarks(base, head)? {
        if let Some(request) = find_request(workspace, &name)?
            && request.state == "OPEN"
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
    input: Option<&'a mut dyn BufRead>,
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
        write!(self.output, "{label} ").map_err(output_error)?;
        self.output.flush().map_err(output_error)?;
        let mut answer = String::new();
        input
            .read_line(&mut answer)
            .map_err(|_| failure("could not read publication confirmation"))?;
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
