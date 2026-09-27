mod catalog;
mod output;
mod proxy;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use serde_json::{Value, json};

use proxy::{Generate, Proxy};

const GUIDANCE: &str = include_str!("../guidance.md");

#[derive(Parser)]
#[command(about = "Generate local image assets through a CLIProxyAPI MCP server")]
struct Args {
    #[arg(long, help = "CLIProxyAPI base URL, with or without /v1")]
    base_url: String,
    #[arg(long, help = "Name of the environment variable containing the API key")]
    api_key_env: String,
}

#[derive(Clone)]
struct ImageServer {
    proxy: Proxy,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl ImageServer {
    fn new(proxy: Proxy) -> Self {
        Self {
            proxy,
            tool_router: Self::tool_router(),
        }
    }

    fn result(&self, result: Result<Value>) -> CallToolResult {
        match result {
            Ok(value) => CallToolResult::structured(value),
            Err(error) => CallToolResult::error(vec![ContentBlock::text(
                self.proxy.redact(&format!("{error:#}")),
            )]),
        }
    }

    #[tool(
        description = "Get available models and prompt-refinement guidance. Must succeed before image generation. On failure, stop and report the error.",
        annotations(read_only_hint = true, idempotent_hint = true)
    )]
    async fn guidance(&self) -> CallToolResult {
        self.result(self.proxy.list_models().await.and_then(|catalog| {
            ensure!(
                catalog.models.iter().any(|model| model.route.is_some()),
                "No supported image-generation models are available. Cannot generate an image."
            );
            Ok(json!({
                "models": catalog.models,
                "warnings": catalog.warnings,
                "guidance": GUIDANCE,
            }))
        }))
    }

    #[tool(
        description = "Generate and save an image and its unmodified original. First get a successful response from the guidance tool. Never overwrites. One paid request, no automatic retry.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn generate(&self, Parameters(args): Parameters<Generate>) -> CallToolResult {
        self.result(self.proxy.generate(args).await)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ImageServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Before the first image generation, call the guidance tool. If it fails, stop and report the error. Do not guess a model or call the generate tool without a successful guidance response. Use the returned models and guidance to prepare the generate call. Do not orchestrate shell scripts. Report success only after the generate tool returns a saved path.")
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let key = std::env::var(&args.api_key_env)
        .with_context(|| format!("Missing API key environment variable: {}", args.api_key_env))?;
    let proxy = Proxy::new(&args.base_url, key)?;
    ImageServer::new(proxy)
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_exactly_two_tools_and_five_generation_arguments() {
        let tools = ImageServer::tool_router().list_all();
        let mut names: Vec<_> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        names.sort_unstable();
        assert_eq!(names, ["generate", "guidance"]);
        let guidance = tools.iter().find(|tool| tool.name == "guidance").unwrap();
        assert!(
            guidance
                .input_schema
                .get("properties")
                .is_none_or(|properties| properties.as_object().unwrap().is_empty())
        );
        assert!(
            guidance
                .input_schema
                .get("required")
                .is_none_or(|required| required.as_array().unwrap().is_empty())
        );
        let generation = tools.iter().find(|tool| tool.name == "generate").unwrap();
        assert_eq!(
            generation.input_schema["properties"]
                .as_object()
                .unwrap()
                .len(),
            5
        );
        assert_eq!(
            generation.input_schema["required"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
        assert!(
            generation
                .description
                .as_ref()
                .unwrap()
                .contains("guidance tool")
        );
        assert!(GUIDANCE.contains("Preserve every explicit user requirement"));
    }
}
