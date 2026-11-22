use std::env;

fn main() {
    println!("cargo:rerun-if-env-changed=SKYBER_A1_NATIVE_DIR");
    if env::var_os("CARGO_FEATURE_A1_NATIVE").is_none() {
        return;
    }
    match env::var("TARGET").as_deref() {
        Ok("x86_64-pc-windows-gnu") => {}
        Ok("x86_64-unknown-linux-gnu") => {
            println!("cargo:rustc-link-arg-bin=skyber-a1-peer=-Wl,-rpath,$ORIGIN");
        }
        _ => panic!("native proof codecs require x86-64 Windows GNU or Linux GNU"),
    }
    let directory = env::var("SKYBER_A1_NATIVE_DIR")
        .expect("set SKYBER_A1_NATIVE_DIR to the DLL and import library directory");
    println!("cargo:rustc-link-search=native={directory}");
    println!("cargo:rustc-link-lib=dylib=skyber_a1_codecs");
}
