use std::{ffi::OsString, fs, path::Path};

pub fn private_environment(base: &Path, home: &Path, config: &Path) -> Vec<(OsString, OsString)> {
    let mut vars = vec![
        ("HOME".into(), home.as_os_str().to_owned()),
        ("DOTS_ROOT".into(), config.as_os_str().to_owned()),
        ("MISE_CONFIG_DIR".into(), config.as_os_str().to_owned()),
        ("DOTS_SKILL_LINKS".into(), "0".into()),
        ("MISE_CEILING_PATHS".into(), base.as_os_str().to_owned()),
    ];
    for (key, relative) in [
        ("TMPDIR", "tmp"),
        ("XDG_CONFIG_HOME", "xdg-config"),
        ("XDG_DATA_HOME", "xdg-data"),
        ("XDG_STATE_HOME", "xdg-state"),
        ("XDG_CACHE_HOME", "xdg-cache"),
        ("CODEX_HOME", "codex"),
        ("CLAUDE_CONFIG_DIR", "claude"),
        ("MISE_CACHE_DIR", "mise/cache"),
        ("MISE_DATA_DIR", "mise/data"),
        ("MISE_STATE_DIR", "mise/state"),
        ("MISE_TMP_DIR", "mise/tmp"),
        ("MISE_DEFAULT_CONFIG_DIR", "mise/default-config"),
        ("MISE_SYSTEM_CONFIG_DIR", "mise/system/config"),
        ("MISE_SYSTEM_DATA_DIR", "mise/system/data"),
        ("MISE_SYSTEM_DIR", "mise/system"),
        ("MISE_PLUGINS_DIR", "mise/plugins"),
        ("MISE_SYSTEM_PLUGINS_DIR", "mise/system/plugins"),
        ("MISE_DOWNLOADS_DIR", "mise/downloads"),
        ("MISE_INSTALLS_DIR", "mise/installs"),
        ("MISE_SYSTEM_INSTALLS_DIR", "mise/system/installs"),
        ("MISE_INSTALL_STORE_DIR", "mise/install-store"),
        ("MISE_SHARED_INSTALL_DIR", "mise/shared"),
        ("MISE_SHIMS_DIR", "mise/shims"),
    ] {
        let path = base.join(relative);
        fs::create_dir_all(&path).unwrap();
        vars.push((key.into(), path.into_os_string()));
    }
    for (key, relative) in [
        ("MISE_GLOBAL_CONFIG_FILE", "mise/user.toml"),
        ("MISE_SYSTEM_CONFIG_FILE", "mise/system/config.toml"),
    ] {
        vars.push((key.into(), base.join(relative).into_os_string()));
    }
    vars
}
