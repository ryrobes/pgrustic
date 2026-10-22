fn main() {
    // The image ships libpq.so.5 without a libpq.so linker name.
    let out = std::env::var("OUT_DIR").unwrap();
    let dest = std::path::Path::new(&out).join("libpq.so");
    let _ = std::fs::remove_file(&dest);
    std::os::unix::fs::symlink("/usr/lib/x86_64-linux-gnu/libpq.so.5", &dest)
        .expect("symlink libpq.so.5");
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=dylib=pq");
    println!("cargo:rerun-if-changed=build.rs");
}
