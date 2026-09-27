use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use reqwest::{
    Client, RequestBuilder, Url,
    header::{AUTHORIZATION, HeaderValue},
    redirect::Policy,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{
    catalog::{self, Catalog, Route},
    input::InputImage,
    output::{self, Destination},
};

const CATALOG_URL: &str =
    "https://raw.githubusercontent.com/router-for-me/models/refs/heads/main/models.json";
const MAX_RESPONSE: usize = 64 * 1024 * 1024;

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Generate {
    #[schemars(
        description = "Exact model slug returned by the guidance tool, without an llm-proxy/ prefix."
    )]
    pub model: String,
    #[schemars(
        description = "Absolute destination ending in .png, .jpg, .jpeg, or .webp. The parent must exist. Existing files are never overwritten."
    )]
    pub local_path: String,
    #[schemars(
        description = "Final width in pixels, 64 to 4096. Use 1024 unless the user needs another size."
    )]
    pub width: u32,
    #[schemars(
        description = "Final height in pixels, 64 to 4096. The server resizes and center-crops if needed."
    )]
    pub height: u32,
    #[schemars(
        description = "Complete art-directed prompt. First get a successful response from the guidance tool. Preserve every explicit user requirement. The server sends and saves this exact text."
    )]
    pub prompt: String,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Modify {
    #[schemars(
        description = "Exact model slug with route openai returned by the guidance tool. Other providers are not supported by modify."
    )]
    pub model: String,
    #[schemars(
        description = "Absolute path to a PNG, JPEG, or WebP source image under 50 MB. The server uploads its actual bytes and never changes this file."
    )]
    pub input_path: String,
    #[schemars(
        description = "Absolute destination ending in .png, .jpg, .jpeg, or .webp. The parent must exist. Existing files are never overwritten."
    )]
    pub local_path: String,
    #[schemars(
        description = "Exact editing instructions: describe the changes and what must remain unchanged. First get a successful response from the guidance tool."
    )]
    pub prompt: String,
    #[schemars(
        description = "Final width in pixels, 64 to 4096. Provide both width and height, or omit both to use source dimensions. The server resizes and center-crops if needed."
    )]
    pub width: Option<u32>,
    #[schemars(
        description = "Final height in pixels, 64 to 4096. Provide both width and height, or omit both to use source dimensions. Aspect ratio must be between 1:8 and 8:1."
    )]
    pub height: Option<u32>,
    #[schemars(
        description = "Optional absolute path to a PNG mask under 4 MB with an alpha channel and source-matching dimensions. Fully transparent pixels mark the area to edit."
    )]
    pub mask_path: Option<String>,
}

#[derive(Clone)]
pub struct Proxy {
    client: Client,
    base: Url,
    auth: HeaderValue,
    secret: String,
    metadata_url: Url,
    cache: Arc<Mutex<Option<(Instant, Catalog)>>>,
}

impl Proxy {
    pub fn new(base: &str, key: String) -> Result<Self> {
        let mut base = Url::parse(base).context("Invalid CLIProxyAPI base URL")?;
        ensure!(
            matches!(base.scheme(), "https" | "http") && base.host_str().is_some(),
            "Base URL must use HTTP or HTTPS"
        );
        ensure!(
            base.username().is_empty()
                && base.password().is_none()
                && base.query().is_none()
                && base.fragment().is_none(),
            "Base URL must not contain credentials, a query, or a fragment"
        );
        if base.scheme() == "http" {
            ensure!(
                matches!(base.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
                "Use HTTPS except for a loopback proxy"
            );
        }
        let path = base.path().trim_end_matches('/');
        let path = if path.ends_with("/v1") {
            format!("{path}/")
        } else {
            format!("{path}/v1/")
        };
        base.set_path(&path);
        ensure!(
            !key.trim().is_empty(),
            "The CLIProxyAPI key environment variable is empty"
        );
        let mut auth =
            HeaderValue::from_str(&format!("Bearer {key}")).context("Invalid API key header")?;
        auth.set_sensitive(true);
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(240))
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .user_agent("image-generation-mcp/0.1")
            .build()?;
        Ok(Self {
            client,
            base,
            auth,
            secret: key,
            metadata_url: Url::parse(CATALOG_URL)?,
            cache: Arc::new(Mutex::new(None)),
        })
    }

    pub fn redact(&self, text: &str) -> String {
        text.replace(&self.secret, "[redacted]")
            .chars()
            .take(2000)
            .collect()
    }

