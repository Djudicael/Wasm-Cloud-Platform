fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc should exist");
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }

    println!("cargo:rerun-if-changed=proto/echo.proto");
    prost_build::Config::new()
        .compile_protos(&["proto/echo.proto"], &["proto"])
        .expect("proto compilation should succeed");
}
