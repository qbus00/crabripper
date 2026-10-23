// Compiles the vendored minimp3 (public-domain) via its thin C shim. Build-time
// only (the `cc` crate); no runtime dependency is added.

fn main() {
    cc::Build::new()
        .file("csrc/minimp3_shim.c")
        .include("vendor")
        .opt_level(2)
        .warnings(false)
        .compile("sr_minimp3");
    println!("cargo:rerun-if-changed=csrc/minimp3_shim.c");
    println!("cargo:rerun-if-changed=vendor/minimp3.h");
}
