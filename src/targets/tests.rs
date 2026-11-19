use super::*;
use serde_json::json;
use std::os::unix::fs::symlink;

fn declare(targets: &mut Targets, path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    targets
        .add_declarations(path, &passive::toml(path).unwrap(), false)
        .unwrap();
}

fn marker(dir: &Path, path: &Path, repo: &str, filename: Option<&str>) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let bundle = path.file_name().unwrap().to_str().unwrap();
    let name = ownership::marker_filename(bundle);
    let marker = dir.join(filename.unwrap_or(&name));
    fs::write(
        &marker,
        json!({"repo": repo, "path": path, "bundle_id": "test.example"}).to_string(),
    )
    .unwrap();
    marker
}

fn values(targets: &Targets, prefix: &str, used: &[&str], keep: bool) -> Vec<String> {
    let used: Vec<_> = used.iter().map(|word| (*word).into()).collect();
    targets
        .available(prefix, &used, keep)
        .into_iter()
        .map(|item| item.value)
        .collect()
}

fn one(targets: &Targets, request: &str, keep: bool) -> Target {
    match targets.resolve(request, keep) {
        Resolution::One(target) => target,
        other => panic!("expected one match for {request:?}, got {other:?}"),
    }
}

// The caller's path and all-profile hold behavior are unchanged since v0.4.0.
// A leading literal "~/" is a 1.0 completion case once the shell decodes it.
#[test]
fn explicit_scope_reads_only_selected_declarations_and_global_holds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("root");
    let caller = dir.path().join("caller");
    let literal_home = caller.join("~");
    fs::create_dir_all(path.join("dots")).unwrap();
    fs::create_dir_all(&literal_home).unwrap();
    let root = Root::resolve(&Env {
        vars: [("DOTS_ROOT".into(), path.clone().into_os_string())].into(),
        cwd: caller.clone(),
    })
    .unwrap();
    fs::write(
        path.join("config.toml"),
        "[vars]\ndots_profiles = 'laptop server'\n[tools]\nbase = 'latest'\n",
    )
    .unwrap();
    fs::write(
        path.join("config.laptop.toml"),
        "[tools]\nlaptop = 'latest'\n",
    )
    .unwrap();
    fs::write(
        path.join("config.local.toml"),
        "[tools]\nlocal = 'latest'\n",
    )
    .unwrap();
    fs::write(
        path.join("dots/exceptions.tsv"),
        "server\tpackage-owner\thold=brew:widget\n",
    )
    .unwrap();
    let selected = literal_home.join("chosen.toml");
    let selected_bytes =
        "[bootstrap.packages]\n'brew:owner/tap/widget@2' = 'latest'\n[tools]\nchosen = 'latest'\n";
    fs::write(&selected, selected_bytes).unwrap();

    let scope = Scope::Path("~/chosen.toml".into());
    let targets = Targets::read(&root, scope.clone(), &caller).unwrap();
    assert_eq!(values(&targets, "", &[], false), ["chosen"]);
    let held = one(&targets, "widget@2", false);
    assert_eq!(held.held.as_deref(), Some("package-owner"));
    assert_eq!(held.declarations[0].path, selected.canonicalize().unwrap());
    assert_eq!(fs::read_to_string(selected).unwrap(), selected_bytes);
    assert_eq!(
        values(
            &Targets::read(&root, Scope::Base, &caller).unwrap(),
            "",
            &[],
            false
        ),
        ["base"]
    );
    assert_eq!(
        values(
            &Targets::read(&root, Scope::Profile("laptop".into()), &caller).unwrap(),
            "",
            &[],
            false,
        ),
        ["laptop"]
    );
    assert!(Targets::read(&root, Scope::Profile("../caller".into()), &caller).is_err());

    fs::write(path.join("dots/exceptions.tsv"), "not-a-profile\tname\n").unwrap();
    assert!(Targets::read(&root, scope.clone(), &caller).is_err());
    assert!(
        values(
            &Targets::read_for_completion(&root, scope, &caller),
            "",
            &[],
            false
        )
        .is_empty()
    );
}