    async fn request_json(
        &self,
        request: RequestBuilder,
        max_bytes: usize,
        generation: bool,
    ) -> Result<Value> {
        let mut response = request.send().await.map_err(|error| {
            let advice = if generation {
                " No automatic retry: the upstream request may still complete and consume quota."
            } else {
                ""
            };
            anyhow::anyhow!(
                "HTTP request failed: {}.{advice}",
                self.redact(&error.without_url().to_string())
            )
        })?;
        let status = response.status();
        let limit = if status.is_success() { max_bytes } else { 8192 };
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("Cannot read API response. Do not automatically repeat the request")?
        {
            if body.len() + chunk.len() > limit {
                if !status.is_success() {
                    break;
                }
                bail!("API response exceeds the size limit; no automatic retry");
            }
            body.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let message = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|value| {
                    value
                        .pointer("/error/message")
                        .or_else(|| value.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| "No structured error message".into());
            bail!(
                "CLIProxyAPI HTTP {}: {}. No automatic retry or model switch.",
                status.as_u16(),
                self.redact(&message)
            );
        }
        serde_json::from_slice(&body).context("API returned invalid JSON")
    }

    pub async fn list_models(&self) -> Result<Catalog> {
        let mut cache = self.cache.lock().await;
        if let Some((_, catalog)) = cache
            .as_ref()
            .filter(|(time, _)| time.elapsed() < Duration::from_secs(300))
        {
            return Ok(catalog.clone());
        }
        let live = self
            .client
            .get(self.base.join("models")?)
            .header(AUTHORIZATION, self.auth.clone())
            .timeout(Duration::from_secs(30));
        let metadata = self
            .client
            .get(self.metadata_url.clone())
            .timeout(Duration::from_secs(15));
        let (live, metadata) = tokio::join!(
            self.request_json(live, 4 * 1024 * 1024, false),
            self.request_json(metadata, 8 * 1024 * 1024, false)
        );
        let live = live?;
        let catalog = match metadata {
            Ok(metadata) => match catalog::compile(live.clone(), Some(&metadata)) {
                Ok(catalog) => catalog,
                Err(_) => {
                    let mut catalog = catalog::compile(live, None)?;
                    catalog.warnings.push(
                        "Metadata catalog is invalid; image model names supply candidates.".into(),
                    );
                    catalog
                }
            },
            Err(error) => {
                let mut catalog = catalog::compile(live, None)?;
                catalog.warnings.push(format!(
                    "Metadata unavailable: {}",
                    self.redact(&error.to_string())
                ));
                catalog
            }
        };
        *cache = Some((Instant::now(), catalog.clone()));
        Ok(catalog)
    }

    pub async fn generate(&self, args: Generate) -> Result<Value> {
        validate(&args)?;
        let destination = Destination::prepare(Path::new(&args.local_path))?;
        let catalog = self.list_models().await?;
        let model = catalog.models.iter().find(|item| item.model == args.model || item.account_ids.contains(&args.model))
            .context("Model is not in the live image catalog. Call the guidance tool and use an exact returned slug.")?;
        let route = model
            .route
            .context("This image model family has no supported CLIProxyAPI route")?;
        let request = generation_request(&args, route);
        let response = self
            .request_json(
                self.client
                    .post(self.base.join(route.endpoint())?)
                    .header(AUTHORIZATION, self.auth.clone())
                    .json(&request),
                MAX_RESPONSE,
                true,
            )
            .await?;
        let decoded = output::decode(&response)?;
        let metadata = json!({"request": request, "response_model": response.get("model"), "requested_dimensions": {"width": args.width, "height": args.height}});
        tokio::task::spawn_blocking(move || {
            destination.save(decoded, args.width, args.height, metadata)
        })
        .await
        .context("Image save worker failed")?
    }

    pub async fn modify(&self, args: Modify) -> Result<Value> {
        validate_prompt_model(&args.prompt, &args.model)?;
        ensure!(
            args.model
                .rsplit('/')
                .next()
                .is_some_and(|model| model.starts_with("gpt-image-")),
            "modify supports only OpenAI GPT image models with route openai"
        );
        ensure!(
            args.width.is_some() == args.height.is_some(),
            "Provide both width and height, or omit both to use source dimensions"
        );
        if let (Some(width), Some(height)) = (args.width, args.height) {
            validate_dimensions(width, height)?;
        }
        let destination = Destination::prepare(Path::new(&args.local_path))?;
        let (output, mut request, mut metadata) =
            tokio::task::spawn_blocking(move || prepare_modification(args))
                .await
                .context("Image input worker failed")??;
        let catalog = self.list_models().await?;
        let model = catalog.models.iter().find(|item| item.model == output.model || item.account_ids.contains(&output.model))
            .context("Model is not in the live image catalog. Call the guidance tool and use an exact returned slug.")?;
        ensure!(
            model.route == Some(Route::Openai),
            "modify supports only OpenAI GPT image models with route openai"
        );
        let response = self
            .request_json(
                self.client
                    .post(self.base.join("images/edits")?)
                    .header(AUTHORIZATION, self.auth.clone())
                    .json(&request),
                MAX_RESPONSE,
                true,
            )
            .await?;
        request.as_object_mut().unwrap().remove("images");
        request.as_object_mut().unwrap().remove("mask");
        metadata["request"] = request;
        metadata["response_model"] = json!(response.get("model"));
        let decoded = output::decode(&response)?;
        tokio::task::spawn_blocking(move || {
            destination.save(decoded, output.width, output.height, metadata)
        })
        .await
        .context("Image save worker failed")?
    }
}

fn prepare_modification(args: Modify) -> Result<(Generate, Value, Value)> {
    let input = InputImage::load(Path::new(&args.input_path), None)?;
    let mask = args
        .mask_path
        .as_ref()
        .map(|path| InputImage::load(Path::new(path), Some(&input)))
        .transpose()?;
    let output = Generate {
        model: args.model,
        local_path: args.local_path,
        prompt: args.prompt,
        width: args.width.unwrap_or(input.width),
        height: args.height.unwrap_or(input.height),
    };
    validate(&output)
        .context("Invalid modification output. Supply width and height within the output limits")?;
    let mut request = generation_request(&output, Route::Openai);
    request.as_object_mut().unwrap().remove("response_format");
    request["images"] = json!([{"image_url": input.data_url()}]);
    let model = output.model.rsplit('/').next().unwrap_or(&output.model);
    if model == "gpt-image-1" || model == "gpt-image-1.5" || model.starts_with("gpt-image-1.5-") {
        request["input_fidelity"] = json!("high");
    }
    if args.width.is_none() && args.height.is_none() {
        request["size"] = json!("auto");
    }
    if let Some(mask) = &mask {
        request["mask"] = json!({"image_url": mask.data_url()});
    }
    ensure!(
        serde_json::to_vec(&request)?.len() <= MAX_RESPONSE,
        "Image edit request exceeds the 64 MiB proxy limit; use smaller input files"
    );
    let metadata = json!({
        "operation": "modify", "input": input.metadata(), "mask": mask.as_ref().map(InputImage::metadata),
        "requested_dimensions": {"width": output.width, "height": output.height},
    });
    Ok((output, request, metadata))
}

pub fn validate(args: &Generate) -> Result<()> {
    validate_dimensions(args.width, args.height)?;
    validate_prompt_model(&args.prompt, &args.model)
}

fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    ensure!(
        (64..=4096).contains(&width) && (64..=4096).contains(&height),
        "width and height must be between 64 and 4096 pixels"
    );
    ensure!(
        width <= height * 8 && height <= width * 8,
        "Aspect ratio must be between 1:8 and 8:1"
    );
    Ok(())
}

