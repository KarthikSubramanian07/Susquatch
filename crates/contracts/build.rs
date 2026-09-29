//! Generates `susq.v1` types from the single schema source in `/contracts/proto`.

use std::{env, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?).join("../../contracts/proto");
    let protos: Vec<PathBuf> = ["common", "targets", "bus", "ops", "needle"]
        .iter()
        .map(|p| root.join(format!("susq/v1/{p}.proto")))
        .collect();
    for p in &protos {
        println!("cargo:rerun-if-changed={}", p.display());
    }

    let out = PathBuf::from(env::var("OUT_DIR")?);
    let descriptor = out.join("susq_descriptor.bin");
    prost_build::Config::new()
        .file_descriptor_set_path(&descriptor)
        .compile_well_known_types()
        .extern_path(".google.protobuf", "::pbjson_types")
        .compile_protos(&protos, &[root])?;

    let set = std::fs::read(&descriptor)?;
    pbjson_build::Builder::new()
        .register_descriptors(&set)?
        .build(&[".susq"])?;
    Ok(())
}
