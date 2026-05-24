fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc should exist");
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }

    println!("cargo:rerun-if-changed=../../apps/wasi-grpc-echo/proto/echo.proto");
    tonic_prost_build::compile_protos("../../apps/wasi-grpc-echo/proto/echo.proto")
        .expect("grpc test proto compilation should succeed");
}
