// CI passes the tag-derived release version as CI_VERSION_COOPERALIANCA_LIBRARY.
// Local builds, and an empty value, fall back to the version in Cargo.toml.
fn main() {
    println!("cargo:rerun-if-env-changed=CI_VERSION_COOPERALIANCA_LIBRARY");
    let version = std::env::var("CI_VERSION_COOPERALIANCA_LIBRARY")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("CARGO_PKG_VERSION").ok())
        .expect("cargo sets CARGO_PKG_VERSION");
    println!("cargo:rustc-env=COOPERALIANCA_VERSION={version}");
}