#[test]
fn unreadable_declaration_file_is_an_error_for_removal_and_skipped_for_completion() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::resolve(&Env {
        vars: [("DOTS_ROOT".into(), dir.path().as_os_str().to_owned())].into(),
        cwd: dir.path().to_owned(),
    })
    .unwrap();
    let broken = dir.path().join("config.toml");
    fs::write(&broken, "[tools]\nnope = [\n").unwrap();
    assert!(Targets::read(&root, Scope::Base, dir.path()).is_err());
    assert!(
        values(
            &Targets::read_for_completion(&root, Scope::Base, dir.path()),
            "",
            &[],
            false,
        )
        .is_empty()
    );
    assert_eq!(fs::read_to_string(broken).unwrap(), "[tools]\nnope = [\n");
}

// Unchanged from v0.4.0 test_remove_accepts_alias_from_second_declaration
// and the Go TestScopedRemove (with an additional machine-local declaration).
#[test]
fn aliases_share_an_identity_and_keep_original_declarations() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("config.toml");
    let local = dir.path().join("config.local.toml");
    let base_contents = r#"
        [tools]
        fd = "latest"
        "npm:@scope/foo" = [{version = "1"}, {version = "2"}]
        [bootstrap.packages]
        "brew:fd" = "latest" # keep this comment
        "brew:homebrew/core/fd" = "latest"
    "#;
    let local_contents = "[bootstrap.packages]\n'brew:fd' = 'latest'\n";
    let mut targets = Targets::default();
    declare(&mut targets, &base, base_contents);
    declare(&mut targets, &local, local_contents);
    targets.finish(|_| None);

    let fd = one(&targets, "Homebrew/Core/FD", true);
    assert_eq!(fd.id, "brew:fd");
    assert_eq!(targets.get("brew:fd"), Some(&fd));
    assert_eq!(fd.declarations.len(), 3);
    assert!(fd.declarations.contains(&Declaration {
        path: base.clone(),
        section: Section::Packages,
        key: "brew:homebrew/core/fd".into(),
    }));
    assert_eq!(fd.declarations[2].path, local);
    assert_eq!(one(&targets, "brew:homebrew/core/fd", false), fd);
    let Resolution::Ambiguous(alternatives) = targets.resolve("FD", false) else {
        panic!("expected a formula and a mise tool");
    };
    assert_eq!(
        alternatives
            .iter()
            .map(Target::qualified)
            .collect::<Vec<_>>(),
        ["brew:fd", "mise:fd"]
    );
    assert_eq!(
        values(&targets, "fd", &["Homebrew/Core/FD"], false),
        ["mise:fd"]
    );
    assert_eq!(values(&targets, "NPM:", &[], false), ["npm:@scope/foo"]);
    assert!(values(&targets, "NPM:", &["Foo"], false).is_empty());
    assert_eq!(fs::read_to_string(base).unwrap(), base_contents);
    assert_eq!(fs::read_to_string(local).unwrap(), local_contents);
}

#[test]
fn trailing_slash_github_declarations_offer_the_normalized_short_name() {
    let dir = tempfile::tempdir().unwrap();
    let mut targets = Targets::default();
    declare(
        &mut targets,
        &dir.path().join("config.toml"),
        "[tools]\n'https://github.com/acme/widget/' = 'latest'\n",
    );
    targets.finish(|_| None);
    assert_eq!(one(&targets, "widget", false).id, "github:acme/widget");
    assert_eq!(values(&targets, "widget", &[], false), ["widget"]);
}

#[test]
fn managers_and_taps_stay_distinct_and_unsafe_names_are_omitted() {
    let dir = tempfile::tempdir().unwrap();
    let mut targets = Targets::default();
    declare(
        &mut targets,
        &dir.path().join("config.toml"),
        r#"
        [tools]
        "-bad" = "latest"
        "./path" = "latest"
        "bad\nname" = "latest"
        [bootstrap.packages]
        "brew:one/tools/widget" = "latest"
        "brew:two/tools/widget" = "latest"
        "brew-cask:homebrew/cask/widget" = "latest"
        "apt:widget:arm64" = "latest"
        "mas:12345" = "latest"
        "mas:invalid" = "latest"
        "brew:one/../invalid" = "latest"
        "apt:bad name" = "latest"
        "custom:invalid" = "latest"
    "#,
    );
    targets.finish(|_| None);
    assert_eq!(targets.iter().count(), 5);
    assert_eq!(
        values(&targets, "widget", &[], false),
        [
            "brew-cask:widget",
            "brew:one/tools/widget",
            "brew:two/tools/widget",
            "widget:arm64",
        ]
    );
    assert_eq!(
        one(&targets, "ONE/TOOLS/WIDGET", false).id,
        "brew:one/tools/widget"
    );
    assert_eq!(
        one(&targets, "APT:WIDGET:ARM64", false).id,
        "apt:widget:arm64"
    );
    assert_eq!(
        targets.available("123", &[], false),
        [Available {
            value: "12345".into(),
            description: "Mac App Store app".into(),
        }]
    );
}

