mod catalog;
mod input;
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

use proxy::{Generate, Modify, Proxy};

const GUIDANCE: &str = include_str!("../guidance.md");

#[derive(Parser)]
#[command(about = "Generate and modify local image assets through a CLIProxyAPI MCP server")]
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
        description = "Get available models and prompt-refinement guidance. Must succeed before image generation or modification. On failure, stop and report the error.",
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
        description = "Generate and save an image with the exact provider bytes. Use provider-native size values; aspect_ratio is Gemini-only. First get a successful response from the guidance tool. Generation is complete only when this tool returns a saved path. Never overwrites. One paid request, no automatic retry.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn generate(&self, Parameters(args): Parameters<Generate>) -> CallToolResult {
        self.result(self.proxy.generate(args).await)
    }

    #[tool(
        description = "Modify a local image with an OpenAI GPT image model using the actual source bytes and optional mask. First get guidance. Modification is complete only when this tool returns a saved path. Never overwrites the input or output. One paid request, no automatic retry. Unrelated pixels may change.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn modify(&self, Parameters(args): Parameters<Modify>) -> CallToolResult {
        self.result(self.proxy.modify(args).await)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ImageServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Before image generation or modification, call the guidance tool. If it fails, stop and report the error. Do not guess a model or call generate or modify without successful guidance. Use generate for new images and modify for edits to a local image. The modify tool supports only models with route openai. Do not orchestrate shell scripts, use curl, or research API syntax to perform a generate or modify request; call the tool directly. Generation or modification is complete only after generate or modify returns a saved image path. Do not report success before that. After the tool returns a saved path, you can postprocess that local file, for example with ImageMagick.")
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
    fn exposes_three_tools_with_generation_and_modification_arguments() {
        let tools = ImageServer::tool_router().list_all();
        let mut names: Vec<_> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        names.sort_unstable();
        assert_eq!(names, ["generate", "guidance", "modify"]);
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
        let properties = generation.input_schema["properties"].as_object().unwrap();
        assert_eq!(properties.len(), 5);
        assert!(properties.contains_key("size"));
        assert!(properties.contains_key("aspect_ratio"));
        assert!(!properties.contains_key("width"));
        assert!(!properties.contains_key("height"));
        assert_eq!(properties["size"]["type"], json!(["string", "null"]));
        assert!(properties["size"].get("enum").is_none());
        let mut required: Vec<_> = generation.input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        required.sort_unstable();
        assert_eq!(required, ["local_path", "model", "prompt"]);
        assert!(
            generation
                .description
                .as_ref()
                .unwrap()
                .contains("guidance tool")
        );
        let modification = tools.iter().find(|tool| tool.name == "modify").unwrap();
        let properties = modification.input_schema["properties"].as_object().unwrap();
        assert_eq!(properties.len(), 6);
        assert_eq!(properties["size"]["type"], json!(["string", "null"]));
        assert!(properties["size"].get("enum").is_none());
        assert!(!properties.contains_key("aspect_ratio"));
        assert!(!properties.contains_key("width"));
        assert!(!properties.contains_key("height"));
        let mut required: Vec<_> = modification.input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        required.sort_unstable();
        assert_eq!(required, ["input_path", "local_path", "model", "prompt"]);
        assert_eq!(modification.input_schema["additionalProperties"], false);
        assert!(GUIDANCE.contains("Preserve every explicit user requirement"));
        assert!(GUIDANCE.contains("actual source image"));
    }
}
