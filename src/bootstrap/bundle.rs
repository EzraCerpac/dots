use std::collections::{BTreeMap, HashSet};
use std::fs::{self, Permissions};
use std::io::{self, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;

use crate::process::Cwd;

use serde::{Deserialize, Serialize};
use serde_json::Number;

use super::{arg_path, bundle_pipe::BundlePipe};
use crate::report::Command as ReportCommand;
use crate::run::{Issue, Outcome, Run, Sink};
use crate::{Args, Ctx, Fail};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Bundle {
    version: Number,
    secrets: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) recipients: Vec<String>,
}

impl Bundle {
    fn parse(bytes: &[u8]) -> Result<Self, Fail> {
        if bytes.iter().copied().find(|b| !b.is_ascii_whitespace()) != Some(b'{') {
            return Err(Fail::new(1, "the bootstrap bundle must be a JSON object"));
        }
        // Do not expose serde's error text. It can include rejected secret values.
        let mut bundle: Self = serde_json::from_slice(bytes)
            .map_err(|_| Fail::new(1, "invalid bootstrap bundle document"))?;
        if bundle.version.as_f64() != Some(1.0)
            || bundle
                .secrets
                .values()
                .any(|value| value.is_empty() || value.contains('\0'))
            || bundle.recipients.iter().any(|key| !recipient(key))
        {
            return Err(Fail::new(
                1,
                "invalid version, secrets or recipients in bootstrap bundle",
            ));
        }
        let mut seen = HashSet::new();
        bundle.recipients.retain(|key| seen.insert(key.clone()));
        Ok(bundle)
    }

    pub(super) fn secrets_json(&self) -> Result<Vec<u8>, Fail> {
        serde_json::to_vec(&self.secrets)
            .map_err(|_| Fail::new(1, "could not prepare bootstrap secrets"))
    }
}

pub(super) fn recipient(value: &str) -> bool {
    value.strip_prefix("age1").is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    })
}

fn age(ctx: &Ctx, run: &Run<'_>) -> Result<Command, Fail> {
    // Bundle check and create work without a configuration or mise.
    ctx.command_with_profile(run.tool("age"), Cwd::Caller, "".as_ref())
}

pub(super) fn decrypt(
    ctx: &Ctx,
    run: &Run<'_>,
    file: &Path,
    identity: &Path,
) -> Result<Bundle, Fail> {
    let mut command = age(ctx, run)?;
    command
        .arg("--decrypt")
        .arg("--identity")
        .arg(identity)
        .arg("--")
        .arg(file);
    let output = run.probe(command).map_err(age_error)?;
    if run.cancelled() || !output.status.success() {
        return Err(Fail::new(
            1,
            "cannot decrypt the bootstrap bundle with the selected age identity",
        ));
    }
    Bundle::parse(&output.stdout)
}

