mod addin;

use addin::AddinClient;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::transport::stdio;
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use tonic::Code;
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
struct SolidWorksTools {
    tool_router: ToolRouter<SolidWorksTools>,
    addin: AddinClient,
}

#[tool_router]
impl SolidWorksTools {
    fn new(addin: AddinClient) -> Self {
        Self {
            tool_router: Self::tool_router(),
            addin,
        }
    }

    #[tool(description = "Get the name of the currently active SolidWorks document")]
    async fn get_active_document_name(&self) -> Result<CallToolResult, McpError> {
        let document = self
            .addin
            .get_active_document()
            .await
            .map_err(|status| self.addin_error(status))?;

        let name = document
            .map(|doc| doc.title)
            .unwrap_or_else(|| "(no active document)".to_string());
        Ok(CallToolResult::success(vec![ContentBlock::text(name)]))
    }
}

impl SolidWorksTools {
    /// Turns a gRPC failure into an MCP error with a message that tells the
    /// model (and the user) what to actually do about it.
    fn addin_error(&self, status: tonic::Status) -> McpError {
        let message = match status.code() {
            Code::Unavailable => format!(
                "can't reach the sld_mcp add-in at {} -- open SolidWorks with the \
                 sld_mcp add-in enabled ({})",
                self.addin.endpoint(),
                status.message()
            ),
            Code::DeadlineExceeded => {
                "SolidWorks didn't respond in time -- it may be busy or showing a dialog"
                    .to_string()
            }
            code => format!("sld_mcp add-in error ({code:?}): {}", status.message()),
        };
        McpError::internal_error(message, None)
    }
}

#[tool_handler]
impl ServerHandler for SolidWorksTools {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "Bridges to a running SolidWorks instance via the sld_mcp COM add-in, \
                 over gRPC on localhost. Requires SolidWorks to be running with that \
                 add-in enabled."
                    .to_string(),
            )
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // MCP stdio transport uses stdout for protocol messages, so all logging
    // must go to stderr -- anything on stdout would corrupt the stream.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let addin = AddinClient::new()?;
    let service = SolidWorksTools::new(addin).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
