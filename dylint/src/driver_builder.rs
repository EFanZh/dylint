use crate::error::warn;
use anyhow::{anyhow, ensure, Context, Result};
use cargo_metadata::MetadataCommand;
use dylint_internal::{
    env::{self, var},
    rustup::{toolchain_path, SanitizeEnvironment},
};
use semver::Version;
use std::{
    env::consts,
    fs::{copy, create_dir_all, write},
    path::{Path, PathBuf},
    process::Stdio,
};
use tempfile::tempdir;

const README_TXT: &str = r#"
This directory contains Rust compiler drivers used by Dylint
(https://github.com/trailofbits/dylint).

Deleting this directory will cause Dylint to rebuild the drivers
the next time it needs them, but will have no ill effects.
"#;

fn cargo_toml(toolchain: &str, dylint_driver_spec: &str) -> String {
    format!(
        r#"
[package]
name = "dylint_driver-{}"
version = "0.1.0"
edition = "2018"

[dependencies]
anyhow = "1.0.38"
env_logger = "0.8.3"
dylint_driver = {{ {} }}
"#,
        toolchain, dylint_driver_spec,
    )
}

fn rust_toolchain(toolchain: &str) -> String {
    format!(
        r#"
[toolchain]
channel = "{}"
components = ["llvm-tools-preview", "rustc-dev"]
"#,
        toolchain,
    )
}

const MAIN_RS: &str = r#"
use anyhow::Result;
use std::env;
use std::ffi::OsString;

pub fn main() -> Result<()> {
    env_logger::init();

    let args: Vec<_> = env::args().map(OsString::from).collect();

    dylint_driver::dylint_driver(&args)
}
"#;

#[allow(unknown_lints)]
#[allow(question_mark_in_expression)]
pub fn get(opts: &crate::Dylint, toolchain: &str) -> Result<PathBuf> {
    let dylint_drivers = dylint_drivers()?;

    let driver_dir = dylint_drivers.join(&toolchain);
    if !driver_dir.is_dir() {
        create_dir_all(&driver_dir).with_context(|| {
            format!(
                "`create_dir_all` failed for `{}`",
                driver_dir.to_string_lossy()
            )
        })?;
    }

    let driver = driver_dir.join("dylint-driver");
    if !driver.exists() || is_outdated(opts, toolchain, &driver)? {
        build(opts, toolchain, &driver)?;
    }

    Ok(driver)
}

fn dylint_drivers() -> Result<PathBuf> {
    if let Ok(dylint_driver_path) = var(env::DYLINT_DRIVER_PATH) {
        let dylint_drivers = Path::new(&dylint_driver_path);
        ensure!(dylint_drivers.is_dir());
        Ok(dylint_drivers.to_path_buf())
    } else {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("Could not find HOME directory"))?;
        let dylint_drivers = Path::new(&home).join(".dylint_drivers");
        if !dylint_drivers.is_dir() {
            create_dir_all(&dylint_drivers).with_context(|| {
                format!(
                    "`create_dir_all` failed for `{}`",
                    dylint_drivers.to_string_lossy()
                )
            })?;
            let readme_txt = dylint_drivers.join("README.txt");
            write(&readme_txt, README_TXT).with_context(|| {
                format!("`write` failed for `{}`", readme_txt.to_string_lossy())
            })?;
        }
        Ok(dylint_drivers)
    }
}

fn is_outdated(opts: &crate::Dylint, toolchain: &str, driver: &Path) -> Result<bool> {
    let mut command = dylint_internal::driver(toolchain, driver)?;
    let output = command.args(&["-V"]).output()?;
    let stdout = std::str::from_utf8(&output.stdout)?;
    let theirs = stdout
        .trim_end()
        .rsplit_once(' ')
        .map(|pair| pair.1)
        .ok_or_else(|| anyhow!("Could not determine driver version"))?;

    let result = Version::parse(theirs);

    let their_version = match result {
        Ok(version) => version,
        Err(err) => {
            warn(
                opts,
                &format!("Could not parse driver version `{}`: {}", theirs, err),
            );
            return Ok(true);
        }
    };

    let our_version = Version::parse(env!("CARGO_PKG_VERSION"))?;

    Ok(their_version < our_version)
}

#[allow(clippy::assertions_on_constants)]
#[allow(clippy::expect_used)]
fn build(opts: &crate::Dylint, toolchain: &str, driver: &Path) -> Result<()> {
    let tempdir = tempdir().with_context(|| "`tempdir` failed")?;
    let package = tempdir.path();

    initialize(toolchain, package)?;

    let metadata = MetadataCommand::new()
        .current_dir(package)
        .no_deps()
        .exec()?;

    let toolchain_path = toolchain_path(package)?;

    // smoelius: The commented code was the old behavior. It would cause the driver to have rpaths
    // like `$ORIGIN/../../`... (see https://github.com/trailofbits/dylint/issues/54). The new
    // behavior causes the driver to have absolute rpaths.
    // let rustflags = "-C rpath=yes";
    let rustflags = format!(
        "-C link-args=-Wl,-rpath,{}/lib",
        toolchain_path.to_string_lossy()
    );

    let mut command = dylint_internal::build();
    command
        .sanitize_environment()
        .envs(vec![(env::RUSTFLAGS, rustflags)])
        .current_dir(&package);
    if opts.quiet {
        command.stderr(Stdio::null());
    }
    command.success()?;

    let binary = metadata.target_directory.join("debug").join(format!(
        "dylint_driver-{}{}",
        toolchain,
        consts::EXE_SUFFIX
    ));
    copy(&binary, driver).with_context(|| {
        format!(
            "Could not copy `{}` to `{}`",
            binary,
            driver.to_string_lossy()
        )
    })?;

    Ok(())
}

fn initialize(toolchain: &str, package: &Path) -> Result<()> {
    let version_spec = format!("version = \"={}\"", env!("CARGO_PKG_VERSION"));

    // smoelius: Assume the `dylint_driver` package is local if building in debug mode and if
    // `dylint_driver_local` is enabled.
    #[cfg(any(not(debug_assertions), not(feature = "dylint_driver_local")))]
    let path_spec = "";
    #[cfg(all(debug_assertions, feature = "dylint_driver_local"))]
    #[allow(clippy::expect_used)]
    let path_spec = format!(
        ", path = \"{}\"",
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("Could not get parent directory")
            .join("driver")
            .to_string_lossy()
            .replace('\\', "\\\\")
    );

    let dylint_driver_spec = format!("{}{}", version_spec, path_spec);

    let cargo_toml_path = package.join("Cargo.toml");
    write(&cargo_toml_path, cargo_toml(toolchain, &dylint_driver_spec))
        .with_context(|| format!("`write` failed for `{}`", cargo_toml_path.to_string_lossy()))?;
    let rust_toolchain_path = package.join("rust-toolchain");
    write(&rust_toolchain_path, rust_toolchain(toolchain)).with_context(|| {
        format!(
            "`write` failed for `{}`",
            rust_toolchain_path.to_string_lossy()
        )
    })?;
    let src = package.join("src");
    create_dir_all(&src)
        .with_context(|| format!("`create_dir_all` failed for `{}`", src.to_string_lossy()))?;
    let main_rs = src.join("main.rs");
    write(&main_rs, MAIN_RS)
        .with_context(|| format!("`write` failed for `{}`", main_rs.to_string_lossy()))?;

    Ok(())
}
