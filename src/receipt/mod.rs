mod child;
mod patch;
mod store;

use std::ffi::OsString;
use std::io::{self, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::run::Outcome;
use crate::{Args, Ctx, Fail};

#[derive(Clone, Copy, Debug)]
pub enum Display {
    Standalone,
    Plain,
    Embedded,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Receipt {
    #[serde(with = "store::optional_path")]
    pub checkout: Option<PathBuf>,
    /// Summary of the verified push and local-save result.
    pub outcome: String,
    pub issues: Vec<Issue>,
    pub shared: Vec<ChangeGroup>,
    pub saved: Vec<ChangeGroup>,
    pub updated: Vec<ChangeGroup>,
    pub recovered: Vec<ChangeGroup>,
    pub kept: usize,
    pub pulled_dev: usize,
    pub pulled_main: usize,
    pub local_only: bool,
    pub stage_outcome: Outcome,
    /// Private destinations where moved originals remain, including after a successful write.
    #[serde(with = "store::paths")]
    pub recovery_files: Vec<PathBuf>,
    pub adopted_revision: Option<(String, String)>,
}

impl Receipt {
    pub fn new(checkout: Option<PathBuf>, outcome: &'static str) -> Self {
        Self {
            checkout,
            outcome: outcome.into(),
            issues: Vec::new(),
            shared: Vec::new(),
            saved: Vec::new(),
            updated: Vec::new(),
            recovered: Vec::new(),
            kept: 0,
            pulled_dev: 0,
            pulled_main: 0,
            local_only: false,
            stage_outcome: Outcome::Completed,
            recovery_files: Vec::new(),
            adopted_revision: None,
        }
    }

    fn groups(&self) -> [(&'static str, &[ChangeGroup]); 4] {
        [
            ("Shared on dev", &self.shared),
            ("Saved locally", &self.saved),
            ("Updated in this checkout", &self.updated),
            ("Finished pending checkout", &self.recovered),
        ]
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Issue {
    pub diagnostic: String,
    pub next_action: String,
}

impl Issue {
    /// Only pass a sanitized diagnostic and action, never backend output.
    pub fn new(diagnostic: &'static str, next_action: &'static str) -> Self {
        Self {
            diagnostic: diagnostic.into(),
            next_action: next_action.into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChangeGroup {
    pub before: Option<String>,
    pub after: Option<String>,
    #[serde(with = "store::paths")]
    pub paths: Vec<PathBuf>,
}

impl ChangeGroup {
    /// Endpoints are verified full commit IDs and paths are relative to the checkout.
    pub fn new(before: Option<String>, after: Option<String>, paths: Vec<PathBuf>) -> Self {
        Self {
            before,
            after,
            paths,
        }
    }
}

pub fn emit(ctx: &Ctx, receipt: &Receipt, display: Display) -> Result<(), Fail> {
    let root = ctx.root.path();
    let saved = store::save(&ctx.env, root, receipt);
    let mut output = metadata(receipt, false);
    let screened =
        if patch_eligible(ctx, display) && matches!(receipt.stage_outcome, Outcome::Completed) {
            let screened = patch::render(
                &ctx.env,
                root,
                receipt,
                false,
                patch::MAX_BYTES - output.len().min(patch::MAX_BYTES),
            );
            output.push_str(&screened.text);
            Some(screened)
        } else {
            None
        };
    write_stdout(&output);
    drop(screened);
    saved.map_err(|error| {
        Fail::new(
            1,
            format!("could not save the sync receipt for replay: {error}"),
        )
    })
}

pub(crate) fn startup_failure(error: &Fail) {
    let mut receipt = Receipt::new(None, "not pushed");
    receipt.stage_outcome = Outcome::Deferred;
    receipt.issues.push(Issue {
        diagnostic: format!("Could not start sync: {error}"),
        next_action: "Run from an existing directory with DOTS_ROOT set to the configuration checkout, then rerun dots sync.".into(),
    });
    write_stdout(&metadata(&receipt, false));
    warn_save_failure();
}

pub(crate) fn warn_save_failure() {
    let _ = writeln!(
        io::stderr().lock(),
        "dots sync: could not save the sync receipt for replay"
    );
}

pub(crate) fn no_receipt() {
    write_stdout("No sync receipt for this checkout.\n");
}

pub fn last(ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    let root = ctx.root.path();
    let receipt = match store::load(&ctx.env, root) {
        Ok(Some(receipt)) => receipt,
        Ok(None) => {
            no_receipt();
            return Ok(0);
        }
        Err(error) => {
            let _ = writeln!(
                io::stderr().lock(),
                "dots sync: could not read the latest receipt: {error}"
            );
            no_receipt();
            return Ok(0);
        }
    };
    let metadata = metadata(&receipt, true);
    let mut screened = None;
    if patch_eligible(ctx, Display::Standalone) {
        let (mut output, truncated) = pager_metadata(&metadata);
        if !truncated {
            let remaining = patch::MAX_BYTES - output.len();
            let patches = patch::render(&ctx.env, root, &receipt, true, remaining);
            output.push_str(&patches.text);
            screened = Some(patches);
        }
        if page(&ctx.env, &output) {
            return Ok(0);
        }
    }
    write_stdout(&metadata);
    drop(screened);
    Ok(0)
}

fn pager_metadata(metadata: &str) -> (String, bool) {
    const LIMIT: usize = patch::MAX_BYTES - 128;
    if metadata.len() <= LIMIT {
        return (metadata.to_owned(), false);
    }
    let prefix = &metadata[..metadata.floor_char_boundary(LIMIT)];
    let mut output = prefix
        .rfind('\n')
        .map_or("", |end| &prefix[..=end])
        .to_owned();
    output.push_str("Additional receipt metadata omitted.\n");
    (output, true)
}

fn patch_eligible(ctx: &Ctx, display: Display) -> bool {
    matches!(display, Display::Standalone)
        && !ctx.flags.plain
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && child::leaders_waitable()
}

fn write_stdout(text: &str) {
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

fn page(env: &crate::Env, text: &str) -> bool {
    let Ok(mut child) = pager(env).spawn() else {
        return false;
    };
    let written = child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(text.as_bytes());
    let completed = child.wait().is_ok_and(|status| status.success());
    completed
        && (written.is_ok()
            || written.is_err_and(|error| error.kind() == io::ErrorKind::BrokenPipe))
}

fn pager(env: &crate::Env) -> Command {
    let mut command = Tools::new(env).command("less");
    command
        .args(["-FX"])
        .env("LESSSECURE", "1")
        .env("LESSHISTFILE", "-")
        .stdin(Stdio::piped());
    command
}

struct Tools(Vec<(&'static str, OsString)>);

impl Tools {
    fn new(env: &crate::Env) -> Self {
        Self(
            ["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "TMPDIR"]
                .into_iter()
                .filter_map(|key| Some((key, env.get(key)?.to_owned())))
                .collect(),
        )
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command.env_clear().stdin(Stdio::null());
        command.envs(self.0.iter().map(|(key, value)| (*key, value)));
        command
    }

    fn with_scratch_home(&self, directory: &Path) -> Self {
        let mut vars = self.0.clone();
        vars.extend(
            [
                ("HOME", "home"),
                ("XDG_CONFIG_HOME", "config"),
                ("XDG_DATA_HOME", "data"),
                ("XDG_STATE_HOME", "state"),
                ("XDG_CACHE_HOME", "cache"),
            ]
            .map(|(key, path)| (key, directory.join(path).into_os_string())),
        );
        Self(vars)
    }
}

fn printable(text: &str) -> String {
    let mut printable = String::new();
    for c in text.chars() {
        if terminal_control(c) {
            printable.extend(c.escape_default());
        } else {
            printable.push(c);
        }
    }
    printable
}

pub(crate) fn terminal_control(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

fn path_label(path: &Path) -> String {
    match path.to_str() {
        Some(path) => printable(&path.replace('\\', "\\\\")),
        None => path
            .as_os_str()
            .as_encoded_bytes()
            .escape_ascii()
            .to_string(),
    }
}

fn metadata(receipt: &Receipt, replay: bool) -> String {
    use std::fmt::Write as _;

    let mut text = String::new();
    if replay {
        text.push_str("Latest sync\n");
    }
    for issue in &receipt.issues {
        let _ = writeln!(text, "{}", printable(&issue.diagnostic));
        let _ = writeln!(text, "  {}", printable(&issue.next_action));
    }
    let outcome = printable(&receipt.outcome);
    text.push_str("dots sync: ");
    if !receipt.local_only {
        let saved = receipt
            .saved
            .iter()
            .map(|group| group.paths.len())
            .sum::<usize>();
        let _ = write!(
            text,
            "saved {saved} locally · kept {} files on disk · pulled dev +{}, main +{} · ",
            receipt.kept, receipt.pulled_dev, receipt.pulled_main
        );
    }
    let _ = writeln!(text, "{outcome}");
    for path in &receipt.recovery_files {
        let _ = writeln!(text, "Recovery copy at {}", path_label(path));
    }
    for (label, groups) in receipt.groups() {
        let paths = groups.iter().flat_map(|group| &group.paths);
        let count = paths.clone().count();
        if count == 0 {
            continue;
        }
        let _ = writeln!(text, "{label}");
        let limit = if replay { count } else { 5 };
        for path in paths.take(limit) {
            let _ = writeln!(text, "  {}", path_label(path));
        }
        if count > limit {
            let _ = writeln!(text, "  … and {} more", count - limit);
        }
    }
    text
}

#[cfg(test)]
mod tests;
