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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub enum AspectRatio {
    #[serde(rename = "1:1")]
    Square,
    #[serde(rename = "2:3")]
    Portrait,
    #[serde(rename = "3:2")]
    Landscape,
    #[serde(rename = "3:4")]
    ThreeFour,
    #[serde(rename = "4:3")]
    FourThree,
    #[serde(rename = "4:5")]
    FourFive,
    #[serde(rename = "5:4")]
    FiveFour,
    #[serde(rename = "9:16")]
    NineSixteen,
    #[serde(rename = "16:9")]
    SixteenNine,
    #[serde(rename = "21:9")]
    TwentyOneNine,
    #[serde(rename = "1:4")]
    OneFour,
    #[serde(rename = "4:1")]
    FourOne,
    #[serde(rename = "1:8")]
    OneEight,
    #[serde(rename = "8:1")]
    EightOne,
}

impl AspectRatio {
    fn is_extended(self) -> bool {
        matches!(
            self,
            Self::OneFour | Self::FourOne | Self::OneEight | Self::EightOne
        )
    }
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Generate {
    #[schemars(
        description = "Exact model slug returned by the guidance tool, without an llm-proxy/ prefix."
    )]
    pub model: String,
    #[schemars(
        description = "Absolute destination ending in .png, .jpg, .jpeg, or .webp. The parent must exist. The saved extension follows the provider format. Existing files are never overwritten."
    )]
    pub local_path: String,
    #[schemars(
        description = "Provider-native size. GPT: WIDTHxHEIGHT (for example 1600x1024) or auto; omission means auto. Gemini: supported tier 512, 1K, 2K, or 4K; omit for fixed-resolution models or provider defaults. See guidance for model limits. Passed unchanged, without resizing."
    )]
    pub size: Option<String>,
    #[schemars(
        description = "Gemini-only native aspect ratio. Omit for provider defaults. Do not supply for GPT models: their size specifies the ratio. See guidance for model support."
    )]
    pub aspect_ratio: Option<AspectRatio>,
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
        description = "Absolute destination ending in .png, .jpg, .jpeg, or .webp. The parent must exist. The saved extension follows the provider format. Existing files are never overwritten."
    )]
    pub local_path: String,
    #[schemars(
        description = "Exact editing instructions: describe the changes and what must remain unchanged. First get a successful response from the guidance tool."
    )]
    pub prompt: String,
    #[schemars(
        description = "OpenAI-native size: WIDTHxHEIGHT (for example 1600x1024) or auto. Omission means auto. See guidance for model limits. Passed unchanged; automatic sizing does not guarantee source dimensions."
    )]
    pub size: Option<String>,
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
        validate_prompt_model(&args.prompt, &args.model)?;
        let destination = Destination::prepare(Path::new(&args.local_path))?;
        let catalog = self.list_models().await?;
        let model = catalog.models.iter().find(|item| item.model == args.model || item.account_ids.contains(&args.model))
            .context("Model is not in the live image catalog. Call the guidance tool and use an exact returned slug.")?;
        let route = model
            .route
            .context("This image model family has no supported CLIProxyAPI route")?;
        let request = generation_request(&args, route)?;
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
        let metadata = json!({"request": request, "response_model": response.get("model"),
            "requested_size": args.size, "requested_aspect_ratio": args.aspect_ratio});
        tokio::task::spawn_blocking(move || destination.save(decoded, metadata))
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
        modification_size(&args)?;
        let destination = Destination::prepare(Path::new(&args.local_path))?;
        let model_slug = args.model.clone();
        let (mut request, mut metadata) =
            tokio::task::spawn_blocking(move || prepare_modification(args))
                .await
                .context("Image input worker failed")??;
        let catalog = self.list_models().await?;
        let model = catalog.models.iter().find(|item| item.model == model_slug || item.account_ids.contains(&model_slug))
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
        tokio::task::spawn_blocking(move || destination.save(decoded, metadata))
            .await
            .context("Image save worker failed")?
    }
}

fn modification_size(args: &Modify) -> Result<&str> {
    openai_size(&args.model, args.size.as_deref())
}

