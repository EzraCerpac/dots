use super::*;

fn fixture(scenario: &str) -> UpdateFixture {
    let f = UpdateFixture::new();
    let bin = f.world.home().join(".local/bin");
    fs::rename(bin.join("mise"), bin.join("mise-default")).unwrap();
    script(&bin.join("mise"), include_str!("review-mise"));
    f.write("scenario", scenario);
    f
}

fn packages(f: &UpdateFixture, manager: &str, package: &str, desired: &str, state: &str) {
    f.write(
        "config.toml",
        &format!("{CONFIG}\n[bootstrap.packages]\n\"{manager}:{package}\" = \"latest\"\n"),
    );
    f.write(
        "status.json",
        &serde_json::json!({manager: {"available": true, "packages": [{
            "package": package, "requested_version": "latest", "desired_state": desired,
            "state": state, "installed_version": "1"
        }]}})
        .to_string(),
    );
}

#[test]
fn after_packages_invalid_config_stops_tools() {
    let f = fixture("invalidate");
    packages(&f, "apt", "sample", "present", "installed");
    let output = run(&mut f.up("--plain"));
    let calls = f.read_home("mise-calls");
    assert!(
        calls.contains("bootstrap packages apply --manager apt"),
        "{calls}"
    );
    assert!(!calls.lines().any(|line| line == "install"), "{calls}");
    mentions(&exit(&output, 1), &["Validate update configuration"]);
}

#[test]
fn package_snapshot_failure_preserves_action_result_and_reports_unknown_state() {
    for (scenario, expected) in [("snapshot", 0), ("snapshot-action-fails", 1)] {
        let f = fixture(scenario);
        packages(&f, "apt", "sample", "present", "installed");
        let text = exit(&run(&mut f.up("--plain")), expected);
        mentions(
            &text,
            &[
                "could not read package versions after the action",
                "fixture snapshot unavailable",
            ],
        );
        if expected == 1 {
            mentions(&text, &["Command failed (exit 74)"]);
        }
        assert!(!text.contains("apt:sample"), "{text}");
    }
}

#[test]
fn cask_rows_excluded_by_grouped_assignment_require_named_upgrade() {
    let f = fixture("cask");
    packages(&f, "brew-cask", "sample", "present", "installed");
    let ordinary = serde_json::from_str::<serde_json::Value>(
        &fs::read_to_string(f.world.root().join("status.json")).unwrap(),
    )
    .unwrap();
    let mut narrow = ordinary.clone();
    let mut auto = ordinary["brew-cask"]["packages"][0].clone();
    auto["package"] = "bare".into();
    auto["auto_updates"] = true.into();
    narrow["brew-cask"]["packages"]
        .as_array_mut()
        .unwrap()
        .push(auto);
    f.write("narrow.json", &narrow.to_string());
    f.write("inventory.json", r#"{"brew-cask":{"packages":[{"package":"sample"}]},"unprefixed":{"packages":[{"package":"bare"}]}}"#);
    exit(&run(&mut f.up("--plain")), 0);
    let calls = f.read_home("mise-calls");
    assert!(
        calls
            .lines()
            .any(|line| line
                == "bootstrap packages upgrade --manager brew-cask --yes brew-cask:sample"),
        "{calls}"
    );
}

#[test]
fn unknown_manager_baseline_does_not_claim_an_install() {
    let f = fixture("manager");
    f.write(
        "config.toml",
        &format!("{CONFIG}\n[tools]\n\"github:EzraCerpac/dots\" = \"latest\"\n"),
    );
    script(
        &f.world.home().join("new-manager/bin/dots"),
        "printf '%s\\n' 'dots 99.0.0'",
    );
    let text = exit(&run(&mut f.up("--plain")), 0);
    assert!(!text.contains("used from the next run"), "{text}");
}

#[test]
fn contract_mock_rejects_unknown_mise_commands() {
    let f = UpdateFixture::new();
    let output = run(f
        .world
        .command(f.world.home().join(".local/bin/mise"))
        .arg("prune"));
    assert!(!output.status.success(), "{output:?}");
}

#[cfg(target_os = "macos")]
fn store(scenario: &str, desired: &str, state: &str) -> UpdateFixture {
    let f = fixture(scenario);
    packages(&f, "mas", "42", desired, state);
    let bin = f.world.home().join(".local/bin");
    script(
        &bin.join("sudo"),
        r#"
        test "$*" = '-n -v'
        test ! -f expired
        if [ -f authorized-status.json ]; then cp authorized-status.json status.json; fi
    "#,
    );
    script(&f.world.home().join("store/bin/mas"), ":");
    f
}

#[test]
#[cfg(target_os = "macos")]
fn store_expired_authorization_takes_precedence_over_removal_diagnostic() {
    for (scenario, expected) in [("store-expire", 0), ("store-removal", 1)] {
        let f = store(scenario, "absent", "unexpectedly_installed");
        let text = exit(&run(f.up("--plain").arg("--mas")), expected);
        let diagnostic = if expected == 0 {
            "Cached administrator authorization is unavailable"
        } else {
            "mas cannot remove mas:42"
        };
        mentions(&text, &[diagnostic]);
        assert_eq!(
            text.contains("mas cannot remove mas:42"),
            expected == 1,
            "{text}"
        );
    }
}

#[test]
#[cfg(target_os = "macos")]
fn store_baseline_excludes_changes_during_authorization() {
    let f = store("store-baseline", "present", "installed");
    let state = fs::read_to_string(f.world.root().join("status.json")).unwrap();
    f.write(
        "authorized-status.json",
        &state.replace("\"installed_version\":\"1\"", "\"installed_version\":\"2\""),
    );
    let text = exit(&run(f.up("--plain").arg("--mas")), 0);
    assert!(!text.contains("mas:42"), "{text}");
    assert!(
        f.read_home("mise-calls")
            .contains("bootstrap packages upgrade --manager mas")
    );
}

#[test]
#[cfg(target_os = "macos")]
fn store_actions_use_configured_ruby() {
    let f = store("store-ruby", "present", "installed");
    script(&f.world.home().join("store/bin/ruby"), ":");
    script(&f.world.home().join("configured-ruby/bin/ruby"), ":");
    exit(&run(f.up("--plain").arg("--mas")), 0);
    let calls = f.read_home("mise-calls");
    assert!(
        calls.contains("store Ruby for apply") && calls.contains("store Ruby for upgrade"),
        "{calls}"
    );
}
