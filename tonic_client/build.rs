use std::path::PathBuf;

fn main() {
    let descriptor_path = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("descriptor.bin");
    tonic_prost_build::configure()
        .build_server(false)
        .file_descriptor_set_path(descriptor_path)
        .compile_protos(&["proto/helloworld/helloworld.proto"], &["proto/helloworld"])
        .unwrap();
}
