use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{self, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use crate::{
    Args, Ctx, Fail,
    process::Cwd,
    targets::{OwnedApp, safe_text},
};

use super::failure;

const APPLICATIONS: &str = "/Applications";
const MAX_PLIST: u64 = 1024 * 1024;

pub fn validate_owned_app(ctx: &Ctx, app: &OwnedApp) -> Result<(), Fail> {
    let applications = Path::new(APPLICATIONS);
    let state = marker_directory(ctx)?;
    validate_ownership(ctx, applications, &state, app)
}

fn validate_ownership(
    ctx: &Ctx,
    applications: &Path,
    state: &Path,
    app: &OwnedApp,
) -> Result<(), Fail> {
    let invalid = || {
        failure(format!(
            "invalid GitHub app ownership for {}; refusing to change it",
            app.path.display()
        ))
    };
    check_state(state)?;
    let filename = app.marker.file_name().ok_or_else(invalid)?;
    if app.marker.as_os_str() != state.join(filename).as_os_str() {
        return Err(invalid());
    }
    let (recorded, installation) =
        OwnedApp::read_marker(&app.marker, applications).map_err(ownership_error)?;
    if recorded != *app || recorded.path.as_os_str() != app.path.as_os_str() {
        return Err(invalid());
    }
    if let Some(meta) = metadata(&app.path)? {
        if let Some(installation) = installation
            && !installation.matches(&meta)
        {
            return Err(invalid());
        }
        if bundle_id(ctx, &app.path, true)? != app.bundle_id {
            return Err(failure(format!(
                "installed app identity differs at {}; refusing to change it",
                app.path.display()
            )));
        }
    }
    Ok(())
}

fn ownership_error(error: crate::targets::OwnershipError) -> Fail {
    failure(format!(
        "invalid GitHub app ownership {}: {}",
        error.path.display(),
        error.detail
    ))
}

fn marker_directory(ctx: &Ctx) -> Result<PathBuf, Fail> {
    Ok(ctx
        .env
        .state_dir()
        .map_err(|err| {
            failure(format!(
                "could not locate GitHub app ownership state: {err}"
            ))
        })?
        .join("github-apps"))
}

fn check_state(state: &Path) -> Result<(), Fail> {
    if metadata(state)?.is_some_and(|meta| !meta.is_dir()) {
        return Err(failure(format!(
            "invalid GitHub app ownership directory: {}",
            state.display()
        )));
    }
    Ok(())
}

fn bundle_id(ctx: &Ctx, root: &Path, verify_signature: bool) -> Result<String, Fail> {
    let invalid =
        |detail: &str| failure(format!("invalid app bundle {}: {detail}", root.display()));
    if !metadata(root)?.is_some_and(|meta| meta.is_dir()) {
        return Err(invalid("expected a real directory"));
    }

    let physical_root = fs::canonicalize(root).map_err(|err| path_error(root, err))?;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for path in children(&directory)? {
            let kind = fs::symlink_metadata(&path)
                .map_err(|err| path_error(&path, err))?
                .file_type();
            if kind.is_symlink() {
                let target = fs::canonicalize(&path).map_err(|err| path_error(&path, err))?;
                if !target.starts_with(&physical_root) {
                    return Err(invalid("symlink escapes the bundle"));
                }
            } else if kind.is_dir() {
                pending.push(path);
            } else if !kind.is_file() {
                return Err(invalid("contains an unsupported filesystem entry"));
            }
        }
    }

    let info_path = root.join("Contents/Info.plist");
    let info = File::open(&info_path).map_err(|err| path_error(&info_path, err))?;
    let mut bytes = Vec::new();
    info.take(MAX_PLIST + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| path_error(&info_path, err))?;
    if bytes.len() as u64 > MAX_PLIST {
        return Err(invalid("Info.plist exceeds the inspection limit"));
    }
    let plist = plist::Value::from_reader(io::Cursor::new(bytes))
        .map_err(|_| invalid("could not parse Info.plist"))?;
    let dict = plist
        .as_dictionary()
        .ok_or_else(|| invalid("Info.plist must be a dictionary"))?;
    let property = |key| {
        dict.get(key)
            .and_then(plist::Value::as_string)
            .filter(|text| !text.is_empty())
    };
    let executable = property("CFBundleExecutable")
        .filter(|name| !matches!(*name, "." | "..") && !name.contains(['/', '\\', '\0']))
        .ok_or_else(|| invalid("missing or invalid CFBundleExecutable"))?;
    let identifier = property("CFBundleIdentifier")
        .filter(|name| safe_text(name))
        .ok_or_else(|| invalid("missing or invalid CFBundleIdentifier"))?;
    let binary = root.join("Contents/MacOS").join(executable);
    if !fs::metadata(&binary)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    {
        return Err(invalid("bundle executable is missing or not executable"));
    }
    if verify_signature && metadata(&root.join("Contents/_CodeSignature"))?.is_some() {
        native(
            ctx,
            "/usr/bin/codesign",
            [
                "--verify".as_ref(),
                "--deep".as_ref(),
                "--strict".as_ref(),
                root.as_os_str(),
            ],
        )?;
    }
    Ok(identifier.to_owned())
}