fn prepare_modification(args: Modify) -> Result<(Value, Value)> {
    let size = modification_size(&args)?;
    let input = InputImage::load(Path::new(&args.input_path), None)?;
    let mask = args
        .mask_path
        .as_ref()
        .map(|path| InputImage::load(Path::new(path), Some(&input)))
        .transpose()?;
    let mut request = openai_request(&args.model, &args.prompt, size);
    request["images"] = json!([{"image_url": input.data_url()}]);
    let model = args.model.rsplit('/').next().unwrap_or(&args.model);
    if model == "gpt-image-1" || model == "gpt-image-1.5" || model.starts_with("gpt-image-1.5-") {
        request["input_fidelity"] = json!("high");
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
        "requested_size": args.size,
    });
    Ok((request, metadata))
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

fn model_family(model: &str, family: &str) -> bool {
    model == family
        || model
            .strip_prefix(family)
            .is_some_and(|suffix| suffix.starts_with('-'))
}

fn openai_size<'a>(model: &str, size: Option<&'a str>) -> Result<&'a str> {
    let size = size.unwrap_or("auto");
    if size == "auto" {
        return Ok(size);
    }
    let (width, height) = size
        .split_once('x')
        .context("GPT size must be auto or WIDTHxHEIGHT, not a resolution tier")?;
    ensure!(
        !width.is_empty()
            && !height.is_empty()
            && width.bytes().all(|c| c.is_ascii_digit())
            && height.bytes().all(|c| c.is_ascii_digit()),
        "GPT size must be auto or WIDTHxHEIGHT with positive integer dimensions"
    );
    let width: u32 = width.parse().context("Invalid GPT size width")?;
    let height: u32 = height.parse().context("Invalid GPT size height")?;
    ensure!(
        width > 0 && height > 0,
        "GPT size dimensions must be positive"
    );
    let canonical = model.rsplit('/').next().unwrap_or(model);
    if model_family(canonical, "gpt-image-1") || model_family(canonical, "gpt-image-1.5") {
        ensure!(
            matches!(size, "1024x1024" | "1536x1024" | "1024x1536"),
            "This size is not supported by {model}; use auto, 1024x1024, 1536x1024, or 1024x1536"
        );
    } else if model_family(canonical, "gpt-image-2") || model_family(canonical, "gpt-image-2.5") {
        ensure!(
            width <= 3840
                && height <= 3840
                && width.is_multiple_of(16)
                && height.is_multiple_of(16),
            "GPT size dimensions for {model} must be multiples of 16 and no larger than 3840 pixels"
        );
        ensure!(
            width <= height * 3 && height <= width * 3,
            "GPT size aspect ratio for {model} must be between 1:3 and 3:1"
        );
        ensure!(
            (655_360..=8_294_400).contains(&(width * height)),
            "GPT size for {model} must contain between 655360 and 8294400 pixels"
        );
    }
    Ok(size)
}

fn openai_request(model: &str, prompt: &str, size: &str) -> Value {
    json!({"model": model, "prompt": prompt, "n": 1, "size": size,
        "quality": "auto", "output_format": "png"})
}

