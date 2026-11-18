use std::io::{self, IsTerminal, Write};
use std::process::{Output, Stdio};

use crate::Fail;
use crate::config::RepoSetting;
use crate::process::Cwd;

use super::command_line;
use super::git::{Git, trim_line};

pub(super) fn configure(
    git: &Git<'_>,
    configured: Option<&RepoSetting>,
    created_seed: bool,
) -> Result<(), Fail> {
    match git.has_origin() {
        Ok(true) => {
            println!("kept remote origin");
            return Ok(());
        }
        Ok(false) => {}
        Err(error) => {
            println!("Could not check origin ({error}). The local configuration is ready.");
            return Ok(());
        }
    }
    let seed = match git.pristine() {
        Ok(seed) => seed,
        Err(error) => {
            println!("Could not verify the seed ({error}).");
            None
        }
    };
    let Some(seed) = seed else {
        println!();
        println!(
            "No remote yet. Add an empty repository, then run dots sync on dev to scan and share the configuration."
        );
        command_line("git -C ", git.root, " remote add origin URL")?;
        return command_line("DOTS_ROOT=", git.root, " dots sync");
    };
    if created_seed
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && let Some(account) = github_account(git)
    {
        let repo = format!("{account}/dotfiles");
        let url = format!("https://github.com/{repo}.git");
        let expected = RepoSetting::parse(&repo)?;
        if configured.is_some_and(|configured| !configured.matches(&url)) {
            println!("The configured source repository differs from {repo}.");
        } else if accept(&repo)? {
            return create_and_push(git, &repo, &url, &expected, &seed);
        }
    }

    println!();
    println!("No remote yet. To share this configuration, create an empty repository and run");
    command_line("git -C ", git.root, " remote add origin URL")?;
    print_push(git, &seed)?;
    explain_seed();
    Ok(())
}

fn gh(git: &Git<'_>, args: &[&str]) -> Option<Output> {
    git.ctx
        .command_with_profile("gh", Cwd::Root, git.profile.as_ref())
        .ok()?
        .env("GH_HOST", "github.com")
        .env("GH_PROMPT_DISABLED", "1")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
}

fn github_account(git: &Git<'_>) -> Option<String> {
    gh(git, &["auth", "status", "--hostname", "github.com"])?;
    let result = gh(
        git,
        &["api", "--hostname", "github.com", "user", "--jq", ".login"],
    )?;
    let login = std::str::from_utf8(trim_line(&result.stdout)).ok()?;
    (!login.is_empty()
        && login.as_bytes().first()?.is_ascii_alphanumeric()
        && login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-'))
    .then(|| login.to_owned())
}

fn accept(repo: &str) -> Result<bool, Fail> {
    print!("\nCreate a private GitHub repository {repo} and push the seed to it? [y/N] ");
    io::stdout().flush().map_err(prompt_error)?;
    let mut answer = String::new();
    // EOF or a failed read is a decline.
    let _ = io::stdin().read_line(&mut answer);
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn prompt_error(err: io::Error) -> Fail {
    Fail::new(
        1,
        format!("could not print the optional remote offer: {err}"),
    )
}

fn create_and_push(
    git: &Git<'_>,
    repo: &str,
    url: &str,
    expected: &RepoSetting,
    seed: &str,
) -> Result<(), Fail> {
    if gh(git, &["repo", "create", repo, "--private"]).is_none() {
        println!(
            "GitHub did not confirm that it created {repo}. Check that the repository exists and is empty before running"
        );
        command_line("git -C ", git.root, &format!(" remote add origin '{url}'"))?;
        print_push(git, seed)?;
        explain_seed();
        return Ok(());
    }
    println!("created private GitHub repository {repo}");

    let added = git
        .output(&["remote", "add", "origin", url])
        .is_ok_and(|out| out.status.success());
    if !added {
        match git.has_origin() {
            Ok(false) => {
                println!("Could not add origin. Add the repository and push the seed with");
                command_line("git -C ", git.root, &format!(" remote add origin '{url}'"))?;
                print_push(git, seed)?;
                explain_seed();
                return Ok(());
            }
            Ok(true) => {
                println!("Git reported a failure while adding origin, but origin exists.");
            }
            Err(_) => {
                println!(
                    "Git reported a failure while adding origin. Check the local remote configuration."
                );
                return Ok(());
            }
        }
    } else {
        println!("added remote origin");
    }

    if !effective_urls_match(git, expected) {
        println!(
            "Git resolves origin to a different or invalid repository. Correct its URL or your Git URL rewrites before running"
        );
    } else if !added {
        println!("The seed push still needs to run");
    } else if git
        .output(&[
            "push",
            "--no-follow-tags",
            "--recurse-submodules=no",
            "origin",
            &format!("{seed}:refs/heads/main"),
            &format!("{seed}:refs/heads/dev"),
        ])
        .is_ok_and(|out| out.status.success())
    {
        println!("pushed seed commit {seed} to origin/main and origin/dev");
        return Ok(());
    } else {
        println!("Could not finish pushing the seed to origin. Retry with");
    }
    print_push(git, seed)?;
    explain_seed();
    Ok(())
}

fn effective_urls_match(git: &Git<'_>, expected: &RepoSetting) -> bool {
    [
        &["remote", "get-url", "--all", "origin"][..],
        &["remote", "get-url", "--push", "--all", "origin"][..],
    ]
    .iter()
    .all(|args| {
        let Ok(out) = git.output(args) else {
            return false;
        };
        let Ok(urls) = std::str::from_utf8(trim_line(&out.stdout)) else {
            return false;
        };
        out.status.success() && !urls.is_empty() && urls.lines().all(|url| expected.matches(url))
    })
}

fn print_push(git: &Git<'_>, seed: &str) -> Result<(), Fail> {
    command_line(
        "git -C ",
        git.root,
        &format!(
            " push --no-follow-tags --recurse-submodules=no origin {seed}:refs/heads/main {seed}:refs/heads/dev"
        ),
    )
}

fn explain_seed() {
    println!(
        "These publish only the seed. Later changes are shared with dots sync, which scans them first."
    );
}
