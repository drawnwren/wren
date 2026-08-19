fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/wren.proto");
    let descriptors = protox::compile(["proto/wren.proto"], ["proto"])?;
    prost_build::compile_fds(descriptors)?;
    Ok(())
}
