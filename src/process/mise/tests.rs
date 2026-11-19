use super::*;
use crate::config::Root;
use std::os::unix::fs::symlink;

#[test]
fn mise_minimum_probe_is_shared_by_fresh_instances_and_keyed_by_executable() {
    let fixture = tempfile::tempdir().unwrap();
    let dir = fixture.path().canonicalize().unwrap();
    fs::write(
        dir.join("config.toml"),
        "[vars]\ndots_profiles = 'default'\n",
    )
    .unwrap();
    let trace = dir.join("trace");
    let executable = |name, version| {
        let path = dir.join(name);
        fs::write(
            &path,
            format!("#!/bin/sh\necho {version} >> \"$PROBE_TRACE\"\necho {version}\n"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    };
    let first = executable("mise-first", "2026.11.9");
    let old = executable("mise-old", "2026.11.7");
    let alias = dir.join("mise-alias");
    symlink(&first, &alias).unwrap();
    let env = Env {
        vars: [
            ("DOTS_MISE_BIN".into(), first.as_os_str().into()),
            ("HOME".into(), dir.as_os_str().into()),
            ("PROBE_TRACE".into(), trace.as_os_str().into()),
        ]
        .into(),
        cwd: dir.clone(),
    };
    let root = Root::from_dir(&env, &dir).unwrap();
    let mut ctx = Ctx {
        env,
        root,
        flags: Default::default(),
    };

    assert_eq!(require_mise(&ctx.env).unwrap(), first);
    for _ in 0..2 {
        assert_eq!(Mise::new(&ctx).unwrap().binary, first);
    }
    ctx.env.vars.insert("DOTS_MISE_BIN".into(), alias.into());
    assert_eq!(Mise::new(&ctx).unwrap().binary, dir.join("mise-alias"));
    assert_eq!(fs::read_to_string(&trace).unwrap(), "2026.11.9\n");

    ctx.env
        .vars
        .insert("DOTS_MISE_BIN".into(), old.clone().into());
    for _ in 0..2 {
        let error = Mise::new(&ctx).err().unwrap();
        assert!(error.diagnostic.contains("mise 2026.11.7 is too old"));
        assert!(error.diagnostic.contains(&format!("{old:?}")));
    }
    fs::rename(executable("replacement", "2026.11.10"), &old).unwrap();
    assert_eq!(Mise::new(&ctx).unwrap().binary, old);
    assert_eq!(
        fs::read_to_string(trace).unwrap(),
        "2026.11.9\n2026.11.7\n2026.11.10\n"
    );
}