fn metadata(path: &Path) -> Result<Option<fs::Metadata>, Fail> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(path_error(path, err)),
    }
}

fn children(path: &Path) -> Result<impl Iterator<Item = PathBuf>, Fail> {
    // Resolve all directory-entry errors rather than silently skipping files.
    fs::read_dir(path)
        .map_err(|err| path_error(path, err))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|err| path_error(path, err))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Vec::into_iter)
}

fn path_error(path: &Path, error: io::Error) -> Fail {
    failure(format!("{}: {error}", path.display()))
}

fn native(
    ctx: &Ctx,
    program: &str,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> Result<(), Fail> {
    // The post-install callback also runs when there is no dots configuration.
    let profile = ctx.env.get("DOTS_PROFILE").unwrap_or_default();
    let status = ctx
        .command_with_profile(program, Cwd::Caller, profile)?
        .args(args)
        .status()
        .map_err(|err| {
            failure(format!(
                "could not run {program} for the GitHub release app: {err}"
            ))
        })?;
    if !status.success() {
        return Err(failure(format!(
            "{program} failed while checking or installing the GitHub release app ({status})"
        )));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn install_release_app(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(failure("app installation requires macOS"))
}

#[cfg(target_os = "macos")]
pub use macos::install_release_app;

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        ffi::CString,
        os::unix::{ffi::OsStrExt, fs::MetadataExt},
        time::{SystemTime, UNIX_EPOCH},
    };

    use nix::libc;
    use serde_json::json;

    use crate::targets::{AppInstallation, github_repo, marker_filename};

    use super::*;

    pub fn install_release_app(ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
        let repo = args
            .get_one::<std::ffi::OsString>("OWNER/REPO")
            .and_then(|argument| argument.to_str())
            .and_then(github_repo)
            .ok_or_else(|| Fail::usage("use a GitHub repository in OWNER/REPO form"))?;
        let install_path = ctx
            .env
            .get("MISE_TOOL_INSTALL_PATH")
            .filter(|path| !path.is_empty())
            .map(Path::new)
            .ok_or_else(|| {
                failure(
                    "MISE_TOOL_INSTALL_PATH is missing; run dots install-release-app through mise",
                )
            })?;
        if !install_path.is_absolute() {
            return Err(failure("MISE_TOOL_INSTALL_PATH must be absolute"));
        }
        let app = find_app(install_path)?;
        let applications = Path::new(APPLICATIONS);
        publish(ctx, &app, &repo, applications, &marker_directory(ctx)?)?;
        eprintln!(
            "dots: installed {} from {repo}",
            applications.join(app.file_name().unwrap()).display()
        );
        Ok(0)
    }

    fn find_app(path: &Path) -> Result<PathBuf, Fail> {
        let invalid = || failure("installed release must contain exactly one app bundle");
        if !metadata(path)?.is_some_and(|meta| meta.is_dir()) {
            return Err(invalid());
        }
        let mut found = None;
        let mut pending = vec![path.to_owned()];
        while let Some(directory) = pending.pop() {
            let name = directory
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or_default();
            if name == "__MACOSX" {
                continue;
            }
            if app_name(name)
                && metadata(&directory.join("Contents"))?.is_some_and(|meta| meta.is_dir())
                && metadata(&directory.join("Contents/Info.plist"))?.is_some()
            {
                if found.replace(directory).is_some() {
                    return Err(invalid());
                }
                // An app's embedded helpers are part of the containing bundle.
                continue;
            }
            for child in children(&directory)? {
                if metadata(&child)?.is_some_and(|meta| meta.is_dir()) {
                    pending.push(child);
                }
            }
        }
        found.ok_or_else(invalid)
    }

    fn app_name(name: &str) -> bool {
        name.ends_with(".app") && !matches!(name, ".app" | "..app") && safe_text(name)
    }

    fn publish(
        ctx: &Ctx,
        source: &Path,
        repo: &str,
        applications: &Path,
        state: &Path,
    ) -> Result<(), Fail> {
        use std::io::Write;

        if !metadata(applications)?.is_some_and(|meta| meta.is_dir()) {
            return Err(failure(format!(
                "app install directory is missing or untrusted: {}",
                applications.display()
            )));
        }
        check_state(state)?;
        let name = source
            .file_name()
            .and_then(OsStr::to_str)
            .filter(|name| app_name(name))
            .ok_or_else(|| failure("invalid release app bundle name"))?;
        let identifier = bundle_id(ctx, source, false)?;
        let target = applications.join(name);
        let marker = state.join(marker_filename(name));
        let previous = OwnedApp::for_release(state, applications, repo, &identifier)
            .map_err(ownership_error)?;
        let previous = match previous.as_slice() {
            [] => None,
            [app] => Some(app),
            _ => {
                return Err(failure(
                    "multiple managed apps match this GitHub release; refusing to replace them",
                ));
            }
        };
        let previous_node = if let Some(app) = previous {
            validate_ownership(ctx, applications, state, app)?;
            metadata(&app.path)?.as_ref().map(node)
        } else {
            None
        };
        let same_marker = previous.is_some_and(|app| app.marker == marker);
        if metadata(&marker)?.is_some() && !same_marker
            || metadata(&target)?.is_some_and(|meta| Some(node(&meta)) != previous_node)
        {
            return Err(failure(format!(
                "{} already exists or has an ownership record outside this GitHub release",
                target.display()
            )));
        }
        let directory = tempfile::Builder::new()
            .prefix(".dots-github-")
            .tempdir_in(applications)
            .map_err(|err| path_error(applications, err))?;
        let stage = directory.path().join(name);
        native(
            ctx,
            "/usr/bin/ditto",
            [
                "--rsrc".as_ref(),
                "--extattr".as_ref(),
                source.as_os_str(),
                stage.as_os_str(),
            ],
        )?;
        native(ctx, "/usr/sbin/dot_clean", [stage.as_os_str()])?;
        if bundle_id(ctx, &stage, true)? != identifier {
            return Err(failure(
                "GitHub release app identity changed during staging",
            ));
        }
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| failure(err.to_string()))?
            .as_secs();
        let quarantine = format!("0081;{seconds:08x};dots;");
        native(
            ctx,
            "/usr/bin/xattr",
            [
                "-w".as_ref(),
                "com.apple.quarantine".as_ref(),
                quarantine.as_ref(),
                stage.as_os_str(),
            ],
        )?;

        let stage_meta = fs::symlink_metadata(&stage).map_err(|err| path_error(&stage, err))?;
        let staged_node = node(&stage_meta);
        let installation =
            AppInstallation::from_metadata(&stage_meta).map_err(|err| path_error(&stage, err))?;
        fs::create_dir_all(state).map_err(|err| path_error(state, err))?;
        let mut pending =
            tempfile::NamedTempFile::new_in(state).map_err(|err| path_error(state, err))?;
        serde_json::to_writer(
            &mut pending,
            &json!({
                "repo": repo,
                "path": target,
                "bundle_id": identifier,
                "installation": installation,
            }),
        )
        .map_err(|err| failure(format!("could not stage GitHub app ownership: {err}")))?;
        pending.flush().map_err(|err| path_error(state, err))?;
        pending
            .as_file()
            .sync_all()
            .map_err(|err| path_error(state, err))?;

        let backup = directory.path().join(".previous-app");
        let mut backed_up = None;
        let mut installed = false;
        let mut new_marker = false;
        let result = (|| {
            if let Some(app) = previous {
                validate_ownership(ctx, applications, state, app)?;
                if metadata(&app.path)?.as_ref().map(node) != previous_node {
                    return Err(failure(format!(
                        "{} changed during installation",
                        app.path.display()
                    )));
                }
                if previous_node.is_some() {
                    rename(&app.path, &backup, libc::RENAME_EXCL)
                        .map_err(|err| path_error(&app.path, err))?;
                    backed_up = Some(&app.path);
                    if metadata(&backup)?.as_ref().map(node) != previous_node {
                        return Err(failure(format!(
                            "{} changed during installation",
                            app.path.display()
                        )));
                    }
                }
            }
            rename(&stage, &target, libc::RENAME_EXCL).map_err(|err| path_error(&target, err))?;
            installed = true;
            if same_marker {
                pending
                    .persist(&marker)
                    .map_err(|err| path_error(&marker, err.error))?;
            } else {
                pending
                    .persist_noclobber(&marker)
                    .map_err(|err| path_error(&marker, err.error))?;
                new_marker = true;
                if let Some(app) = previous {
                    fs::remove_file(&app.marker).map_err(|err| path_error(&app.marker, err))?;
                }
            }
            Ok(())
        })();

        if let Err(issue) = result {
            let marker_restored = !new_marker || fs::remove_file(&marker).is_ok();
            let stage_restored = !installed
                || (metadata(&target)
                    .is_ok_and(|meta| meta.as_ref().map(node) == Some(staged_node))
                    && rename(&target, &stage, libc::RENAME_EXCL).is_ok());
            let previous_restored =
                backed_up.is_none_or(|path| rename(&backup, path, libc::RENAME_EXCL).is_ok());
            if marker_restored && stage_restored && previous_restored {
                return Err(issue);
            }
            let recovery = directory.keep();
            return Err(failure(format!(
                "{issue}; inspect {} and {} to recover the staged app",
                target.display(),
                recovery.display()
            )));
        }
        Ok(())
    }

    fn node(meta: &fs::Metadata) -> (u64, u64) {
        (meta.dev(), meta.ino())
    }

    fn rename(from: &Path, to: &Path, flags: libc::c_uint) -> io::Result<()> {
        let c_path =
            |path: &Path| CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other);
        let from = c_path(from)?;
        let to = c_path(to)?;
        // Keep an unrelated destination intact if it appears during publication.
        if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), flags) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::{Env, Flags, config::Root};
        use std::os::unix::fs::symlink;

        fn ctx(config: &Path) -> Ctx {
            let env = Env {
                vars: crate::packages::github::test_env::private_environment(
                    config, config, config,
                )
                .into_iter()
                .chain([("PATH".into(), "/usr/bin:/bin".into())])
                .collect(),
                cwd: config.to_owned(),
            };
            Ctx {
                root: Root::resolve(&env).unwrap(),
                env,
                flags: Flags::default(),
            }
        }

        fn bundle(root: &Path, identifier: &str, contents: &[u8]) {
            let macos = root.join("Contents/MacOS");
            fs::create_dir_all(&macos).unwrap();
            let binary = macos.join("fixture");
            fs::write(&binary, contents).unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
            fs::write(
                root.join("Contents/Info.plist"),
                format!(
                    r#"
                <?xml version="1.0" encoding="UTF-8"?>
                <plist version="1.0"><dict>
                <key>CFBundleIdentifier</key><string>{identifier}</string>
                <key>CFBundleExecutable</key><string>fixture</string>
                </dict></plist>"#
                )
                .trim(),
            )
            .unwrap();
        }

        #[test]
        fn selects_only_one_real_top_level_bundle() {
            let fixture = tempfile::tempdir().unwrap();
            let app = fixture.path().join("Tool.app");
            bundle(&app, "example.tool", b"tool");
            bundle(
                &app.join("Contents/Helpers/Tool Helper.app"),
                "example.helper",
                b"helper",
            );
            bundle(
                &fixture.path().join("__MACOSX/Extra.app"),
                "example.metadata",
                b"resource",
            );
            assert_eq!(find_app(fixture.path()).unwrap(), app);
            bundle(
                &fixture.path().join("Second.app"),
                "example.second",
                b"other",
            );
            assert!(find_app(fixture.path()).is_err());
        }

        #[test]
        fn installs_and_replaces_only_an_owned_app() {
            let fixture = tempfile::tempdir().unwrap();
            let applications = fixture.path().join("Applications");
            let state = fixture.path().join("state");
            let source = fixture.path().join("release/Tool.app");
            let target = applications.join("Tool.app");
            let marker = state.join(marker_filename("Tool.app"));
            fs::create_dir(&applications).unwrap();
            let ctx = ctx(fixture.path());
            bundle(&source, "example.tool", b"new");
            bundle(&target, "example.tool", b"unmanaged");
            assert!(publish(&ctx, &source, "owner/tool", &applications, &state).is_err());
            assert_eq!(
                fs::read(target.join("Contents/MacOS/fixture")).unwrap(),
                b"unmanaged"
            );
            assert!(!marker.exists());
            fs::remove_dir_all(&target).unwrap();
            publish(&ctx, &source, "owner/tool", &applications, &state).unwrap();
            let owned = OwnedApp {
                repo: "owner/tool".to_owned(),
                path: target.clone(),
                bundle_id: "example.tool".to_owned(),
                marker,
            };
            validate_ownership(&ctx, &applications, &state, &owned).unwrap();
            assert!(publish(&ctx, &source, "different/tool", &applications, &state).is_err());
            bundle(&source, "example.different", b"different");
            assert!(publish(&ctx, &source, "owner/tool", &applications, &state).is_err());
            bundle(&source, "example.tool", b"upgrade");
            publish(&ctx, &source, "owner/tool", &applications, &state).unwrap();
            assert_eq!(
                fs::read(target.join("Contents/MacOS/fixture")).unwrap(),
                b"upgrade"
            );
            let trash = fixture.path().join("Trash");
            fs::create_dir(&trash).unwrap();
            fs::rename(&target, trash.join("Tool.app")).unwrap();
            bundle(&target, "example.tool", b"manual replacement");
            assert!(validate_ownership(&ctx, &applications, &state, &owned).is_err());
            assert!(publish(&ctx, &source, "owner/tool", &applications, &state).is_err());
            assert_eq!(
                fs::read(target.join("Contents/MacOS/fixture")).unwrap(),
                b"manual replacement"
            );
            fs::remove_dir_all(&target).unwrap();
            symlink(&source, &target).unwrap();
            assert!(validate_ownership(&ctx, &applications, &state, &owned).is_err());
        }

        #[test]
        fn unchanged_legacy_marker_allows_owned_upgrade_and_rename() {
            for replacement in ["Tool.app", "Tool Renamed.app", "tool.app"] {
                let fixture = tempfile::tempdir().unwrap();
                let applications = fixture.path().join("Applications");
                let state = fixture.path().join("markers");
                let old = applications.join("Tool.app");
                let marker = state.join(marker_filename("Tool.app"));
                fs::create_dir_all(&state).unwrap();
                bundle(&old, "example.tool", b"legacy");
                let legacy = serde_json::to_vec(&json!({
                    "repo": "Owner/Tool",
                    "path": old,
                    "bundle_id": "example.tool",
                }))
                .unwrap();
                fs::write(&marker, &legacy).unwrap();
                let ctx = ctx(fixture.path());
                let (owned, proof) = OwnedApp::read_marker(&marker, &applications).unwrap();
                assert!(proof.is_none());
                validate_ownership(&ctx, &applications, &state, &owned).unwrap();
                assert_eq!(fs::read(&marker).unwrap(), legacy);

                let source = fixture.path().join("release").join(replacement);
                bundle(&source, "example.tool", b"upgrade");
                publish(&ctx, &source, "owner/tool", &applications, &state).unwrap();
                let expected = applications.join(replacement);
                let entries = children(&applications).unwrap().collect::<Vec<_>>();
                assert_eq!(entries, [expected.clone()]);
                let new_marker = state.join(marker_filename(replacement));
                let (owned, proof) = OwnedApp::read_marker(&new_marker, &applications).unwrap();
                assert!(proof.is_some());
                validate_ownership(&ctx, &applications, &state, &owned).unwrap();
                assert_eq!(
                    fs::read(expected.join("Contents/MacOS/fixture")).unwrap(),
                    b"upgrade"
                );
                assert_eq!(children(&state).unwrap().collect::<Vec<_>>(), [new_marker]);
            }
        }

        #[test]
        fn refuses_ambiguous_rename_without_replacing_either_app() {
            let fixture = tempfile::tempdir().unwrap();
            let applications = fixture.path().join("Applications");
            let state = fixture.path().join("markers");
            fs::create_dir_all(&state).unwrap();
            for name in ["One.app", "Two.app"] {
                let path = applications.join(name);
                bundle(&path, "example.tool", name.as_bytes());
                fs::write(
                    state.join(marker_filename(name)),
                    serde_json::to_vec(&json!({
                        "repo": "owner/tool", "path": path, "bundle_id": "example.tool",
                    }))
                    .unwrap(),
                )
                .unwrap();
            }
            let source = fixture.path().join("Renamed.app");
            bundle(&source, "example.tool", b"new");
            let error = publish(
                &ctx(fixture.path()),
                &source,
                "owner/tool",
                &applications,
                &state,
            )
            .unwrap_err();
            assert!(error.diagnostic.contains("multiple managed apps"));
            assert_eq!(children(&applications).unwrap().count(), 2);
            assert_eq!(children(&state).unwrap().count(), 2);
        }

        #[test]
        fn rejects_marker_symlink_and_bundle_link_outside_release() {
            let fixture = tempfile::tempdir().unwrap();
            let source = fixture.path().join("Tool.app");
            let other = fixture.path().join("outside");
            fs::write(&other, "outside").unwrap();
            bundle(&source, "example.tool", b"tool");
            symlink(&other, source.join("Contents/other")).unwrap();
            assert!(bundle_id(&ctx(fixture.path()), &source, false).is_err());
            let marker = fixture.path().join("marker.json");
            symlink(&other, &marker).unwrap();
            assert!(OwnedApp::read_marker(&marker, fixture.path()).is_err());
        }
    }
}