fn create(
    ctx: &Ctx,
    run: &Run<'_>,
    input: &Path,
    output: &Path,
    recipients: &[&str],
) -> Result<(), Fail> {
    if recipients.is_empty() || recipients.iter().any(|key| !recipient(key)) {
        return Err(Fail::usage("supply at least one age public recipient"));
    }
    match fs::symlink_metadata(output) {
        Ok(_) => return Err(output_error(io::Error::from(io::ErrorKind::AlreadyExists))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(output_error(error)),
    }

    let document = Bundle::parse(
        &fs::read(input)
            .map_err(|_| Fail::new(1, "could not read the private bootstrap JSON input"))?,
    )?;
    let plaintext = serde_json::to_vec(&document)
        .map_err(|_| Fail::new(1, "could not prepare the private bootstrap JSON input"))?;
    let pipe = BundlePipe::new(&plaintext)?;
    let mut command = age(ctx, run)?;
    for key in recipients {
        command.args(["--recipient", key]);
    }
    let encrypted = run
        .probe_with_stdin(command, pipe.stdin()?)
        .map_err(age_error)?;
    if run.cancelled() || !encrypted.status.success() || encrypted.stdout.is_empty() {
        return Err(Fail::new(1, "could not encrypt the bootstrap bundle"));
    }

    let parent = output.parent().expect("absolute output path");
    let mut pending = tempfile::NamedTempFile::new_in(parent).map_err(output_error)?;
    pending
        .as_file()
        .set_permissions(Permissions::from_mode(0o600))
        .map_err(output_error)?;
    pending.write_all(&encrypted.stdout).map_err(output_error)?;
    pending
        .persist_noclobber(output)
        .map_err(|error| output_error(error.error))?;
    Ok(())
}

fn age_error(error: io::Error) -> Fail {
    if error.kind() == io::ErrorKind::NotFound {
        Fail::new(1, "age is required for bootstrap bundles")
    } else {
        Fail::new(
            1,
            format!("could not run age for the bootstrap bundle: {error}"),
        )
    }
}

fn output_error(error: io::Error) -> Fail {
    Fail::new(
        1,
        format!("could not create the encrypted bundle output: {error}"),
    )
}

pub fn bundle(ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    let (operation, args) = args.subcommand().expect("required by CLI");
    let run = Run::begin(ctx, Sink::Report(ReportCommand::Bundle));
    let stage = run.start(if operation == "create" {
        "Encrypt bundle"
    } else {
        "Validate bundle"
    });
    let result = match operation {
        "create" => {
            let input = arg_path(ctx, args, "input").expect("required by CLI");
            let output = arg_path(ctx, args, "output").expect("required by CLI");
            let recipients: Vec<_> = args
                .get_many::<String>("recipient")
                .into_iter()
                .flatten()
                .map(String::as_str)
                .collect();
            create(ctx, &run, &input, &output, &recipients)
        }
        "check" => {
            let bundle = arg_path(ctx, args, "bundle").expect("required by CLI");
            let identity = arg_path(ctx, args, "identity").expect("required by CLI");
            decrypt(ctx, &run, &bundle, &identity).map(|_| ())
        }
        _ => unreachable!("selected by CLI"),
    };
    if run.cancelled() {
        return Ok(run.end(1));
    }
    match result {
        Ok(()) => {
            run.finish(stage, Outcome::Completed);
            println!(
                "{}",
                if operation == "check" {
                    "Bootstrap bundle decrypts and validates."
                } else {
                    "Encrypted bootstrap bundle created."
                }
            );
            Ok(run.end(0))
        }
        Err(error) => {
            run.issue(
                stage,
                Issue::new(
                    error.to_string(),
                    "Check the bundle arguments and rerun dots bundle.",
                ),
            );
            run.finish(stage, Outcome::Failed);
            run.end(error.status);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_validates_shape_and_preserves_values_and_recipient_order() {
        let bundle = Bundle::parse(br#"{"version":1.0,"secrets":{"nonstandard key":"a\nb"},"recipients":["age1b","age1a","age1b"]}"#).unwrap();
        assert_eq!(bundle.secrets["nonstandard key"], "a\nb");
        assert_eq!(bundle.recipients, ["age1b", "age1a"]);
        for invalid in [
            r#"[1, {"fixture-sensitive-marker":"value"}, []]"#,
            r#"{"version":"1","secrets":{}}"#,
            r#"{"version":1,"secrets":[]}"#,
            r#"{"version":1,"secrets":{"a":""}}"#,
            r#"{"version":1,"secrets":{"a":"\u0000"}}"#,
            r#"{"version":1,"secrets":{"a":false}}"#,
            r#"{"version":1,"secrets":{},"recipients":null}"#,
            r#"{"version":1,"secrets":{},"recipients":["age1UPPER"]}"#,
            r#"{"version":1,"secrets":{},"unexpected":"fixture-secret"}"#,
        ] {
            let error = Bundle::parse(invalid.as_bytes()).err().unwrap();
            assert!(!error.to_string().contains("fixture-secret"));
        }
    }
}
