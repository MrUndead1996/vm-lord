use std::env;

fn main() {
    let target = env::var("TARGET").expect("Cargo must set target triple");
    if target.ends_with("pc-windows-msvc") {
        // The HCS backend requires an elevated process, so the executable
        // carries a `RequireAdministrator` application manifest.
        embed_resource::compile("vmlord.rc", embed_resource::NONE)
            .manifest_required()
            .expect("cannot embed VMLord application manifest");
    }
    println!("cargo:rerun-if-changed=vmlord.rc");
    println!("cargo:rerun-if-changed=vmlord.manifest");
}
