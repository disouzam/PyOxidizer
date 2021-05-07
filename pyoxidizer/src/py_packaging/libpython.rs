// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/*!
Building a native binary containing Python.
*/

use {
    crate::{environment::Environment, py_packaging::distribution::AppleSdkInfo},
    anyhow::{anyhow, Context, Result},
    python_packaging::libpython::LibPythonBuildContext,
    slog::warn,
    std::{
        fs,
        fs::create_dir_all,
        path::{Path, PathBuf},
    },
    tugger_file_manifest::FileData,
};

/// Produce the content of the config.c file containing built-in extensions.
pub fn make_config_c<T>(extensions: &[(T, T)]) -> String
where
    T: AsRef<str>,
{
    // It is easier to construct the file from scratch than parse the template
    // and insert things in the right places.
    let mut lines: Vec<String> = vec!["#include \"Python.h\"".to_string()];

    // Declare the initialization functions.
    for (_name, init_fn) in extensions {
        if init_fn.as_ref() != "NULL" {
            lines.push(format!("extern PyObject* {}(void);", init_fn.as_ref()));
        }
    }

    lines.push(String::from("struct _inittab _PyImport_Inittab[] = {"));

    for (name, init_fn) in extensions {
        lines.push(format!("{{\"{}\", {}}},", name.as_ref(), init_fn.as_ref()));
    }

    lines.push(String::from("{0, 0}"));
    lines.push(String::from("};"));

    lines.join("\n")
}

#[derive(Debug)]
pub struct LibpythonInfo {
    pub libpython_path: PathBuf,
    pub libpyembeddedconfig_path: PathBuf,
    pub cargo_metadata: Vec<String>,
}

