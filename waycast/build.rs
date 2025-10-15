use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // Check if build-frontend feature is enabled
    let build_frontend = env::var("CARGO_FEATURE_BUILD_FRONTEND").is_ok();

    if build_frontend {
        println!("cargo:warning=Building frontend with npm...");

        // Get the project root (parent of waycast directory)
        let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
        let project_root = manifest_dir.parent().unwrap();
        let dashboard_dir = project_root.join("dashboard");
        let static_dir = manifest_dir.join("static");

        // Check if dashboard directory exists
        if !dashboard_dir.exists() {
            panic!("Dashboard directory not found at {:?}", dashboard_dir);
        }

        // Always install npm dependencies to ensure they're up to date
        println!("cargo:warning=Installing npm dependencies...");
        let npm_install = Command::new("npm")
            .arg("ci")
            .current_dir(&dashboard_dir)
            .status()
            .expect("Failed to run npm ci");

        if !npm_install.success() {
            panic!("npm ci failed");
        }

        // Build the frontend
        println!("cargo:warning=Running npm build...");
        let npm_build = Command::new("npm")
            .arg("run")
            .arg("build")
            .current_dir(&dashboard_dir)
            .status()
            .expect("Failed to run npm build");

        if !npm_build.success() {
            panic!("npm build failed");
        }

        // Copy built files to static directory
        let dist_dir = dashboard_dir.join("dist");
        if !dist_dir.exists() {
            panic!("Dashboard dist directory not found at {:?}", dist_dir);
        }

        // Remove existing static directory and recreate it
        if static_dir.exists() {
            fs::remove_dir_all(&static_dir).expect("Failed to remove static directory");
        }
        fs::create_dir_all(&static_dir).expect("Failed to create static directory");

        // Copy all files from dist to static
        println!("cargo:warning=Copying built files to static directory...");
        copy_dir_all(&dist_dir, &static_dir).expect("Failed to copy dist to static");

        println!("cargo:warning=Frontend built and bundled successfully!");
    } else {
        println!("cargo:warning=Skipping frontend build (use --features build-frontend to enable)");
    }

    // Tell Cargo to rerun this build script if these directories change
    println!("cargo:rerun-if-changed=../dashboard/src");
    println!("cargo:rerun-if-changed=../dashboard/package.json");
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if ty.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
