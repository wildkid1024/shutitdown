fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::compile_protos("proto/node_agent.proto")?;
    println!("cargo:rerun-if-changed=proto/node_agent.proto");
    Ok(())
}
