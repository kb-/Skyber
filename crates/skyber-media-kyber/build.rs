use std::env;

fn main() {
    println!("cargo:rerun-if-env-changed=SKYBER_A1_NATIVE_DIR");
    if env::var_os("CARGO_FEATURE_A1_NATIVE").is_none() {
        return;
    }
    assert_eq!(
        env::var("TARGET").as_deref(),
        Ok("x86_64-pc-windows-gnu"),
        "A1 native codecs require the Windows GNU target"
    );
    let directory = env::var("SKYBER_A1_NATIVE_DIR")
        .expect("set SKYBER_A1_NATIVE_DIR to the DLL and import library directory");
    println!("cargo:rustc-link-search=native={directory}");
    println!("cargo:rustc-link-lib=dylib=skyber_a1_codecs");
}