// Unchanged ownership and scope expectations from Go TestActiveRemoveAndOwnership.
#[test]
fn owned_apps_can_outlive_declarations_but_do_not_escape_selected_scope() {
    let dir = tempfile::tempdir().unwrap();
    let apps = dir.path().join("Applications");
    let markers = dir.path().join("github-apps");
    let config = dir.path().join("config.toml");
    let standalone = apps.join("Standalone Tool.app");
    let widget = apps.join("Widget Tool.app");
    let standalone_marker = marker(&markers, &standalone, "Owner/Standalone", None);
    assert_eq!(
        standalone_marker.file_name().unwrap(),
        "f9f7342537c9b384f64eaa27105aedb6777459f42edb523e7a19a127dba16603.json"
    );
    marker(&markers, &widget, "Owner/Widget", None);
    let before = fs::read(&standalone_marker).unwrap();
    let contents = "[tools]\n'github:OWNER/widget' = 'latest'\n";

    for scoped in [false, true] {
        let mut targets = Targets::default();
        declare(&mut targets, &config, contents);
        targets.add_ownership(&markers, &apps, scoped);
        targets.finish(|_| None);

        let widget_target = one(&targets, "WIDGET TOOL", false);
        assert_eq!(widget_target.id, "github:owner/widget");
        assert_eq!(widget_target.kind, Kind::ReleaseApp);
        assert_eq!(widget_target.declarations.len(), 1);
        assert_eq!(widget_target.owned_apps[0].path, widget);
        assert_eq!(widget_target.owned_apps[0].repo, "owner/widget");
        assert!(
            values(
                &targets,
                "wid",
                &["https://github.com/OWNER/widget.git/"],
                false
            )
            .is_empty()
        );
        assert!(values(&targets, "wid", &["OWNER/WIDGET"], false).is_empty());
        assert!(values(&targets, "stand", &[], true).is_empty());
        if scoped {
            assert_eq!(
                targets.resolve("standalone tool", false),
                Resolution::None(None)
            );
        } else {
            let app = one(&targets, "standalone tool", false);
            assert!(app.declarations.is_empty());
            assert_eq!(app.owned_apps[0].marker, standalone_marker);
            assert_eq!(app.owned_apps[0].path, standalone);
            assert_eq!(
                values(&targets, "stand", &[], false),
                ["standalone", "standalone tool"]
            );
        }
    }
    assert_eq!(fs::read(standalone_marker).unwrap(), before);
    assert_eq!(fs::read_to_string(config).unwrap(), contents);
}

#[test]
fn unsafe_markers_cannot_offer_apps_or_make_an_ambiguous_alias_unique() {
    let dir = tempfile::tempdir().unwrap();
    let apps = dir.path().join("Applications");
    let markers = dir.path().join("github-apps");
    fs::create_dir(&apps).unwrap();
    marker(&markers, &apps.join("Widget.app"), "owner/widget", None);
    marker(
        &markers,
        &apps.join("Stale Widget.app"),
        "other/widget",
        Some("bad-name.json"),
    );
    let linked = apps.join("Linked.app");
    symlink(dir.path(), &linked).unwrap();
    marker(&markers, &linked, "owner/linked", None);
    marker(
        &markers,
        &apps.join("Stealth\u{202e}.app"),
        "owner/stealth",
        None,
    );
    let mut targets = Targets::default();
    targets.add_ownership(&markers, &apps, false);
    targets.finish(|_| None);

    assert_eq!(values(&targets, "wid", &[], false), ["github:owner/widget"]);
    assert!(matches!(
        targets.resolve("widget", false),
        Resolution::Ambiguous(_)
    ));
    assert!(values(&targets, "linked", &[], false).is_empty());
    assert!(values(&targets, "github:owner/stealth", &[], false).is_empty());
    assert!(
        one(&targets, "github:owner/stealth", false)
            .ownership_error
            .is_some()
    );
    let target = one(&targets, "github:owner/linked", false);
    assert!(target.owned_apps.is_empty());
    assert_eq!(target.ownership_error.unwrap().path, linked);
    assert!(targets.ownership_error().is_none());
}

