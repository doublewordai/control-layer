/// Build script - Frontend is built in CI before publishing to crates.io.
/// No Node.js required for users installing from crates.io.
fn main() {
    // Tell Cargo to rerun this build script if the static directory changes
    println!("cargo:rerun-if-changed=static");
}
