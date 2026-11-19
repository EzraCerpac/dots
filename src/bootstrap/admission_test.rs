use super::*;
use crate::supervisor::tests::{bounded_fixture, fixture_context};
use nix::libc;
use std::cell::Cell;
use std::fs;

thread_local! {
    static CANCEL_AT_ADMISSION: Cell<bool> = const { Cell::new(false) };
}

pub(super) fn at_admission() {
    if CANCEL_AT_ADMISSION.replace(false) {
        assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
    }
}

#[test]
fn audit_denied_bootstrap_stage_leaves_local_settings_untouched() {
    const TEST: &str =
        "bootstrap::admission_test::audit_denied_bootstrap_stage_leaves_local_settings_untouched";
    const FLAG: &str = "DOTS_AUDIT_BOOTSTRAP_ADMISSION";
    if let Some(path) = std::env::var_os(FLAG) {
        let path = Path::new(&path);
        let ctx = fixture_context(path, false);
        let settings = path.join("config.local.toml");
        fs::write(&settings, b"before").unwrap();
        let run = Run::begin(&ctx, Sink::Unreported);
        assert!(!run.cancelled());
        CANCEL_AT_ADMISSION.set(true);
        let result = stage(&run, "Restore private settings", |_| {
            fs::write(&settings, b"changed").unwrap();
            Ok((Outcome::Completed, ()))
        });
        assert!(run.cancelled());
        assert!(run.snapshot().stages.is_empty());
        assert_eq!(fs::read(settings).unwrap(), b"before");
        assert!(result.is_none());
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let output = bounded_fixture(fixture.path(), FLAG, TEST);
    assert!(output.status.success(), "{output:?}");
}