pub fn generation_request(args: &Generate, route: Route) -> Result<Value> {
    Ok(match route {
        Route::Openai => {
            ensure!(
                args.aspect_ratio.is_none(),
                "aspect_ratio is Gemini-only; specify GPT dimensions in size"
            );
            let size = openai_size(&args.model, args.size.as_deref())?;
            let mut request = openai_request(&args.model, &args.prompt, size);
            request["response_format"] = json!("b64_json");
            request
        }
        Route::Gemini => {
            let canonical = args.model.rsplit('/').next().unwrap_or(&args.model);
            let fixed_image = model_family(canonical, "gemini-2.5-flash-image");
            let pro_image = model_family(canonical, "gemini-3-pro-image");
            let mut config = json!({});
            if let Some(ratio) = args.aspect_ratio {
                ensure!(
                    !(fixed_image || pro_image) || !ratio.is_extended(),
                    "This aspect ratio is not supported by {}",
                    args.model
                );
                config["aspect_ratio"] = json!(ratio);
            }
            if let Some(size) = args.size.as_deref() {
                ensure!(
                    !fixed_image,
                    "{} has a fixed resolution; omit size",
                    args.model
                );
                ensure!(
                    matches!(size, "512" | "1K" | "2K" | "4K"),
                    "Gemini size must be a native tier: 512, 1K, 2K, or 4K"
                );
                ensure!(
                    !pro_image || size != "512",
                    "The size 512 is not supported by {}",
                    args.model
                );
                config["image_size"] = json!(size);
            }
            let mut request = json!({"model": args.model, "messages": [{"role": "user", "content": args.prompt}],
                "modalities": ["text", "image"], "stream": false});
            if !config.as_object().unwrap().is_empty() {
                request["image_config"] = config;
            }
            request
        }
    })
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
            size: None,
            aspect_ratio: None,
            prompt: "exact fox prompt".into(),
        }
    }

    fn edit_args(input_path: &Path, local_path: &Path) -> Modify {
        Modify {
            model: "gpt-image-test".into(),
            input_path: input_path.to_str().unwrap().into(),
            local_path: local_path.to_str().unwrap().into(),
            prompt: "Change only the coat to red.\nKeep the face and background unchanged.".into(),
            size: None,
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
            let (request, metadata) = prepare_modification(args.clone()).unwrap();
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
    fn modification_respects_requested_size_and_keeps_masks_unmodified() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.png");
        edit_source(&input, 128, 96);
        let mask = directory.path().join("mask.png");
        image::RgbaImage::new(128, 96).save(&mask).unwrap();
        let mask_bytes = std::fs::read(&mask).unwrap();
        let mut args = edit_args(&input, &directory.path().join("output.png"));
        args.mask_path = Some(mask.to_str().unwrap().into());
        args.size = Some("1600x1024".into());
        let (request, metadata) = prepare_modification(args.clone()).unwrap();
        assert_eq!(request["size"], "1600x1024");
        assert_eq!(
            request["mask"]["image_url"],
            format!("data:image/png;base64,{}", STANDARD.encode(&mask_bytes))
        );
        assert_eq!(metadata["requested_size"], "1600x1024");
        assert_eq!(metadata["mask"]["width"], 128);
        assert_eq!(metadata["mask"]["height"], 96);
        assert_eq!(std::fs::read(&mask).unwrap(), mask_bytes);
        args.size = Some("0x1024".into());
        assert!(prepare_modification(args).is_err());
    }

    #[tokio::test]
    async fn modification_uploads_images_and_saves_native_bytes_without_embedding_them_in_metadata()
    {
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
        let saved_path = directory.path().join("output.jpg");
        assert_eq!(result["path"], saved_path.to_str().unwrap());
        assert_eq!(result["width"], 8);
        assert_eq!(result["height"], 6);
        assert_eq!(std::fs::read(&saved_path).unwrap(), response);
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
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 3);
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
            json!({"size": "1K"}),
            json!({"size": "0x1024"}),
            json!({"size": "1600x0"}),
            json!({"size": "1600"}),
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
        for suffix in ["", ".prompt.json"] {
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
            generation_request(&args, Route::Openai).unwrap()["prompt"],
            args.prompt
        );
        args.model = "gemini-3.1-flash-image".into();
        args.size = Some("2K".into());
        args.aspect_ratio = Some(AspectRatio::SixteenNine);
        let gemini = generation_request(&args, Route::Gemini).unwrap();
        assert_eq!(gemini["messages"][0]["content"], args.prompt);
        assert_eq!(gemini["image_config"]["aspect_ratio"], "16:9");
        assert_eq!(gemini["image_config"]["image_size"], "2K");
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
    fn gemini_known_fixed_models_omit_size_and_accept_base_ten_ratios() {
        let mut args = args("/tmp/test.png".into());
        args.model = "gemini-2.5-flash-image".into();
        args.aspect_ratio = Some(AspectRatio::TwentyOneNine);
        let config = generation_request(&args, Route::Gemini).unwrap()["image_config"].clone();
        assert_eq!(config["aspect_ratio"], "21:9");
        assert!(config.get("image_size").is_none());
        args.size = Some("1K".into());
        assert_eq!(
            generation_request(&args, Route::Gemini)
                .unwrap_err()
                .to_string(),
            "gemini-2.5-flash-image has a fixed resolution; omit size"
        );
        args.size = None;
        args.aspect_ratio = Some(AspectRatio::EightOne);
        assert!(generation_request(&args, Route::Gemini).is_err());
    }

    #[test]
    fn gemini_pro_supports_tier_sizes_but_not_extended_ratios() {
        let mut args = args("/tmp/test.png".into());
        args.model = "account/gemini-3-pro-image-preview".into();
        args.aspect_ratio = Some(AspectRatio::TwentyOneNine);
        args.size = Some("4K".into());
        let config = generation_request(&args, Route::Gemini).unwrap()["image_config"].clone();
        assert_eq!(config["aspect_ratio"], "21:9");
        assert_eq!(config["image_size"], "4K");
        args.size = Some("512".into());
        assert!(generation_request(&args, Route::Gemini).is_err());
        args.size = Some("1K".into());
        args.aspect_ratio = Some(AspectRatio::OneEight);
        assert!(generation_request(&args, Route::Gemini).is_err());
    }

    #[test]
    fn gemini_3_1_supports_all_tiers_and_extended_ratios() {
        let mut args = args("/tmp/test.png".into());
        args.model = "account/gemini-3.1-flash-image".into();
        args.aspect_ratio = Some(AspectRatio::EightOne);
        args.size = Some("512".into());
        let config = generation_request(&args, Route::Gemini).unwrap()["image_config"].clone();
        assert_eq!(config["aspect_ratio"], "8:1");
        assert_eq!(config["image_size"], "512");
        args.size = Some("4K".into());
        let config = generation_request(&args, Route::Gemini).unwrap()["image_config"].clone();
        assert_eq!(config["image_size"], "4K");
    }

    #[test]
    fn gemini_unknown_models_permit_any_tier_and_ratio_without_assumptions() {
        let mut args = args("/tmp/test.png".into());
        args.model = "gemini-image-test".into();
        for size in ["512", "1K", "2K", "4K"] {
            args.size = Some(size.into());
            for ratio in [
                AspectRatio::Square,
                AspectRatio::OneEight,
                AspectRatio::EightOne,
            ] {
                args.aspect_ratio = Some(ratio);
                assert!(generation_request(&args, Route::Gemini).is_ok());
            }
        }
        args.size = Some("8K".into());
        assert!(generation_request(&args, Route::Gemini).is_err());
    }

    #[test]
    fn gemini_fields_are_independent_and_absent_config_is_omitted() {
        let mut args = args("/tmp/test.png".into());
        args.model = "account/gemini-3.1-flash-image".into();
        args.aspect_ratio = None;
        args.size = Some("2K".into());
        let request = generation_request(&args, Route::Gemini).unwrap();
        assert!(request["image_config"].get("aspect_ratio").is_none());
        assert_eq!(request["image_config"]["image_size"], "2K");
        args.size = None;
        args.aspect_ratio = Some(AspectRatio::Square);
        let request = generation_request(&args, Route::Gemini).unwrap();
        assert!(request["image_config"].get("image_size").is_none());
        assert_eq!(request["image_config"]["aspect_ratio"], "1:1");
        args.aspect_ratio = None;
        let request = generation_request(&args, Route::Gemini).unwrap();
        assert!(request.get("image_config").is_none());
    }

    #[test]
    fn openai_size_missing_means_auto_and_native_sizes_pass_through_unchanged() {
        assert_eq!(openai_size("gpt-image-test", None).unwrap(), "auto");
        assert_eq!(
            openai_size("gpt-image-test", Some("1600x1024")).unwrap(),
            "1600x1024"
        );
    }

    #[test]
    fn openai_size_known_legacy_family_restricts_to_three_fixed_strings() {
        for model in [
            "gpt-image-1",
            "gpt-image-1.5",
            "account/gpt-image-1.5-2025-12-16",
            "gpt-image-1-mini",
        ] {
            for size in ["1024x1024", "1536x1024", "1024x1536"] {
                assert_eq!(openai_size(model, Some(size)).unwrap(), size);
            }
            assert!(openai_size(model, Some("1600x1024")).is_err());
            assert!(openai_size(model, Some("2048x2048")).is_err());
        }
    }

    #[test]
    fn openai_size_known_gpt2_family_enforces_documented_boundaries() {
        for model in [
            "gpt-image-2",
            "account/gpt-image-2-2026-04-21",
            "gpt-image-2.5-flare",
        ] {
            for size in [
                "1600x1024",
                "1008x1008",
                "1920x640",
                "640x1920",
                "640x1024",
                "1024x640",
                "3840x2160",
                "2160x3840",
            ] {
                assert_eq!(
                    openai_size(model, Some(size)).unwrap(),
                    size,
                    "{model} {size}"
                );
            }
            for size in [
                "0x1024",
                "1024x0",
                "1600x1025",
                "624x1024",
                "3856x3856",
                "1920x608",
            ] {
                assert!(openai_size(model, Some(size)).is_err(), "{model} {size}");
            }
        }
    }

    #[test]
    fn openai_size_unknown_models_validate_syntax_only() {
        for size in ["auto", "9999x1", "1x9999"] {
            assert_eq!(openai_size("gpt-image-test", Some(size)).unwrap(), size);
        }
        for size in ["0x5", "5x0", "abc", "1600", "1600x", "x1024", "1600xAB"] {
            assert!(openai_size("gpt-image-test", Some(size)).is_err(), "{size}");
        }
    }

    #[test]
    fn automatic_modification_does_not_constrain_source_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("source.png");
        for (width, height) in [(32, 24), (4097, 64), (64, 1024)] {
            edit_source(&input, width, height);
            let args = edit_args(&input, &dir.path().join("edited.png"));
            let (request, metadata) = prepare_modification(args).unwrap();
            assert_eq!(request["size"], "auto");
            assert_eq!(metadata["input"]["width"], width);
            assert_eq!(metadata["input"]["height"], height);
            assert!(metadata["requested_size"].is_null());
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
        assert_eq!(result["path"], dir.path().join("fox.png").to_str().unwrap());
        assert_eq!(result["width"], 8);
        assert_eq!(result["height"], 6);
        assert_eq!(std::fs::read(dir.path().join("fox.png")).unwrap(), bytes);
        let metadata: Value = serde_json::from_slice(
            &std::fs::read(result["prompt_file"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert!(metadata["requested_size"].is_null());
        assert!(metadata["requested_aspect_ratio"].is_null());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
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
    async fn gemini_uses_chat_and_saves_native_bytes_with_corrected_extension() {
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
        request.aspect_ratio = Some(AspectRatio::SixteenNine);
        let result = proxy.generate(request).await.unwrap();
        assert_eq!(result["mime_type"], "image/jpeg");
        assert_eq!(result["width"], 8);
        assert_eq!(result["height"], 6);
        let saved_path = dir.path().join("fox.jpg");
        assert_eq!(result["path"], saved_path.to_str().unwrap());
        assert_eq!(std::fs::read(&saved_path).unwrap(), original);
        let metadata: Value = serde_json::from_slice(
            &std::fs::read(result["prompt_file"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert!(metadata["requested_size"].is_null());
        assert_eq!(metadata["requested_aspect_ratio"], "16:9");
        assert_eq!(
            metadata["request"]["messages"][0]["content"],
            "exact fox prompt"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
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
            let args = args(dir.path().join("fox.png").to_str().unwrap().into());
            let mut request = json!({
                "jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {"name": "generate", "arguments": args}
            });
            let response = rpc(&mut writer, &mut reader, request.clone()).await;
            assert_eq!(response["result"]["structuredContent"]["width"], 8);
            assert_eq!(
                response["result"]["structuredContent"]["path"],
                args.local_path
            );
            assert_eq!(
                std::fs::read(dir.path().join("fox.png")).unwrap(),
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
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        }
    }

    #[tokio::test]
    async fn existing_native_extension_variants_prevent_any_api_requests() {
        let server = MockServer::start().await;
        let proxy = Proxy::new(&server.uri(), "test-key".into()).unwrap();
        for extension in ["png", "jpg", "jpeg", "webp"] {
            for conflict in ["", ".prompt.json"] {
                for symlink in [false, true] {
                    let dir = tempfile::tempdir().unwrap();
                    let existing = dir.path().join(format!("fox.{extension}{conflict}"));
                    if symlink {
                        std::os::unix::fs::symlink(dir.path().join("missing"), &existing).unwrap();
                    } else {
                        std::fs::write(&existing, b"keep").unwrap();
                    }
                    let error = proxy
                        .generate(args(dir.path().join("fox.png").to_str().unwrap().into()))
                        .await
                        .unwrap_err();
                    assert!(error.to_string().contains("already exists"));
                    if symlink {
                        assert_eq!(
                            std::fs::read_link(&existing).unwrap(),
                            dir.path().join("missing")
                        );
                    } else {
                        assert_eq!(std::fs::read(&existing).unwrap(), b"keep");
                    }
                    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
                }
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
