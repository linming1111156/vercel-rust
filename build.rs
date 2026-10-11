fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/nezha.proto");
    println!("cargo:rerun-if-env-changed=PROTOC");

    // prost-build 需要 protoc：优先用显式指定的，其次用内置的，这样构建机
    // （Vercel / CI / 容器）不需要预装 protoc。
    if std::env::var_os("PROTOC").is_none() {
        let protoc = protoc_bin_vendored::protoc_bin_path()?;
        // SAFETY: build scripts run single-threaded before any other thread reads env.
        unsafe { std::env::set_var("PROTOC", protoc) };
    }

    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&["proto/nezha.proto"], &["proto"])?;
    Ok(())
}
