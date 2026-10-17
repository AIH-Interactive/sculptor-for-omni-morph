fn main() -> Result<(), Box<dyn std::error::Error>> {
    let schema = "protocol/omnimorph/cloud/v1/cloud.proto";
    println!("cargo:rerun-if-changed={schema}");
    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    config.compile_protos(&[schema], &["protocol"])?;
    Ok(())
}
