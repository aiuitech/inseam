//! Exposes the build target triple to the binary. `inseam self update` must
//! name the artifact for the platform it is actually running on, and cargo
//! hands the triple only to build scripts, never to the crate itself.
fn main() {
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=INSEAM_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