#[test]
fn mismatched_installation_is_omitted_but_a_missing_app_can_be_cleaned_up() {
    let dir = tempfile::tempdir().unwrap();
    let apps = dir.path().join("Applications");
    let markers = dir.path().join("github-apps");
    let path = apps.join("Owned.app");
    fs::create_dir_all(&path).unwrap();
    let marker = marker(&markers, &path, "acme/widget", None);
    let installation =
        ownership::AppInstallation::from_metadata(&fs::metadata(&path).unwrap()).unwrap();
    fs::write(&marker, json!({"repo": "acme/widget", "path": path, "bundle_id": "test.example", "installation": installation}).to_string()).unwrap();
    fs::rename(&path, apps.join("Original.app")).unwrap();
    fs::create_dir(&path).unwrap();

    let mut targets = Targets::default();
    targets.add_ownership(&markers, &apps, false);
    targets.finish(|_| None);
    assert!(values(&targets, "owned", &[], false).is_empty());
    assert!(one(&targets, "owned", false).ownership_error.is_some());

    fs::remove_dir(&path).unwrap();
    let mut targets = Targets::default();
    targets.add_ownership(&markers, &apps, false);
    targets.finish(|_| None);
    assert_eq!(values(&targets, "owned", &[], false), ["owned"]);
}

#[test]
fn an_array_is_not_an_ownership_record() {
    let dir = tempfile::tempdir().unwrap();
    let apps = dir.path().join("Applications");
    let records = dir.path().join("github-apps");
    let path = apps.join("Widget.app");
    let file = marker(&records, &path, "owner/widget", None);
    fs::write(
        &file,
        json!(["owner/widget", path, "test.example"]).to_string(),
    )
    .unwrap();
    let mut targets = Targets::default();
    targets.add_ownership(&records, &apps, false);
    targets.finish(|_| None);

    assert_eq!(targets.ownership_error().unwrap().path, file);
    assert!(targets.available("", &[], false).is_empty());
    assert_eq!(
        targets.resolve("github:owner/widget", false),
        Resolution::None(None)
    );
}

#[test]
fn unicode_app_aliases_count_toward_ambiguity() {
    let dir = tempfile::tempdir().unwrap();
    let apps = dir.path().join("Applications");
    let records = dir.path().join("github-apps");
    marker(&records, &apps.join("Straße.app"), "owner/street", None);
    let mut targets = Targets::default();
    declare(
        &mut targets,
        &dir.path().join("config.toml"),
        "[tools]\nstrasse = 'latest'\n",
    );
    targets.add_ownership(&records, &apps, false);
    targets.finish(|_| None);

    assert!(
        matches!(targets.resolve("strasse", false), Resolution::Ambiguous(choices) if choices.len() == 2)
    );
    assert_eq!(
        values(&targets, "STRAẞE", &[], false),
        ["github:owner/street", "mise:strasse"]
    );
    assert_eq!(
        values(&targets, "STRAẞE", &["mise:strasse"], false),
        ["github:owner/street"]
    );
}

#[test]
fn synthetic_mise_names_are_exact_even_with_a_folded_completion_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let mut targets = Targets::default();
    declare(
        &mut targets,
        &dir.path().join("config.toml"),
        r#"
        [tools]
        "Straße" = "latest"
        "npm:strasse" = "latest"
    "#,
    );
    targets.finish(|_| None);

    let Resolution::Ambiguous(choices) = targets.resolve("strasse", false) else {
        panic!("expected a mise tool and its npm alternative");
    };
    assert_eq!(
        choices.iter().map(Target::qualified).collect::<Vec<_>>(),
        ["mise:Straße", "npm:strasse"]
    );
    assert_eq!(one(&targets, "mise:Straße", false).id, "Straße");
    assert!(matches!(
        targets.resolve("mise:STRASSE", false),
        Resolution::None(_)
    ));
    assert!(matches!(
        targets.resolve("MISE:STRASSE", false),
        Resolution::None(_)
    ));
    assert_eq!(
        values(
            &targets,
            "MISE:STRASS",
            &["mise:STRASSE", "MISE:STRASSE"],
            false
        ),
        ["mise:Straße"]
    );
    assert!(values(&targets, "MISE:STRASS", &["mise:Straße"], false).is_empty());
}

