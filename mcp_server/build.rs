// Generates the gRPC client for the sld_mcp add-in from the shared schema
// in ../proto. Uses a vendored protoc so building doesn't need one on PATH.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = tonic_prost_build::Config::new();
    config.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);

    tonic_prost_build::configure()
        .build_server(false)
        .compile_with_config(config, &["../proto/sld_mcp.proto"], &["../proto"])?;

    // Version resource for mcp_server.exe; file and product version come from
    // Cargo.toml. build.rs runs on the host, so check the target instead.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set("ProductName", "SolidWorks MCP")
            .set("FileDescription", "SolidWorks MCP server")
            .set("CompanyName", "Marc Wang")
            .set("LegalCopyright", "Copyright (C) 2026 Marc Wang")
            .compile()?;
    }
    Ok(())
}
