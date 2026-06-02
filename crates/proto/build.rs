use std::path::PathBuf;

fn main() {
    let proto = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("protos")
        .join("rendezvous.proto");
    println!("cargo:rerun-if-changed={}", proto.display());

    let file_descriptors = protox::compile([&proto], [proto.parent().unwrap()])
        .expect("failed to compile rendezvous.proto with protox");

    prost_build::Config::new()
        .compile_fds(file_descriptors)
        .expect("failed to generate prost types");
}