#[test]
fn an_unavailable_active_list_only_blocks_strict_reads() {
    let dir = tempfile::tempdir().unwrap();
    let apps = dir.path().join("Applications");
    let records = dir.path().join("github-apps");
    marker(&records, &apps.join("Widget.app"), "owner/widget", None);
    let unavailable = || Err(Fail::new(1, "mise discovery unavailable"));
    assert!(Targets::read_declarations(unavailable(), false).is_err());
    let mut targets = Targets::read_declarations(unavailable(), true).unwrap();
    targets.add_ownership(&records, &apps, false);
    targets.finish(|_| None);
    assert_eq!(values(&targets, "widget", &[], false), ["widget"]);

    let base = dir.path().join("config.toml");
    fs::write(&base, "[tools]\nfd = 'latest'\n").unwrap();
    let incomplete = || {
        Ok(vec![
            Ok(base.clone()),
            Err(Fail::new(1, "config unavailable")),
        ])
    };
    assert!(Targets::read_declarations(incomplete(), false).is_err());
    let mut targets = Targets::read_declarations(incomplete(), true).unwrap();
    targets.finish(|_| None);
    assert_eq!(values(&targets, "", &[], false), ["fd"]);
}

// Unchanged from v0.4.0 test_remove_suggests_only_a_unique_managed_match.
#[test]
fn suggestions_require_one_close_identity_that_is_not_held() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    for held in [false, true] {
        for ambiguous in [false, true] {
            let contents = format!(
                "[bootstrap.packages]\n'brew-cask:codexbar' = 'latest'\n{}",
                if ambiguous {
                    "'brew:codexbar' = 'latest'\n"
                } else {
                    ""
                }
            );
            let mut targets = Targets::default();
            declare(&mut targets, &config, &contents);
            targets.finish(|id| (held && id == "brew-cask:codexbar").then(|| "manual".into()));
            let suggestion = (!held && !ambiguous).then(|| "brew-cask:codexbar".into());
            assert_eq!(
                targets.resolve("codex-bar", false),
                Resolution::None(suggestion)
            );
            assert_eq!(
                one(&targets, "brew-cask:codexbar", false).held.as_deref(),
                held.then_some("manual")
            );
            if held {
                assert!(values(&targets, "brew-cask:", &[], false).is_empty());
            }
        }
    }
}

#[test]
fn equal_letters_in_a_different_order_do_not_always_make_a_close_name() {
    let dir = tempfile::tempdir().unwrap();
    let mut targets = Targets::default();
    declare(
        &mut targets,
        &dir.path().join("config.toml"),
        "[tools]\nabcab = 'latest'\n",
    );
    targets.finish(|_| None);
    assert_eq!(targets.resolve("ababc", false), Resolution::None(None));
}

// Unchanged from v0.4.0 test_remove_hint_respects_unreadable_app_ownership.
#[test]
fn unknown_ownership_does_not_block_a_declaration_only_operation() {
    let dir = tempfile::tempdir().unwrap();
    let markers = dir.path().join("github-apps");
    fs::create_dir(&markers).unwrap();
    let invalid = markers.join("broken.json");
    fs::write(&invalid, "{").unwrap();
    let mut targets = Targets::default();
    declare(
        &mut targets,
        &dir.path().join("config.toml"),
        r#"
        [tools]
        "github:example/codexbar" = "latest"
        fd = "latest"
    "#,
    );
    targets.add_ownership(&markers, &dir.path().join("Applications"), false);
    targets.finish(|_| None);

    assert_eq!(targets.ownership_error().unwrap().path, invalid);
    assert_eq!(targets.resolve("codex-bar", false), Resolution::None(None));
    assert_eq!(
        targets.resolve("codex-bar", true),
        Resolution::None(Some("github:example/codexbar".into()))
    );
    assert!(
        one(&targets, "github:example/codexbar", false)
            .ownership_error
            .is_some()
    );
    assert_eq!(
        one(&targets, "github:example/codexbar", true)
            .declarations
            .len(),
        1
    );
    assert!(one(&targets, "fd", false).ownership_error.is_none());
    assert!(values(&targets, "codex", &[], false).is_empty());
    assert_eq!(values(&targets, "codex", &[], true), ["codexbar"]);
}
