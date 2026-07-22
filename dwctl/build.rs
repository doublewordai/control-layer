/// Build script - the frontend is built before compiling release images.
/// Rust-only checks can reuse the already generated static directory.
fn main() {
    // Tell Cargo to rerun this build script if the static directory changes
    println!("cargo:rerun-if-changed=static");
}
