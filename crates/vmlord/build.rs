use std::{env, fs, path::PathBuf};

fn main() {
    let target = env::var("TARGET").expect("Cargo must set target triple");
    if target.ends_with("pc-windows-msvc") {
        // The HCS backend requires an elevated process, matching AppSandbox's
        // `RequireAdministrator` application manifest.
        embed_resource::compile("vmlord.rc", embed_resource::NONE)
            .manifest_required()
            .expect("cannot embed VMLord application manifest");
    }
    println!("cargo:rerun-if-changed=vmlord.rc");
    println!("cargo:rerun-if-changed=vmlord.manifest");

    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("Cargo must set manifest directory"));
    let runtime_directory = manifest_dir.join("../../third_party/appsandbox/x64");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("Cargo must set output directory"));
    let profile_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("Cargo output directory must be nested below a profile directory");

    for runtime_file in ["appsandbox_core.dll", "iso-patch.exe"] {
        let source = runtime_directory.join(runtime_file);
        println!("cargo:rerun-if-changed={}", source.display());
        assert!(
            source.is_file(),
            "required AppSandbox runtime file is missing: {}",
            source.display()
        );

        let destination = profile_dir.join(runtime_file);
        fs::copy(&source, &destination).unwrap_or_else(|error| {
            panic!(
                "cannot stage AppSandbox runtime file at {}: {error}",
                destination.display()
            )
        });
    }
}
