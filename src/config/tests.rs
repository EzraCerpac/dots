use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::Command;

use super::{Doc, Exceptions, Root, Vars, passive, selection};
use crate::Env;

fn fixture() -> (tempfile::TempDir, Env, Root) {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().canonicalize().unwrap();
    let env = Env {
        vars: Default::default(),
        cwd,
    };
    let root = Root {
        path: env.cwd.join("mise"),
        child_bin: Default::default(),
    };
    fs::create_dir(root.path()).unwrap();
    (temp, env, root)
}

#[test]
fn exception_holds_are_bounded_and_an_absent_manifest_needs_no_profile() {
    let (_temp, _env, root) = fixture();
    assert!(
        Exceptions::read(&root)
            .unwrap()
            .held_by("brew:jq")
            .is_none()
    );
    let directory = root.path().join("dots");
    fs::create_dir(&directory).unwrap();
    let manifest = directory.join("exceptions.tsv");
    symlink("missing-manifest", &manifest).unwrap();
    assert!(Exceptions::read(&root).is_err());
    fs::remove_file(&manifest).unwrap();
    assert!(
        Command::new("/usr/bin/mkfifo")
            .arg(&manifest)
            .status()
            .unwrap()
            .success()
    );
    let error = Exceptions::read(&root).unwrap_err();
    assert!(error.diagnostic.contains("expected a regular file"));

    fs::remove_file(&manifest).unwrap();
    let file = fs::File::create(&manifest).unwrap();
    file.set_len((2 << 20) + 1).unwrap();
    assert!(
        Exceptions::read(&root)
            .unwrap_err()
            .diagnostic
            .contains("too large")
    );
    fs::write(
        root.path().join("config.toml"),
        "[vars]\ndots_profiles = \"laptop server\"\n",
    )
    .unwrap();
    fs::write(
        &manifest,
        "server\tjq-recipe\tinstall-only,hold=brew:jq,hold=brew:yq\n",
    )
    .unwrap();
    let exceptions = Exceptions::read(&root).unwrap();
    assert_eq!(exceptions.held_by("brew:tap/jq@1"), Some("jq-recipe"));
    assert_eq!(exceptions.held_by("brew:yq"), Some("jq-recipe"));
    assert!(exceptions.selected("laptop").unwrap().is_empty());
}

#[test]
fn bootstrap_alone_reads_a_single_caller_mise_env_profile_before_selection() {
    let (_temp, mut env, root) = fixture();
    fs::write(
        root.path().join("config.toml"),
        "[vars]\ndots_profiles = \"laptop server\"\n",
    )
    .unwrap();
    env.vars.insert("MISE_ENV".into(), "extra,laptop".into());
    assert_eq!(selection::read(&root, &env).unwrap().profile, None);
    assert_eq!(
        selection::for_bootstrap(&root, &env, None)
            .unwrap()
            .profile
            .as_deref(),
        Some("laptop")
    );
    env.vars.insert("MISE_ENV".into(), "laptop,server".into());
    assert!(selection::for_bootstrap(&root, &env, None).is_err());
    fs::write(
        root.path().join("miserc.toml"),
        "env = [\"server\", \"host-persisted\"]\n",
    )
    .unwrap();
    env.vars.insert("SETUP_PROFILE".into(), "laptop".into());
    env.vars.insert("SETUP_MACHINE_ID".into(), "new-id".into());
    let selected = selection::for_bootstrap(&root, &env, None).unwrap();
    assert_eq!(selected.profile.as_deref(), Some("server"));
    assert_eq!(selected.machine_id.as_deref(), Some("persisted"));
    assert!(selection::for_bootstrap(&root, &env, Some("other")).is_err());
}

#[test]
fn base_vars_keep_raw_strings_and_atomic_doc_edits_keep_the_referent_mode() {
    let (_temp, env, root) = fixture();
    let referent = env.cwd.join("existing.toml");
    let source = "[\"vars\"] # header\n\"dots_profiles\" = \"laptop server\" # roles\ndots_self_updating_casks = \"one two\"\n";
    fs::write(&referent, source).unwrap();
    fs::set_permissions(&referent, fs::Permissions::from_mode(0o640)).unwrap();
    let config = root.path().join("config.toml");
    symlink(&referent, &config).unwrap();
    fs::write(
        root.path().join("config.local.toml"),
        "[vars]\ndots_profiles = \"decoy\"\n",
    )
    .unwrap();
    let vars = Vars::read(&root).unwrap();
    assert_eq!(vars.profiles, ["laptop", "server"]);
    assert_eq!(vars.self_updating_casks, ["one", "two"]);
    let mut doc = Doc::open(&config).unwrap();
    doc.document["vars"]["dots_publish_on_behalf_of"] = toml_edit::value("Example");
    doc.install().unwrap();
    assert!(fs::read_to_string(&referent).unwrap().starts_with(source));
    assert_eq!(fs::read_link(config).unwrap(), referent);
    assert_eq!(
        referent.metadata().unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[test]
fn passive_mise_discovers_active_in_root_paths_without_running_env_or_hooks() {
    let (_temp, mut env, root) = fixture();
    let mut ambient = Env::current().unwrap();
    // Locate the tested installation once, then give the child isolated paths.
    ambient.vars.remove(std::ffi::OsStr::new("DOTS_MISE_BIN"));
    let binary = crate::process::mise_binary(&ambient).unwrap();
    env.vars
        .insert("DOTS_MISE_BIN".into(), binary.into_os_string());
    env.vars.insert("PATH".into(), "/usr/bin:/bin".into());
    let locations = [
        ("HOME", "home"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "real-cache"),
        ("XDG_STATE_HOME", "real-state"),
        ("MISE_SYSTEM_CONFIG_DIR", "system"),
        ("MISE_CEILING_PATHS", ""),
    ];
    for (key, location) in locations {
        env.vars
            .insert(key.into(), env.cwd.join(location).into_os_string());
    }
    env.vars.insert("MISE_ENV".into(), "laptop".into());
    env.vars.insert(
        "MISE_TRUSTED_CONFIG_PATHS".into(),
        root.path.as_os_str().to_owned(),
    );
    fs::create_dir(env.cwd.join("home")).unwrap();
    let fixture = "[vars]\ndots_profiles = \"laptop server\"\n[env]\n_.source = 'sentinel.sh'\n";
    fs::write(root.path().join("config.toml"), fixture).unwrap();
    fs::write(root.path().join("sentinel.sh"), "touch executed\n").unwrap();
    fs::write(
        root.path().join("miserc.toml"),
        "env = [\"server\", \"host-demo\", \"extra\"]\n",
    )
    .unwrap();
    for profile in ["server", "host-demo", "extra", "laptop"] {
        fs::write(root.path().join(format!("config.{profile}.toml")), "").unwrap();
    }
    let outside = env.cwd.join("outside.toml");
    fs::write(&outside, "[vars]\nignore = \"outside\"\n").unwrap();
    symlink(outside, root.path().join("config.local.toml")).unwrap();

    let files = passive::active_files_in(&root, &env)
        .unwrap()
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let names: Vec<_> = files
        .iter()
        .map(|path| path.file_name().unwrap().to_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "config.extra.toml",
            "config.host-demo.toml",
            "config.server.toml",
            "config.toml"
        ]
    );
    for path in [
        env.cwd.join("real-cache"),
        env.cwd.join("real-state"),
        root.path().join("executed"),
    ] {
        assert!(!path.exists(), "probe wrote to {}", path.display());
    }
}
