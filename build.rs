fn main() {
    // Proxy Rust — 原生 SQL 解析器,无需 FFI
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/");
}