fn validate_prompt_model(prompt: &str, model: &str) -> Result<()> {
    ensure!(
        !prompt.trim().is_empty() && prompt.len() <= 32000,
        "prompt must contain 1 to 32000 bytes"
    );
    ensure!(
        !model.trim().is_empty() && model.len() <= 256,
        "Invalid model slug"
    );
    Ok(())
}

pub fn generation_request(args: &Generate, route: Route) -> Value {
    match route {
        Route::Openai => {
            let size = if args.width == args.height {
                "1024x1024"
            } else if args.width > args.height {
                "1536x1024"
            } else {
                "1024x1536"
            };
            json!({"model": args.model, "prompt": args.prompt, "n": 1, "size": size,
                "quality": "auto", "output_format": "png", "response_format": "b64_json"})
        }
        Route::Gemini => {
            let canonical = args.model.rsplit('/').next().unwrap_or(&args.model);
            let flash_image = canonical.starts_with("gemini-3.1-flash-image");
            let mut ratios = vec![
                (1, 1),
                (2, 3),
                (3, 2),
                (3, 4),
                (4, 3),
                (4, 5),
                (5, 4),
                (9, 16),
                (16, 9),
                (21, 9),
            ];
            if flash_image {
                ratios.extend([(1, 4), (1, 8), (4, 1), (8, 1), (9, 21)]);
            }
            let ratio = f64::from(args.width) / f64::from(args.height);
            let (w, h) = ratios
                .into_iter()
                .min_by(|(aw, ah), (bw, bh)| {
                    (ratio / (f64::from(*aw) / f64::from(*ah)))
                        .ln()
                        .abs()
                        .total_cmp(&(ratio / (f64::from(*bw) / f64::from(*bh))).ln().abs())
                })
                .unwrap();
            let mut config = json!({"aspect_ratio": format!("{w}:{h}")});
            if flash_image || canonical.starts_with("gemini-3-pro-image") {
                let resolution = match args.width.max(args.height) {
                    0..=512 if flash_image => "512",
                    0..=1024 => "1K",
                    1025..=2048 => "2K",
                    _ => "4K",
                };
                config["image_size"] = json!(resolution);
            }
            json!({"model": args.model, "messages": [{"role": "user", "content": args.prompt}],
                "modalities": ["text", "image"], "image_config": config, "stream": false})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    fn args(path: String) -> Generate {
        Generate {
            model: "gpt-image-test".into(),
            local_path: path,
            width: 1024,
            height: 1024,
            prompt: "exact fox prompt".into(),
        }
    }

    fn edit_args(input_path: &Path, local_path: &Path) -> Modify {
        Modify {
            model: "gpt-image-test".into(),
            input_path: input_path.to_str().unwrap().into(),
            local_path: local_path.to_str().unwrap().into(),
            prompt: "Change only the coat to red.\nKeep the face and background unchanged.".into(),
            width: None,
            height: None,
            mask_path: None,
        }
    }

    fn edit_source(path: &Path, width: u32, height: u32) {
        image::RgbImage::from_pixel(width, height, image::Rgb([80, 160, 200]))
            .save(path)
            .unwrap();
    }

    #[test]
    fn modification_request_preserves_bytes_and_selects_model_fidelity() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.jpg");
        edit_source(&input, 128, 96);
        let bytes = std::fs::read(&input).unwrap();
        let expected = format!("data:image/jpeg;base64,{}", STANDARD.encode(&bytes));
        for (model, fidelity) in [
            ("gpt-image-1", Some("high")),
            ("gpt-image-1.5", Some("high")),
            ("account/gpt-image-1.5-2025-12-16", Some("high")),
            ("gpt-image-1-mini", None),
            ("gpt-image-2", None),
            ("account/gpt-image-2-2026-04-21", None),
            ("gpt-image-2.5-flare", None),
        ] {
            let mut args = edit_args(&input, &directory.path().join("output.png"));
            args.model = model.into();
            let (output, request, metadata) = prepare_modification(args.clone()).unwrap();
            assert_eq!((output.width, output.height), (128, 96));
            assert_eq!(request["images"], json!([{"image_url": expected}]));
            assert_eq!(request["model"], model);
            assert_eq!(request["prompt"], args.prompt);
            assert_eq!(request["size"], "auto");
            assert_eq!(
                request.get("input_fidelity").and_then(Value::as_str),
                fidelity
            );
            assert!(request.get("response_format").is_none());
            assert!(request.get("mask").is_none());
            assert_eq!(metadata["operation"], "modify");
            assert_eq!(metadata["input"]["path"], input.to_str().unwrap());
            assert_eq!(metadata["input"]["bytes"], bytes.len());
            assert!(metadata["mask"].is_null());
        }
        assert_eq!(std::fs::read(input).unwrap(), bytes);
    }

    #[test]
    fn modification_respects_output_dimensions_and_keeps_masks_unmodified() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.png");
        edit_source(&input, 128, 96);
        let mask = directory.path().join("mask.png");
        image::RgbaImage::new(128, 96).save(&mask).unwrap();
        let mask_bytes = std::fs::read(&mask).unwrap();
        let mut args = edit_args(&input, &directory.path().join("output.png"));
        args.mask_path = Some(mask.to_str().unwrap().into());
        args.width = Some(64);
        args.height = Some(128);
        let (output, request, metadata) = prepare_modification(args.clone()).unwrap();
        assert_eq!((output.width, output.height), (64, 128));
        assert_eq!(request["size"], "1024x1536");
        assert_eq!(
            request["mask"]["image_url"],
            format!("data:image/png;base64,{}", STANDARD.encode(&mask_bytes))
        );
        assert_eq!(metadata["mask"]["width"], 128);
        assert_eq!(metadata["mask"]["height"], 96);
        assert_eq!(std::fs::read(&mask).unwrap(), mask_bytes);
        edit_source(&input, 4097, 64);
        args.mask_path = None;
        args.width = None;
        args.height = None;
        assert!(prepare_modification(args.clone()).is_err());
        args.width = Some(512);
        args.height = Some(64);
        assert!(prepare_modification(args).is_ok());
    }

    #[tokio::test]
    async fn modification_uploads_images_and_saves_without_embedding_them_in_metadata() {
        let server = MockServer::start().await;
        let proxy = setup(&server).await;
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.webp");
        edit_source(&input, 128, 96);
        let bytes = std::fs::read(&input).unwrap();
        let output_path = directory.path().join("output.png");
        let response = output::tests::fixture(image::ImageFormat::Jpeg);
        Mock::given(method("POST"))
            .and(path("/v1/images/edits"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"b64_json": STANDARD.encode(&response)}],
            })))
            .expect(1)
            .mount(&server)
            .await;
        let args = edit_args(&input, &output_path);
        let result = proxy.modify(args.clone()).await.unwrap();
        assert_eq!(result["width"], 128);
        assert_eq!(result["height"], 96);
        assert_eq!(
            std::fs::read(result["original_path"].as_str().unwrap()).unwrap(),
            response
        );
        assert_eq!(std::fs::read(&input).unwrap(), bytes);
        let sidecar_bytes = std::fs::read(result["prompt_file"].as_str().unwrap()).unwrap();
        let metadata: Value = serde_json::from_slice(&sidecar_bytes).unwrap();
        assert_eq!(metadata["request"]["prompt"], args.prompt);
        assert_eq!(metadata["input"]["path"], input.to_str().unwrap());
        assert_eq!(metadata["input"]["mime_type"], "image/webp");
        assert!(metadata["request"].get("images").is_none());
        assert!(metadata["request"].get("mask").is_none());
        assert!(!String::from_utf8(sidecar_bytes).unwrap().contains("base64"));
        let requests = server.received_requests().await.unwrap();
        let request = requests
            .iter()
            .find(|request| request.method == "POST")
            .unwrap()
            .body_json::<Value>()
            .unwrap();
        assert_eq!(
            request["images"][0]["image_url"],
            format!("data:image/webp;base64,{}", STANDARD.encode(&bytes))
        );
        assert_eq!(request["prompt"], args.prompt);
        assert!(proxy.modify(args).await.is_err());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 4);
    }

    #[tokio::test]
    async fn invalid_modifications_fail_before_any_api_requests() {
        let server = MockServer::start().await;
        let proxy = Proxy::new(&server.uri(), "test-key".into()).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.png");
        edit_source(&input, 128, 96);
        let output = directory.path().join("output.png");
        let args = edit_args(&input, &output);
        let cases = [
            json!({"model": "gemini-image-test"}),
            json!({"model": ""}),
            json!({"prompt": " "}),
            json!({"width": 64}),
            json!({"height": 64}),
            json!({"width": 0, "height": 64}),
            json!({"width": 4097, "height": 64}),
            json!({"width": 64, "height": 4096}),
            json!({"input_path": "relative.png"}),
            json!({"input_path": directory.path()}),
            json!({"input_path": directory.path().join("missing.png")}),
            json!({"mask_path": input}),
            json!({"local_path": "relative.png"}),
            json!({"local_path": input}),
            json!({"local_path": directory.path().join("output.gif")}),
        ];
        for changes in cases {
            let mut value = serde_json::to_value(&args).unwrap();
            value
                .as_object_mut()
                .unwrap()
                .extend(changes.as_object().unwrap().clone());
            let request: Modify = serde_json::from_value(value).unwrap();
            assert!(proxy.modify(request).await.is_err(), "{changes}");
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        }
        for suffix in [
            ".prompt.json",
            ".original.png",
            ".original.jpg",
            ".original.webp",
        ] {
            let conflict = directory.path().join(format!("output.png{suffix}"));
            std::os::unix::fs::symlink(directory.path().join("missing"), &conflict).unwrap();
            assert!(proxy.modify(args.clone()).await.is_err());
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
            std::fs::remove_file(conflict).unwrap();
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn modification_rejects_unknown_models_and_never_retries_failed_edits() {
        let server = MockServer::start().await;
        let proxy = setup(&server).await;
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.png");
        edit_source(&input, 128, 96);
        let bytes = std::fs::read(&input).unwrap();
        let mut args = edit_args(&input, &directory.path().join("output.png"));
        args.model = "gpt-image-unknown".into();
        assert!(
            proxy
                .modify(args.clone())
                .await
                .unwrap_err()
                .to_string()
                .contains("live image catalog")
        );
        args.model = "gpt-image-test".into();
        Mock::given(method("POST"))
            .and(path("/v1/images/edits"))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_body_json(json!({"error": {"message": "test-key quota exceeded"}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let error = proxy.modify(args).await.unwrap_err().to_string();
        assert!(error.contains("429"));
        assert!(!error.contains("test-key"));
        assert_eq!(std::fs::read(&input).unwrap(), bytes);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn builds_provider_requests_without_changing_the_prompt() {
        let mut args = args("/tmp/test.png".into());
        assert_eq!(
            generation_request(&args, Route::Openai)["prompt"],
            args.prompt
        );
        args.model = "gemini-3.1-flash-image".into();
        args.width = 1920;
        args.height = 1080;
        let gemini = generation_request(&args, Route::Gemini);
        assert_eq!(gemini["messages"][0]["content"], args.prompt);
        assert_eq!(gemini["image_config"]["aspect_ratio"], "16:9");
        assert_eq!(gemini["image_config"]["image_size"], "2K");
        args.width = 0;
        assert!(validate(&args).is_err());
        assert!(Proxy::new("https://user:secret@example.com", "key".into()).is_err());
        assert!(Proxy::new("http://example.com", "key".into()).is_err());
    }

    async fn setup(server: &MockServer) -> Proxy {
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"data": [{"id": "gpt-image-test"}, {"id": "gemini-image-test"}]}),
            ))
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(server)
            .await;
        let mut proxy = Proxy::new(&server.uri(), "test-key".into()).unwrap();
        proxy.metadata_url = Url::parse(&format!("{}/metadata", server.uri())).unwrap();
        proxy
    }

    #[test]
    fn gemini_dimensions_follow_model_capabilities() {
        let mut args = args("/tmp/test.png".into());
        args.width = 4096;
        args.height = 512;
        for model in [
            "gemini-2.5-flash-image",
            "account/gemini-3-pro-image-preview",
            "account/gemini-3.1-flash-image",
        ] {
            args.model = model.into();
            let config = generation_request(&args, Route::Gemini)["image_config"].clone();
            if model.contains("3.1-flash") {
                assert_eq!(config["aspect_ratio"], "8:1");
                assert_eq!(config["image_size"], "4K");
            } else {
                assert_eq!(config["aspect_ratio"], "21:9");
                if model.contains("3-pro") {
                    assert_eq!(config["image_size"], "4K");
                } else {
                    assert!(config.get("image_size").is_none());
                }
            }
        }
        args.width = 512;
        args.height = 512;
        assert_eq!(
            generation_request(&args, Route::Gemini)["image_config"]["image_size"],
            "512"
        );
        for (width, height) in [(64, 64), (4096, 4096), (4096, 512), (512, 4096)] {
            args.width = width;
            args.height = height;
            assert!(validate(&args).is_ok());
        }
        for (width, height) in [
            (0, 1024),
            (63, 64),
            (4097, 4096),
            (4096, 64),
            (64, 4096),
            (u32::MAX, u32::MAX),
        ] {
            args.width = width;
            args.height = height;
            assert!(validate(&args).is_err());
        }
    }

    #[tokio::test]
    async fn discovers_caches_generates_and_saves() {
        let server = MockServer::start().await;
        let proxy = setup(&server).await;
        let bytes = output::tests::fixture(image::ImageFormat::Png);
        Mock::given(method("POST"))
            .and(path("/v1/images/generations"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data": [{"b64_json": STANDARD.encode(&bytes)}]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(proxy.list_models().await.unwrap().models.len(), 2);
        assert_eq!(proxy.list_models().await.unwrap().models.len(), 2);
        let dir = tempfile::tempdir().unwrap();
        let args = args(dir.path().join("fox.png").to_str().unwrap().into());
        let result = proxy.generate(args.clone()).await.unwrap();
        assert_eq!(result["width"], 1024);
        let original_path = dir.path().join("fox.png.original.png");
        assert_eq!(result["original_path"], original_path.to_str().unwrap());
        assert_eq!(std::fs::read(&original_path).unwrap(), bytes);
        let metadata: Value = serde_json::from_slice(
            &std::fs::read(result["prompt_file"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            metadata["original"],
            json!({
                "path": original_path, "mime_type": "image/png", "width": 8, "height": 6, "bytes": bytes.len(),
            })
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3);
        assert!(proxy.generate(args).await.is_err());
        let requests = server.received_requests().await.unwrap();
        let catalog_request = requests
            .iter()
            .find(|request| request.url.path() == "/metadata")
            .unwrap();
        assert!(!catalog_request.headers.contains_key("authorization"));
        let generation = requests
            .iter()
            .find(|request| request.method == "POST")
            .unwrap();
        assert_eq!(
            generation.body_json::<Value>().unwrap()["prompt"],
            "exact fox prompt"
        );
    }

    #[tokio::test]
    async fn gemini_uses_chat_and_converts_jpeg_to_requested_png() {
        let server = MockServer::start().await;
        let proxy = setup(&server).await;
        let original = output::tests::fixture(image::ImageFormat::Jpeg);
        let bytes = STANDARD.encode(&original);
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "gemini-image-test",
                "choices": [{"message": {"images": [{"image_url": {
                    "url": format!("data:image/jpeg;base64,{bytes}")
                }}]}}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let mut request = args(dir.path().join("fox.png").to_str().unwrap().into());
        request.model = "gemini-image-test".into();
        request.width = 128;
        request.height = 64;
        let result = proxy.generate(request).await.unwrap();
        assert_eq!(result["mime_type"], "image/png");
        assert_eq!(result["width"], 128);
        assert_eq!(result["height"], 64);
        assert_eq!(result["converted"], true);
        let original_path = dir.path().join("fox.png.original.jpg");
        assert_eq!(result["original_path"], original_path.to_str().unwrap());
        assert_eq!(std::fs::read(&original_path).unwrap(), original);
        let metadata: Value = serde_json::from_slice(
            &std::fs::read(result["prompt_file"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            metadata["original"],
            json!({
                "path": original_path, "mime_type": "image/jpeg", "width": 8, "height": 6, "bytes": original.len(),
            })
        );
        assert_eq!(
            metadata["requested_dimensions"],
            json!({"width": 128, "height": 64})
        );
        assert_eq!(
            metadata["request"]["messages"][0]["content"],
            "exact fox prompt"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3);
        let requests = server.received_requests().await.unwrap();
        let body = requests
            .iter()
            .find(|request| request.method == "POST")
            .unwrap()
            .body_json::<Value>()
            .unwrap();
        assert_eq!(body["messages"][0]["content"], "exact fox prompt");
        assert_eq!(body["stream"], false);
        assert!(body.get("quality").is_none());
    }

    #[tokio::test]
    async fn metadata_outage_preserves_live_candidates() {
        let server = MockServer::start().await;
        Mock::given(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": "gpt-image-test"}]
            })))
            .mount(&server)
            .await;
        Mock::given(path("/metadata"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let mut proxy = Proxy::new(&format!("{}/v1/", server.uri()), "test-key".into()).unwrap();
        proxy.metadata_url = Url::parse(&format!("{}/metadata", server.uri())).unwrap();
        let catalog = proxy.list_models().await.unwrap();
        assert_eq!(catalog.models[0].model, "gpt-image-test");
        assert!(
            catalog
                .warnings
                .iter()
                .any(|warning| warning.contains("Metadata unavailable"))
        );
        let result = crate::ImageServer::new(proxy).guidance().await;
        assert_ne!(result.is_error, Some(true));
        let content = result.structured_content.unwrap();
        assert_eq!(content["models"][0]["model"], "gpt-image-test");
        assert_eq!(content["guidance"], crate::GUIDANCE);
    }

    #[tokio::test]
    async fn guidance_returns_only_an_error_when_discovery_fails() {
        let cases = [
            (
                ResponseTemplate::new(401)
                    .set_body_json(json!({"error": {"message": "test-key rejected"}})),
                "401",
            ),
            (
                ResponseTemplate::new(503)
                    .set_body_json(json!({"error": {"message": "Model discovery unavailable"}})),
                "503",
            ),
            (
                ResponseTemplate::new(200).set_body_string("invalid JSON"),
                "invalid JSON",
            ),
            (
                ResponseTemplate::new(200).set_body_json(json!({})),
                "Invalid /models response",
            ),
            (
                ResponseTemplate::new(200).set_body_json(json!({"data": []})),
                "No supported image-generation models",
            ),
            (
                ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "text-only"}]})),
                "No supported image-generation models",
            ),
            (
                ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "flux-test"}]})),
                "No supported image-generation models",
            ),
        ];
        for (response, expected_error) in cases {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/models"))
                .respond_with(response)
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/metadata"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "models": [{"id": "gpt-image-test", "supportedOutputModalities": ["image"]}]
                })))
                .expect(1)
                .mount(&server)
                .await;
            let mut proxy = Proxy::new(&server.uri(), "test-key".into()).unwrap();
            proxy.metadata_url = Url::parse(&format!("{}/metadata", server.uri())).unwrap();
            *proxy.cache.lock().await = Some((
                Instant::now() - Duration::from_secs(301),
                catalog::compile(json!({"data": [{"id": "gpt-image-stale"}]}), None).unwrap(),
            ));
            let result = crate::ImageServer::new(proxy).guidance().await;
            assert_eq!(result.is_error, Some(true));
            assert!(result.structured_content.is_none());
            assert_eq!(result.content.len(), 1);
            let result = serde_json::to_value(result).unwrap();
            let error = result["content"][0]["text"].as_str().unwrap();
            assert!(error.contains(expected_error), "{error}");
            assert!(!error.contains("test-key"));
            assert!(!error.contains("gpt-image-stale"));
            assert!(!error.contains("art director"));
            assert!(!error.contains("Preserve every explicit user requirement"));
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 2);
            assert!(requests.iter().all(|request| request.method == "GET"));
        }
    }

    #[tokio::test]
    async fn redirects_and_invalid_paths_do_not_send_generation() {
        let server = MockServer::start().await;
        let redirect = MockServer::start().await;
        let proxy = Proxy::new(&server.uri(), "test-key".into()).unwrap();
        Mock::given(path("/redirect"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", redirect.uri()))
            .expect(1)
            .mount(&server)
            .await;
        assert!(
            proxy
                .request_json(
                    proxy
                        .client
                        .get(format!("{}/redirect", server.uri()))
                        .header(AUTHORIZATION, proxy.auth.clone()),
                    1024,
                    false
                )
                .await
                .is_err()
        );
        assert!(redirect.received_requests().await.unwrap().is_empty());
        assert!(proxy.generate(args("relative.png".into())).await.is_err());
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
    }

    async fn rpc(
        writer: &mut (impl tokio::io::AsyncWrite + Unpin),
        reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
        request: Value,
    ) -> Value {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        writer
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], request["id"]);
        assert_eq!(response["jsonrpc"], "2.0");
        response
    }

    #[tokio::test]
    async fn mcp_protocol_discovers_guides_generates_and_reports_errors() {
        use rmcp::ServiceExt;
        use tokio::io::AsyncWriteExt;

        for version in ["2024-11-05", "2025-03-26", "2025-11-25"] {
            let server = MockServer::start().await;
            let proxy = setup(&server).await;
            Mock::given(method("POST"))
                .and(path("/v1/images/generations"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "data": [{"b64_json": STANDARD.encode(output::tests::fixture(image::ImageFormat::Png))}]
                })))
                .expect(1).mount(&server).await;
            let (client_io, server_io) = tokio::io::duplex(16384);
            let task = tokio::spawn(async move {
                crate::ImageServer::new(proxy)
                    .serve(server_io)
                    .await
                    .unwrap()
                    .waiting()
                    .await
                    .unwrap();
            });
            let (reader, mut writer) = tokio::io::split(client_io);
            let mut reader = tokio::io::BufReader::new(reader);
            let response = rpc(&mut writer, &mut reader, json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": version, "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}
            })).await;
            assert_eq!(response["result"]["protocolVersion"], version);
            assert!(response["result"]["capabilities"]["tools"].is_object());
            writer
                .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
                .await
                .unwrap();
            let response = rpc(
                &mut writer,
                &mut reader,
                json!({
                    "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
                }),
            )
            .await;
            let tools = response["result"]["tools"].as_array().unwrap();
            let mut names: Vec<_> = tools
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect();
            names.sort_unstable();
            assert_eq!(names, ["generate", "guidance", "modify"]);
            for id in [3, 4] {
                let response = rpc(&mut writer, &mut reader, json!({
                    "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "guidance", "arguments": {}}
                })).await;
                assert_eq!(
                    response["result"]["structuredContent"]["models"]
                        .as_array()
                        .unwrap()
                        .len(),
                    2
                );
                assert_eq!(
                    response["result"]["structuredContent"]["guidance"],
                    crate::GUIDANCE
                );
                assert!(response["result"]["structuredContent"]["warnings"].is_array());
                let text: Value = serde_json::from_str(
                    response["result"]["content"][0]["text"].as_str().unwrap(),
                )
                .unwrap();
                assert_eq!(text, response["result"]["structuredContent"]);
            }
            let dir = tempfile::tempdir().unwrap();
            let mut args = args(dir.path().join("fox.png").to_str().unwrap().into());
            args.width = 64;
            args.height = 64;
            let mut request = json!({
                "jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {"name": "generate", "arguments": args}
            });
            let response = rpc(&mut writer, &mut reader, request.clone()).await;
            assert_eq!(response["result"]["structuredContent"]["width"], 64);
            assert_eq!(
                response["result"]["structuredContent"]["path"],
                args.local_path
            );
            let original_path = dir.path().join("fox.png.original.png");
            assert_eq!(
                response["result"]["structuredContent"]["original_path"],
                original_path.to_str().unwrap()
            );
            assert_eq!(
                std::fs::read(original_path).unwrap(),
                output::tests::fixture(image::ImageFormat::Png)
            );
            request["id"] = json!(7);
            let response = rpc(&mut writer, &mut reader, request.clone()).await;
            assert_eq!(response["result"]["isError"], true);
            assert!(
                response["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("already exists")
            );
            request["id"] = json!(8);
            request["params"]["arguments"]["extra"] = json!(true);
            let response = rpc(&mut writer, &mut reader, request).await;
            assert!(response["error"].is_object() || response["result"]["isError"] == true);
            drop(writer);
            drop(reader);
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3);
        }
    }

    #[tokio::test]
    async fn existing_original_prevents_any_api_requests() {
        let server = MockServer::start().await;
        let proxy = Proxy::new(&server.uri(), "test-key".into()).unwrap();
        for extension in ["png", "jpg", "webp"] {
            for symlink in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let original_path = dir.path().join(format!("fox.png.original.{extension}"));
                if symlink {
                    std::os::unix::fs::symlink(dir.path().join("missing"), &original_path).unwrap();
                } else {
                    std::fs::write(&original_path, b"keep").unwrap();
                }
                let error = proxy
                    .generate(args(dir.path().join("fox.png").to_str().unwrap().into()))
                    .await
                    .unwrap_err();
                assert!(error.to_string().contains("already exists"));
                if symlink {
                    assert_eq!(
                        std::fs::read_link(&original_path).unwrap(),
                        dir.path().join("missing")
                    );
                } else {
                    assert_eq!(std::fs::read(&original_path).unwrap(), b"keep");
                }
                assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
            }
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn oversized_response_is_rejected_without_retry() {
        let server = MockServer::start().await;
        let proxy = Proxy::new(&server.uri(), "test-key".into()).unwrap();
        Mock::given(path("/large"))
            .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(1024)))
            .expect(1)
            .mount(&server)
            .await;
        let error = proxy
            .request_json(
                proxy.client.get(format!("{}/large", server.uri())),
                512,
                true,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("size limit"));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn redacts_errors_and_never_retries_generation() {
        let server = MockServer::start().await;
        let proxy = setup(&server).await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_body_json(json!({"error": {"message": "test-key quota exceeded"}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let error = proxy
            .generate(args(dir.path().join("fox.png").to_str().unwrap().into()))
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("test-key"));
        assert!(error.to_string().contains("429"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
