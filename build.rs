fn main() {
    println!("cargo:rerun-if-changed=install.sh");
    let minimum = include_str!("install.sh")
        .lines()
        .find_map(|line| line.strip_prefix("minimum_mise="))
        .expect("install.sh must declare minimum_mise");
    assert!(
        minimum.split('.').count() == 3
            && minimum.split('.').all(|part| part.parse::<u32>().is_ok()),
        "minimum_mise must be a year.month.patch version"
    );
    println!("cargo:rustc-env=DOTS_MINIMUM_MISE={minimum}");
}