/// Create a static libpython from a Python distribution.
///
/// Returns a vector of cargo: lines that can be printed in build scripts.
#[allow(clippy::too_many_arguments)]
pub fn link_libpython(
    logger: &slog::Logger,
    env: &Environment,
    context: &LibPythonBuildContext,
    out_dir: &Path,
    host_triple: &str,
    target_triple: &str,
    opt_level: &str,
    apple_sdk_info: Option<&AppleSdkInfo>,
) -> Result<LibpythonInfo> {
    let mut cargo_metadata: Vec<String> = Vec::new();

    let temp_dir = tempfile::Builder::new().prefix("libpython").tempdir()?;
    let temp_dir_path = temp_dir.path();

    let windows = crate::environment::WINDOWS_TARGET_TRIPLES.contains(&target_triple);

    // Sometimes we have canonicalized paths. These can break cc/cl.exe when they
    // are \\?\ paths on Windows for some reason. We hack around this by doing
    // operations in the temp directory and copying files to their final resting
    // place.

    // We derive a custom Modules/config.c from the set of extension modules.
    // We need to do this because config.c defines the built-in extensions and
    // their initialization functions and the file generated by the source
    // distribution may not align with what we want.
    warn!(
        logger,
        "deriving custom config.c from {} extension modules",
        context.init_functions.len()
    );
    let config_c_source = make_config_c(&context.init_functions.iter().collect::<Vec<_>>());
    let config_c_path = out_dir.join("config.c");
    let config_c_temp_path = temp_dir_path.join("config.c");

    fs::write(&config_c_path, config_c_source.as_bytes())?;
    fs::write(&config_c_temp_path, config_c_source.as_bytes())?;

    // Gather all includes into the temporary directory.
    for (rel_path, location) in &context.includes {
        let full = temp_dir_path.join(rel_path);
        create_dir_all(
            full.parent()
                .ok_or_else(|| anyhow!("unable to resolve parent directory"))?,
        )?;
        let data = location.resolve()?;
        std::fs::write(&full, &data)?;
    }

    warn!(logger, "compiling custom config.c to object file");
    let mut build = cc::Build::new();

    if let Some(flags) = &context.inittab_cflags {
        for flag in flags {
            build.flag(flag);
        }
    }

    // The cc crate will pick up the default Apple SDK by default. There could be a mismatch
    // between it and what we want. For example, if we're building for aarch64 but the default
    // SDK is a 10.15 SDK that doesn't support ARM. We attempt to mitigate this by resolving
    // a compatible Apple SDK and pointing the compiler invocation at it via compiler flags.
    if target_triple.contains("-apple-") {
        let sdk_info = apple_sdk_info.ok_or_else(|| {
            anyhow!("Apple SDK info should be defined when targeting Apple platforms")
        })?;

        let sdk = env
            .resolve_apple_sdk(logger, sdk_info)
            .context("resolving Apple SDK to use")?;

        build.flag("-isysroot");
        build.flag(&format!("{}", sdk.path.display()));
    }

    build
        .out_dir(out_dir)
        .host(host_triple)
        .target(target_triple)
        .opt_level_str(opt_level)
        .file(config_c_temp_path)
        .include(temp_dir_path)
        .cargo_metadata(false)
        .compile("pyembeddedconfig");

    let libpyembeddedconfig_path = out_dir.join(if windows {
        "pyembeddedconfig.lib"
    } else {
        "libpyembeddedconfig.a"
    });

    // Since we disabled cargo metadata lines above.
    cargo_metadata.push("cargo:rustc-link-lib=static=pyembeddedconfig".to_string());

    warn!(logger, "resolving inputs for custom Python library...");
    let mut build = cc::Build::new();
    build.out_dir(out_dir);
    build.host(host_triple);
    build.target(target_triple);
    build.opt_level_str(opt_level);
    // We handle this ourselves.
    build.cargo_metadata(false);

    for (i, location) in context.object_files.iter().enumerate() {
        match location {
            FileData::Memory(data) => {
                let out_path = temp_dir_path.join(format!("libpython.{}.o", i));
                fs::write(&out_path, data)?;
                build.object(&out_path);
            }
            FileData::Path(p) => {
                build.object(&p);
            }
        }
    }

    for framework in &context.frameworks {
        cargo_metadata.push(format!("cargo:rustc-link-lib=framework={}", framework));
    }

    for lib in &context.system_libraries {
        cargo_metadata.push(format!("cargo:rustc-link-lib={}", lib));
    }

    for lib in &context.dynamic_libraries {
        cargo_metadata.push(format!("cargo:rustc-link-lib={}", lib));
    }

    for lib in &context.static_libraries {
        cargo_metadata.push(format!("cargo:rustc-link-lib=static={}", lib));
    }

    // Python 3.9+ on macOS uses __builtin_available(), which requires
    // ___isOSVersionAtLeast(), which is part of libclang_rt. However,
    // libclang_rt isn't linked by default by Rust. So unless something else
    // pulls it in, we'll get unresolved symbol errors when attempting to link
    // the final binary. Our solution to this is to always annotate
    // `clang_rt.<platform>` as a library dependency of our static libpython.
    if target_triple.ends_with("-apple-darwin") {
        if let Some(path) = macos_clang_search_path()? {
            cargo_metadata.push(format!("cargo:rustc-link-search={}", path.display()));
        }

        cargo_metadata.push("cargo:rustc-link-lib=clang_rt.osx".to_string());
    }

    // python3-sys uses #[link(name="pythonXY")] attributes heavily on Windows. Its
    // build.rs then remaps ``pythonXY`` to e.g. ``python37``. This causes Cargo to
    // link against ``python37.lib`` (or ``pythonXY.lib`` if the
    // ``rustc-link-lib=pythonXY:python{}{}`` line is missing, which is the case
    // in our invocation).
    //
    // We don't want the "real" libpython being linked. And this is a very real
    // possibility since the path to it could be in an environment variable
    // outside of our control!
    //
    // In addition, we can't naively remap ``pythonXY`` ourselves without adding
    // a ``#[link]`` to the crate.
    //
    // Our current workaround is to produce a ``pythonXY.lib`` file. This satisfies
    // the requirement of ``python3-sys`` that a ``pythonXY.lib`` file exists.

    warn!(logger, "compiling libpythonXY...");
    build.compile("pythonXY");
    warn!(logger, "libpythonXY created");

    let libpython_path = out_dir.join(if windows {
        "pythonXY.lib"
    } else {
        "libpythonXY.a"
    });

    cargo_metadata.push("cargo:rustc-link-lib=static=pythonXY".to_string());
    cargo_metadata.push(format!(
        "cargo:rustc-link-search=native={}",
        out_dir.display()
    ));

    for path in &context.library_search_paths {
        cargo_metadata.push(format!("cargo:rustc-link-search=native={}", path.display()));
    }

    Ok(LibpythonInfo {
        libpython_path,
        libpyembeddedconfig_path,
        cargo_metadata,
    })
}

/// Attempt to resolve the linker search path for clang libraries.
fn macos_clang_search_path() -> Result<Option<PathBuf>> {
    let output = std::process::Command::new("clang")
        .arg("--print-search-dirs")
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.contains("libraries: =") {
            let path = line
                .split('=')
                .nth(1)
                .ok_or_else(|| anyhow!("could not parse libraries line"))?;
            return Ok(Some(PathBuf::from(path).join("lib").join("darwin")));
        }
    }

    Ok(None)
}
