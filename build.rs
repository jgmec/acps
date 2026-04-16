use std::io::Result;

fn main() -> Result<()> {
    prost_build::Config::new()
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_protos(&["src/apple.proto"], &["src/"])?;

    Ok(())
}
