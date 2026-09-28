use std::{
    collections::BTreeSet,
    fs,
    io::{self, BufRead, BufReader, Cursor, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use image::ImageFormat;
use serde_json::{Value, json};
use wiremock::{
    Mock, MockGuard, MockServer, Request, ResponseTemplate,
    matchers::{body_json, body_partial_json, header, method, path},
};

const PROMPT: &str = "Exact smoke prompt: \"fox\".\nKeep this text unchanged.";
const EDIT_PROMPT: &str =
    "Change only the coat to red.\nKeep the face, background, and ‘FOX’ label unchanged.";
const MODELS: [(&str, ImageFormat, &str); 3] = [
    ("gpt-image-smoke", ImageFormat::Png, "png"),
    ("gemini-image-smoke", ImageFormat::Jpeg, "jpg"),
    ("gemini-image-webp", ImageFormat::WebP, "webp"),
];
const BASE_RATIOS: [&str; 10] = [
    "1:1", "2:3", "3:2", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9",
];
const EXTRA_RATIOS: [&str; 4] = ["1:4", "4:1", "1:8", "8:1"];
const ALL_RATIOS: [&str; 14] = [
    "1:1", "2:3", "3:2", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9", "1:4", "4:1", "1:8",
    "8:1",
];
const GEMINI_SIZES: [&str; 4] = ["512", "1K", "2K", "4K"];

struct StdioClient {
    child: Child,
    input: Option<ChildStdin>,
    output: mpsc::Receiver<io::Result<String>>,
    reader: Option<JoinHandle<()>>,
    next_id: u64,
}

impl StdioClient {
    fn start(server: &MockServer, metadata: &MockServer, cwd: &Path, version: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_image-generation-mcp"))
            .args([
                "--base-url",
                &server.uri(),
                "--api-key-env",
                "SMOKE_API_KEY",
            ])
            .env_clear()
            .env("HOME", cwd)
            .env("TMPDIR", cwd)
            .env("SMOKE_API_KEY", "local-test-key")
            .env("HTTP_PROXY", metadata.uri())
            .env("HTTPS_PROXY", metadata.uri())
            .env("ALL_PROXY", metadata.uri())
            .env("NO_PROXY", "127.0.0.1,localhost")
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, output) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut client = Self {
            input: child.stdin.take(),
            child,
            output,
            reader: Some(reader),
            next_id: 1,
        };
        let response = client.rpc(
            "initialize",
            json!({
                "protocolVersion": version, "capabilities": {},
                "clientInfo": {"name": "stdio-smoke", "version": "1"},
            }),
        );
        assert_eq!(response["result"]["protocolVersion"], version);
        assert!(response["result"]["capabilities"]["tools"].is_object());
        client.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        client
    }

    fn send(&mut self, message: Value) {
        let input = self.input.as_mut().unwrap();
        writeln!(input, "{message}").unwrap();
        input.flush().unwrap();
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let line = self
            .output
            .recv_timeout(Duration::from_secs(20))
            .expect("MCP response timed out or stdout closed")
            .unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], id, "{response}");
        response
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let response = self.rpc("tools/call", json!({"name": name, "arguments": arguments}));
        assert!(response.get("error").is_none(), "{response}");
        response["result"].clone()
    }

    fn generate(
        &mut self,
        path: &Path,
        model: &str,
        size: Option<&str>,
        aspect_ratio: Option<&str>,
    ) -> Value {
        let mut arguments = json!({"model": model, "local_path": path, "prompt": PROMPT});
        let object = arguments.as_object_mut().unwrap();
        if let Some(size) = size {
            object.insert("size".into(), json!(size));
        }
        if let Some(ratio) = aspect_ratio {
            object.insert("aspect_ratio".into(), json!(ratio));
        }
        self.tool("generate", arguments)
    }

    fn modify(&mut self, input: &Path, output: &Path, options: Value) -> Value {
        let mut arguments = json!({
            "model": "gpt-image-smoke", "input_path": input,
            "local_path": output, "prompt": EDIT_PROMPT,
        });
        arguments
            .as_object_mut()
            .unwrap()
            .extend(options.as_object().unwrap().clone());
        self.tool("modify", arguments)
    }

    fn finish(mut self) {
        drop(self.input.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "MCP exited with {status}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "MCP did not exit after stdin closed"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        drop(self.input.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

async fn metadata_proxy(expected_requests: u64) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("CONNECT"))
        .respond_with(ResponseTemplate::new(502))
        .expect(expected_requests)
        .mount(&server)
        .await;
    server
}

async fn mount_models(server: &MockServer, models: &[&str]) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer local-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": models.iter().map(|model| json!({"id": model})).collect::<Vec<_>>(),
        })))
        .expect(1)
        .mount(server)
        .await;
}

async fn mount_catalog(server: &MockServer) {
    let ids: Vec<&str> = MODELS.iter().map(|(model, _, _)| *model).collect();
    mount_models(server, &ids).await;
}

fn structured(result: &Value) -> &Value {
    assert_ne!(result["isError"], true, "{result}");
    let data = result.get("structuredContent").unwrap();
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(&text, data);
    data
}

fn assert_guidance(result: &Value) {
    let data = structured(result);
    assert_eq!(data["guidance"], include_str!("../guidance.md"));
    assert!(
        data["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| { warning.as_str().unwrap().contains("Metadata unavailable") })
    );
    assert!(
        data["models"]
            .as_array()
            .unwrap()
            .iter()
            .all(|model| model["route"].is_string())
    );
    assert_eq!(data.as_object().unwrap().len(), 3);
}

fn assert_error(result: &Value, expected: &str) {
    assert_eq!(result["isError"], true, "{result}");
    assert!(result.get("structuredContent").is_none());
    assert_eq!(result["content"].as_array().unwrap().len(), 1);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains(expected), "{text}");
    assert!(!text.contains("local-test-key"));
}

fn assert_schema_rejected(response: &Value) {
    assert!(
        response["error"].is_object() || response["result"]["isError"] == true,
        "{response}"
    );
}

fn entries(directory: &Path) -> BTreeSet<PathBuf> {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect()
}

fn fixture(format: ImageFormat) -> Vec<u8> {
    let image = image::RgbImage::from_fn(128, 96, |x, y| {
        image::Rgb([x as u8, y as u8, (x + y) as u8])
    });
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, format).unwrap();
    bytes.into_inner()
}

fn expected_extension(format: ImageFormat, requested: &str) -> &'static str {
    match format {
        ImageFormat::Png => "png",
        ImageFormat::WebP => "webp",
        ImageFormat::Jpeg if requested == "jpeg" => "jpeg",
        ImageFormat::Jpeg => "jpg",
        _ => unreachable!(),
    }
}

fn is_gpt_model(model: &str) -> bool {
    model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .starts_with("gpt-")
}

fn openai_generate_request(model: &str, size: &str) -> Value {
    json!({"model": model, "prompt": PROMPT, "n": 1, "size": size,
        "quality": "auto", "output_format": "png", "response_format": "b64_json"})
}

fn gemini_config(size: Option<&str>, ratio: Option<&str>) -> Option<Value> {
    if size.is_none() && ratio.is_none() {
        return None;
    }
    let mut config = json!({});
    let object = config.as_object_mut().unwrap();
    if let Some(ratio) = ratio {
        object.insert("aspect_ratio".into(), json!(ratio));
    }
    if let Some(size) = size {
        object.insert("image_size".into(), json!(size));
    }
    Some(config)
}

fn gemini_generate_request(model: &str, config: Option<Value>) -> Value {
    let mut request = json!({"model": model, "messages": [{"role": "user", "content": PROMPT}],
        "modalities": ["text", "image"], "stream": false});
    if let Some(config) = config {
        request["image_config"] = config;
    }
    request
}

fn generation_response(model: &str, bytes: &[u8]) -> ResponseTemplate {
    let encoded = STANDARD.encode(bytes);
    let response = if is_gpt_model(model) {
        json!({"data": [{"b64_json": encoded}]})
    } else {
        json!({"choices": [{"message": {"images": [{"image_url": {
            "url": format!("data:image/png;base64,{encoded}"),
        }}]}}]})
    };
    ResponseTemplate::new(200).set_body_json(response)
}

fn generation_mock(model: &str) -> wiremock::MockBuilder {
    let (endpoint, body) = if is_gpt_model(model) {
        (
            "/v1/images/generations",
            json!({"model": model, "prompt": PROMPT, "n": 1}),
        )
    } else {
        (
            "/v1/chat/completions",
            json!({
                "model": model, "messages": [{"role": "user", "content": PROMPT}], "stream": false,
            }),
        )
    };
    Mock::given(method("POST"))
        .and(path(endpoint))
        .and(header("authorization", "Bearer local-test-key"))
        .and(body_partial_json(body))
}

async fn expect_generation(
    server: &MockServer,
    model: &str,
    body: Value,
    bytes: &[u8],
) -> MockGuard {
    generation_mock(model)
        .and(body_json(body))
        .respond_with(generation_response(model, bytes))
        .expect(1)
        .mount_as_scoped(server)
        .await
}

async fn expect_generate_success(
    server: &MockServer,
    client: &mut StdioClient,
    dir: &Path,
    model: &str,
    size: Option<&str>,
    ratio: Option<&str>,
    expected_body: Value,
) {
    let bytes = fixture(ImageFormat::Png);
    let mock = expect_generation(server, model, expected_body, &bytes).await;
    let output_dir = tempfile::tempdir_in(dir).unwrap();
    let result = client.generate(&output_dir.path().join("asset.png"), model, size, ratio);
    tokio::task::spawn_blocking(move || drop(mock))
        .await
        .unwrap();
    structured(&result);
}

fn expect_generate_error(
    client: &mut StdioClient,
    dir: &Path,
    model: &str,
    size: Option<&str>,
    ratio: Option<&str>,
    expected: &str,
) {
    let output_dir = tempfile::tempdir_in(dir).unwrap();
    let output = output_dir.path().join("asset.png");
    let result = client.generate(&output, model, size, ratio);
    assert_error(&result, expected);
    assert!(entries(output_dir.path()).is_empty());
}

fn modification_mock() -> wiremock::MockBuilder {
    Mock::given(method("POST"))
        .and(path("/v1/images/edits"))
        .and(header("authorization", "Bearer local-test-key"))
        .and(header("content-type", "application/json"))
        .and(body_partial_json(json!({"prompt": EDIT_PROMPT, "n": 1})))
}

#[derive(Clone, Copy, Debug)]
enum Conflict {
    File,
    Symlink,
    Directory,
}

impl Conflict {
    fn create(self, path: &Path) {
        match self {
            Self::File => fs::write(path, b"keep").unwrap(),
            Self::Symlink => {
                std::os::unix::fs::symlink(path.with_extension("missing"), path).unwrap()
            }
            Self::Directory => fs::create_dir(path).unwrap(),
        }
    }

    fn assert_preserved(self, path: &Path) {
        match self {
            Self::File => assert_eq!(fs::read(path).unwrap(), b"keep"),
            Self::Symlink => {
                assert_eq!(fs::read_link(path).unwrap(), path.with_extension("missing"))
            }
            Self::Directory => assert!(entries(path).is_empty()),
        }
        assert_eq!(
            entries(path.parent().unwrap()),
            BTreeSet::from([path.to_path_buf()])
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn negotiates_protocols_and_loads_embedded_guidance_outside_the_source_tree() {
    for version in ["2024-11-05", "2025-03-26", "2025-11-25"] {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        let metadata = metadata_proxy(1).await;
        let directory = tempfile::tempdir().unwrap();
        let mut client = StdioClient::start(&server, &metadata, directory.path(), version);
        let response = client.rpc("tools/list", json!({}));
        let tools = response["result"]["tools"].as_array().unwrap();
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["generate", "guidance", "modify"])
        );
        let generation = tools
            .iter()
            .find(|tool| tool["name"] == "generate")
            .unwrap();
        let properties = generation["inputSchema"]["properties"].as_object().unwrap();
        assert_eq!(properties.len(), 5);
        assert_eq!(properties["size"]["type"], json!(["string", "null"]));
        assert!(properties["size"].get("enum").is_none());
        let ratio_ref = properties["aspect_ratio"]["anyOf"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|schema| schema.get("$ref").and_then(Value::as_str))
            .unwrap();
        let ratios = generation["inputSchema"]
            .pointer(ratio_ref.strip_prefix('#').unwrap())
            .unwrap()["enum"]
            .as_array()
            .unwrap();
        assert_eq!(
            ratios
                .iter()
                .map(|ratio| ratio.as_str().unwrap())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(ALL_RATIOS)
        );
        assert!(!properties.contains_key("width"));
        assert!(!properties.contains_key("height"));
        assert_eq!(
            generation["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|field| field.as_str().unwrap())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["model", "local_path", "prompt"])
        );
        let modification = tools.iter().find(|tool| tool["name"] == "modify").unwrap();
        let schema = &modification["inputSchema"];
        let properties = schema["properties"].as_object().unwrap();
        assert_eq!(
            properties
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "model",
                "input_path",
                "local_path",
                "prompt",
                "size",
                "mask_path"
            ]),
        );
        assert_eq!(properties["size"]["type"], json!(["string", "null"]));
        assert!(properties["size"].get("enum").is_none());
        assert!(!properties.contains_key("aspect_ratio"));
        assert!(!properties.contains_key("width"));
        assert!(!properties.contains_key("height"));
        assert_eq!(
            schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|field| field.as_str().unwrap())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["model", "input_path", "local_path", "prompt"]),
        );
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(modification["annotations"]["readOnlyHint"], false);
        assert_eq!(modification["annotations"]["destructiveHint"], false);
        assert_eq!(modification["annotations"]["idempotentHint"], false);
        for _ in 0..2 {
            assert_guidance(&client.tool("guidance", json!({})));
        }
        for name in ["generate_image_instructions", "generate_image"] {
            let response = client.rpc("tools/call", json!({"name": name, "arguments": {}}));
            assert_schema_rejected(&response);
        }
        client.finish();
        assert!(entries(directory.path()).is_empty());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert!(
            metadata
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|request| !request.headers.contains_key("authorization"))
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saves_all_formats_with_exact_bytes_and_metadata() {
    let server = MockServer::start().await;
    mount_catalog(&server).await;
    let metadata = metadata_proxy(1).await;
    let directory = tempfile::tempdir().unwrap();
    let mut client = StdioClient::start(&server, &metadata, directory.path(), "2025-11-25");
    assert_guidance(&client.tool("guidance", json!({})));
    for (model, source_format, _) in MODELS {
        let bytes = fixture(source_format);
        let (size, ratio): (Option<&str>, Option<&str>) = if is_gpt_model(model) {
            (Some("1536x1024"), None)
        } else {
            (Some("1K"), Some("16:9"))
        };
        generation_mock(model)
            .respond_with(generation_response(model, &bytes))
            .expect(4)
            .mount(&server)
            .await;
        for extension in ["png", "jpg", "jpeg", "webp"] {
            let output_dir = tempfile::tempdir_in(directory.path()).unwrap();
            let requested = output_dir.path().join(format!("asset.{extension}"));
            let result = client.generate(&requested, model, size, ratio);
            let result = structured(&result);
            let actual_extension = expected_extension(source_format, extension);
            let output = output_dir.path().join(format!("asset.{actual_extension}"));
            let prompt_file = output_dir
                .path()
                .join(format!("asset.{actual_extension}.prompt.json"));
            assert_eq!(result["path"], output.to_str().unwrap());
            assert_eq!(result["prompt_file"], prompt_file.to_str().unwrap());
            assert_eq!(result["model"], model);
            assert_eq!(result["width"], 128);
            assert_eq!(result["height"], 96);
            assert_eq!(result["mime_type"], source_format.to_mime_type());
            let saved = fs::read(&output).unwrap();
            assert_eq!(saved, bytes);
            assert_eq!(result["bytes"], saved.len());
            assert_eq!(image::guess_format(&saved).unwrap(), source_format);
            let sidecar: Value = serde_json::from_slice(&fs::read(&prompt_file).unwrap()).unwrap();
            assert_eq!(sidecar["requested_size"], size.unwrap());
            match ratio {
                Some(ratio) => assert_eq!(sidecar["requested_aspect_ratio"], ratio),
                None => assert!(sidecar["requested_aspect_ratio"].is_null()),
            }
            assert!(sidecar["response_model"].is_null());
            assert!(sidecar["revised_prompt"].is_null());
            assert_eq!(
                sidecar["output"],
                json!({
                    "path": output, "mime_type": source_format.to_mime_type(),
                    "width": 128, "height": 96, "bytes": bytes.len(),
                })
            );
            let prompt = if is_gpt_model(model) {
                &sidecar["request"]["prompt"]
            } else {
                &sidecar["request"]["messages"][0]["content"]
            };
            assert_eq!(prompt, PROMPT);
            assert_eq!(
                entries(output_dir.path()),
                BTreeSet::from([output, prompt_file])
            );
        }
    }
    client.finish();
    assert!(entries(directory.path()).is_empty());
    assert_eq!(server.received_requests().await.unwrap().len(), 13);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn modifies_actual_images_and_masks_with_exact_outputs_and_metadata() {
    let cases = [
        (
            "gpt-image-1",
            ImageFormat::Png,
            ImageFormat::Jpeg,
            "webp",
            Some("high"),
            "1024x1536",
        ),
        (
            "gpt-image-1.5",
            ImageFormat::Jpeg,
            ImageFormat::WebP,
            "png",
            Some("high"),
            "1536x1024",
        ),
        (
            "gpt-image-2",
            ImageFormat::WebP,
            ImageFormat::Png,
            "jpeg",
            None,
            "1600x1024",
        ),
    ];
    let server = MockServer::start().await;
    let ids: Vec<&str> = cases.iter().map(|case| case.0).collect();
    mount_models(&server, &ids).await;
    let metadata = metadata_proxy(1).await;
    let directory = tempfile::tempdir().unwrap();
    let mut client = StdioClient::start(&server, &metadata, directory.path(), "2025-11-25");
    assert_eq!(
        structured(&client.tool("guidance", json!({})))["models"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    for (model, input_format, response_format, extension, fidelity, native) in cases {
        for masked in [false, true] {
            for sized in [None, Some(native)] {
                let output_dir = tempfile::tempdir_in(directory.path()).unwrap();
                let input = output_dir.path().join("source.dat");
                let input_bytes = fixture(input_format);
                fs::write(&input, &input_bytes).unwrap();
                let mask = output_dir.path().join("mask.png");
                let mut options = json!({"model": model});
                let size_field = sized.unwrap_or("auto");
                if let Some(size) = sized {
                    options["size"] = json!(size);
                }
                let mut expected_request = json!({
                    "model": model, "prompt": EDIT_PROMPT, "n": 1, "size": size_field,
                    "quality": "auto", "output_format": "png",
                    "images": [{"image_url": format!("data:{};base64,{}", input_format.to_mime_type(), STANDARD.encode(&input_bytes))}],
                });
                if let Some(fidelity) = fidelity {
                    expected_request["input_fidelity"] = json!(fidelity);
                }
                let mask_bytes = if masked {
                    image::RgbaImage::from_fn(128, 96, |x, _| {
                        image::Rgba([0, 0, 0, if x < 64 { 0 } else { 255 }])
                    })
                    .save(&mask)
                    .unwrap();
                    let bytes = fs::read(&mask).unwrap();
                    options["mask_path"] = json!(mask);
                    expected_request["mask"] = json!({"image_url": format!("data:image/png;base64,{}", STANDARD.encode(&bytes))});
                    Some(bytes)
                } else {
                    None
                };
                for path in std::iter::once(&input).chain(mask_bytes.as_ref().map(|_| &mask)) {
                    let mut permissions = fs::metadata(path).unwrap().permissions();
                    permissions.set_readonly(true);
                    fs::set_permissions(path, permissions).unwrap();
                }
                let response_bytes = fixture(response_format);
                let mock = modification_mock()
                    .and(body_json(&expected_request))
                    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                        "model": model, "data": [{"b64_json": STANDARD.encode(&response_bytes), "revised_prompt": "provider revision"}],
                    })))
                    .expect(1).mount_as_scoped(&server).await;
                let output = output_dir.path().join(format!("edited.{extension}"));
                let result = client.modify(&input, &output, options);
                let result = structured(&result);
                let actual_extension = expected_extension(response_format, extension);
                let final_output = output_dir.path().join(format!("edited.{actual_extension}"));
                let prompt_file = output_dir
                    .path()
                    .join(format!("edited.{actual_extension}.prompt.json"));
                assert_eq!(result["path"], final_output.to_str().unwrap());
                assert_eq!(result["prompt_file"], prompt_file.to_str().unwrap());
                assert_eq!(result["model"], model);
                assert_eq!(result["width"], 128);
                assert_eq!(result["height"], 96);
                assert_eq!(result["mime_type"], response_format.to_mime_type());
                let saved = fs::read(&final_output).unwrap();
                assert_eq!(saved, response_bytes);
                assert_eq!(result["bytes"], saved.len());
                assert_eq!(image::guess_format(&saved).unwrap(), response_format);
                assert_ne!(response_bytes, input_bytes);
                assert_eq!(fs::read(&input).unwrap(), input_bytes);
                let sidecar_text = fs::read_to_string(&prompt_file).unwrap();
                assert!(!sidecar_text.contains("base64"));
                let sidecar: Value = serde_json::from_str(&sidecar_text).unwrap();
                expected_request.as_object_mut().unwrap().remove("images");
                expected_request.as_object_mut().unwrap().remove("mask");
                assert_eq!(sidecar["request"], expected_request);
                assert_eq!(sidecar["operation"], "modify");
                assert_eq!(sidecar["response_model"], model);
                assert_eq!(sidecar["revised_prompt"], "provider revision");
                assert!(
                    !sidecar
                        .as_object()
                        .unwrap()
                        .contains_key("requested_aspect_ratio")
                );
                match sized {
                    Some(size) => assert_eq!(sidecar["requested_size"], size),
                    None => assert!(sidecar["requested_size"].is_null()),
                }
                assert_eq!(
                    sidecar["input"],
                    json!({
                        "path": input, "mime_type": input_format.to_mime_type(),
                        "width": 128, "height": 96, "bytes": input_bytes.len(),
                    })
                );
                let mut expected_files =
                    BTreeSet::from([input.clone(), final_output.clone(), prompt_file.clone()]);
                if let Some(mask_bytes) = mask_bytes {
                    assert_eq!(fs::read(&mask).unwrap(), mask_bytes);
                    assert_eq!(
                        sidecar["mask"],
                        json!({
                            "path": mask, "mime_type": "image/png", "width": 128, "height": 96, "bytes": mask_bytes.len(),
                        })
                    );
                    expected_files.insert(mask.clone());
                } else {
                    assert!(sidecar["mask"].is_null());
                }
                assert_eq!(entries(output_dir.path()), expected_files);
                drop(mock);
            }
        }
    }
    client.finish();
    assert_eq!(server.received_requests().await.unwrap().len(), 13);
    assert!(entries(directory.path()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gpt_size_generation_matches_family_capabilities() {
    let legacy_models = [
        "gpt-image-1",
        "gpt-image-1.5",
        "gpt-image-1-mini",
        "account/gpt-image-1.5-2025-12-16",
    ];
    let flexible_models = [
        "gpt-image-2",
        "gpt-image-2.5",
        "account/gpt-image-2-2026-04-21",
        "gpt-image-2.5-flare",
    ];
    let unknown_models = ["gpt-image-smoke", "gpt-image-20", "gpt-image-unknown"];
    let ids: Vec<&str> = legacy_models
        .iter()
        .chain(flexible_models.iter())
        .chain(unknown_models.iter())
        .copied()
        .collect();
    let server = MockServer::start().await;
    mount_models(&server, &ids).await;
    let metadata = metadata_proxy(1).await;
    let directory = tempfile::tempdir().unwrap();
    let mut client = StdioClient::start(&server, &metadata, directory.path(), "2025-11-25");
    assert_guidance(&client.tool("guidance", json!({})));

    for model in legacy_models {
        for (input, native) in [
            (None, "auto"),
            (Some("auto"), "auto"),
            (Some("1024x1024"), "1024x1024"),
            (Some("1536x1024"), "1536x1024"),
            (Some("1024x1536"), "1024x1536"),
        ] {
            let expected = openai_generate_request(model, native);
            expect_generate_success(
                &server,
                &mut client,
                directory.path(),
                model,
                input,
                None,
                expected,
            )
            .await;
        }
        for invalid in ["1600x1024", "2048x2048", "512x512", "1K"] {
            expect_generate_error(
                &mut client,
                directory.path(),
                model,
                Some(invalid),
                None,
                "size",
            );
        }
        expect_generate_error(
            &mut client,
            directory.path(),
            model,
            Some("1024x1024"),
            Some("1:1"),
            "aspect",
        );
    }

    for model in flexible_models {
        for (input, native) in [
            (None, "auto"),
            (Some("auto"), "auto"),
            (Some("1600x1024"), "1600x1024"),
            (Some("1008x1008"), "1008x1008"),
            (Some("640x1024"), "640x1024"),
            (Some("3840x2160"), "3840x2160"),
            (Some("3840x1280"), "3840x1280"),
        ] {
            let expected = openai_generate_request(model, native);
            expect_generate_success(
                &server,
                &mut client,
                directory.path(),
                model,
                input,
                None,
                expected,
            )
            .await;
        }
        for invalid in [
            "320x2048",
            "1601x1024",
            "4096x1024",
            "512x512",
            "3840x3840",
            "0x1024",
            "100x0",
            "-100x100",
            "abcxdef",
            "1024",
            "",
            "99999999999x1024",
            "1K",
            "512",
        ] {
            expect_generate_error(
                &mut client,
                directory.path(),
                model,
                Some(invalid),
                None,
                "size",
            );
        }
        expect_generate_error(
            &mut client,
            directory.path(),
            model,
            Some("1024x1024"),
            Some("1:1"),
            "aspect",
        );
    }

    for model in unknown_models {
        for (input, native) in [
            (None, "auto"),
            (Some("auto"), "auto"),
            (Some("10000x10000"), "10000x10000"),
            (Some("50x50"), "50x50"),
            (Some("1600x1024"), "1600x1024"),
        ] {
            let expected = openai_generate_request(model, native);
            expect_generate_success(
                &server,
                &mut client,
                directory.path(),
                model,
                input,
                None,
                expected,
            )
            .await;
        }
        for invalid in [
            "1K",
            "512",
            "",
            "0x100",
            "100x0",
            "-100x100",
            "abcxdef",
            "1024",
            "99999999999x100",
        ] {
            expect_generate_error(
                &mut client,
                directory.path(),
                model,
                Some(invalid),
                None,
                "size",
            );
        }
        expect_generate_error(
            &mut client,
            directory.path(),
            model,
            Some("1024x1024"),
            Some("1:1"),
            "aspect",
        );
    }

    client.finish();
    assert!(entries(directory.path()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_size_and_aspect_ratio_capabilities() {
    let flash25 = "gemini-2.5-flash-image";
    let pro3 = "gemini-3-pro-image";
    let flash31 = "gemini-3.1-flash-image";
    let unknown = "gemini-image-smoke";
    let ids = [flash25, pro3, flash31, unknown];
    let server = MockServer::start().await;
    mount_models(&server, &ids).await;
    let metadata = metadata_proxy(1).await;
    let directory = tempfile::tempdir().unwrap();
    let mut client = StdioClient::start(&server, &metadata, directory.path(), "2025-11-25");
    assert_guidance(&client.tool("guidance", json!({})));

    for ratio in BASE_RATIOS {
        let expected = gemini_generate_request(flash25, gemini_config(None, Some(ratio)));
        expect_generate_success(
            &server,
            &mut client,
            directory.path(),
            flash25,
            None,
            Some(ratio),
            expected,
        )
        .await;
    }
    for ratio in EXTRA_RATIOS {
        expect_generate_error(
            &mut client,
            directory.path(),
            flash25,
            None,
            Some(ratio),
            "ratio",
        );
    }
    for size in GEMINI_SIZES {
        expect_generate_error(
            &mut client,
            directory.path(),
            flash25,
            Some(size),
            None,
            "size",
        );
    }
    {
        let expected = gemini_generate_request(flash25, None);
        expect_generate_success(
            &server,
            &mut client,
            directory.path(),
            flash25,
            None,
            None,
            expected,
        )
        .await;
    }

    for size in ["1K", "2K", "4K"] {
        for ratio in ["1:1", "16:9", "4:5"] {
            let expected = gemini_generate_request(pro3, gemini_config(Some(size), Some(ratio)));
            expect_generate_success(
                &server,
                &mut client,
                directory.path(),
                pro3,
                Some(size),
                Some(ratio),
                expected,
            )
            .await;
        }
    }
    expect_generate_error(
        &mut client,
        directory.path(),
        pro3,
        Some("512"),
        None,
        "size",
    );
    for ratio in EXTRA_RATIOS {
        expect_generate_error(
            &mut client,
            directory.path(),
            pro3,
            None,
            Some(ratio),
            "ratio",
        );
    }
    {
        let expected = gemini_generate_request(pro3, None);
        expect_generate_success(
            &server,
            &mut client,
            directory.path(),
            pro3,
            None,
            None,
            expected,
        )
        .await;
    }
    {
        let expected = gemini_generate_request(pro3, gemini_config(Some("2K"), None));
        expect_generate_success(
            &server,
            &mut client,
            directory.path(),
            pro3,
            Some("2K"),
            None,
            expected,
        )
        .await;
    }

    for size in GEMINI_SIZES {
        let expected = gemini_generate_request(flash31, gemini_config(Some(size), None));
        expect_generate_success(
            &server,
            &mut client,
            directory.path(),
            flash31,
            Some(size),
            None,
            expected,
        )
        .await;
    }
    for ratio in ALL_RATIOS {
        let expected = gemini_generate_request(flash31, gemini_config(None, Some(ratio)));
        expect_generate_success(
            &server,
            &mut client,
            directory.path(),
            flash31,
            None,
            Some(ratio),
            expected,
        )
        .await;
    }
    {
        let expected = gemini_generate_request(flash31, gemini_config(Some("512"), Some("8:1")));
        expect_generate_success(
            &server,
            &mut client,
            directory.path(),
            flash31,
            Some("512"),
            Some("8:1"),
            expected,
        )
        .await;
    }
    {
        let expected = gemini_generate_request(flash31, None);
        expect_generate_success(
            &server,
            &mut client,
            directory.path(),
            flash31,
            None,
            None,
            expected,
        )
        .await;
    }

    for size in GEMINI_SIZES {
        let expected = gemini_generate_request(unknown, gemini_config(Some(size), None));
        expect_generate_success(
            &server,
            &mut client,
            directory.path(),
            unknown,
            Some(size),
            None,
            expected,
        )
        .await;
    }
    for ratio in ALL_RATIOS {
        let expected = gemini_generate_request(unknown, gemini_config(None, Some(ratio)));
        expect_generate_success(
            &server,
            &mut client,
            directory.path(),
            unknown,
            None,
            Some(ratio),
            expected,
        )
        .await;
    }
    {
        let expected = gemini_generate_request(unknown, gemini_config(Some("2K"), Some("3:4")));
        expect_generate_success(
            &server,
            &mut client,
            directory.path(),
            unknown,
            Some("2K"),
            Some("3:4"),
            expected,
        )
        .await;
    }
    {
        let expected = gemini_generate_request(unknown, None);
        expect_generate_success(
            &server,
            &mut client,
            directory.path(),
            unknown,
            None,
            None,
            expected,
        )
        .await;
    }

    client.finish();
    assert!(entries(directory.path()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_size_values_must_match_exact_native_tiers() {
    let models = ["gemini-3.1-flash-image", "gemini-image-smoke"];
    let server = MockServer::start().await;
    mount_models(&server, &models).await;
    let metadata = metadata_proxy(1).await;
    let directory = tempfile::tempdir().unwrap();
    let mut client = StdioClient::start(&server, &metadata, directory.path(), "2025-11-25");
    assert_guidance(&client.tool("guidance", json!({})));
    for model in models {
        for invalid_size in ["1k", "2k", "4k", "auto", "1024x1024", "512x512", "", "64"] {
            expect_generate_error(
                &mut client,
                directory.path(),
                model,
                Some(invalid_size),
                None,
                "size",
            );
        }
    }
    client.finish();
    assert!(entries(directory.path()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn modify_size_follows_the_same_gpt_family_rules_as_generate() {
    let legacy = "gpt-image-1";
    let flexible = "gpt-image-2";
    let unknown = "gpt-image-smoke";
    let ids = [legacy, flexible, unknown];
    let server = MockServer::start().await;
    mount_models(&server, &ids).await;
    let metadata = metadata_proxy(1).await;
    let directory = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let input = source_dir.path().join("source.png");
    let input_bytes = fixture(ImageFormat::Png);
    fs::write(&input, &input_bytes).unwrap();
    let mut client = StdioClient::start(&server, &metadata, directory.path(), "2025-11-25");
    assert_guidance(&client.tool("guidance", json!({})));
    let bytes = fixture(ImageFormat::Png);

    let cases: [(&str, Vec<&str>, &str); 3] = [
        (
            legacy,
            vec!["auto", "1024x1024", "1536x1024", "1024x1536"],
            "1600x1024",
        ),
        (
            flexible,
            vec!["auto", "1600x1024", "1008x1008", "640x1024", "3840x2160"],
            "320x2048",
        ),
        (unknown, vec!["auto", "10000x10000", "50x50"], "1K"),
    ];
    for (model, valid_sizes, invalid_size) in cases {
        for size in &valid_sizes {
            let mut expected_request = json!({
                "model": model, "prompt": EDIT_PROMPT, "n": 1, "size": size,
                "quality": "auto", "output_format": "png",
                "images": [{"image_url": format!("data:image/png;base64,{}", STANDARD.encode(&input_bytes))}],
            });
            if model == legacy {
                expected_request["input_fidelity"] = json!("high");
            }
            let mock = modification_mock()
                .and(body_json(&expected_request))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "data": [{"b64_json": STANDARD.encode(&bytes)}],
                })))
                .expect(1)
                .mount_as_scoped(&server)
                .await;
            let output_dir = tempfile::tempdir_in(directory.path()).unwrap();
            let output = output_dir.path().join("edited.png");
            let options = json!({"model": model, "size": size});
            let result = client.modify(&input, &output, options);
            structured(&result);
            drop(mock);
        }
        let output_dir = tempfile::tempdir_in(directory.path()).unwrap();
        let output = output_dir.path().join("edited.png");
        let options = json!({"model": model, "size": invalid_size});
        let result = client.modify(&input, &output, options);
        assert_error(&result, "size");
        assert!(entries(output_dir.path()).is_empty());
    }

    let output_dir = tempfile::tempdir_in(directory.path()).unwrap();
    let output = output_dir.path().join("edited.png");
    let response = client.rpc(
        "tools/call",
        json!({"name": "modify", "arguments": {
            "model": legacy, "input_path": input, "local_path": output,
            "prompt": EDIT_PROMPT, "aspect_ratio": "1:1",
        }}),
    );
    assert_schema_rejected(&response);
    assert!(entries(output_dir.path()).is_empty());
    drop(output_dir);

    client.finish();
    assert_eq!(fs::read(&input).unwrap(), input_bytes);
    assert!(entries(directory.path()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_unknown_schema_fields_before_any_request() {
    let server = MockServer::start().await;
    let metadata = metadata_proxy(0).await;
    let directory = tempfile::tempdir().unwrap();
    let mut client = StdioClient::start(&server, &metadata, directory.path(), "2025-11-25");
    let output = directory.path().join("schema.png");
    for changes in [
        json!({"width": 1024}),
        json!({"height": 1024}),
        json!({"aspect_ratio": "1:2"}),
    ] {
        let mut arguments = json!({
            "model": "gpt-image-smoke", "local_path": output, "prompt": PROMPT,
        });
        arguments
            .as_object_mut()
            .unwrap()
            .extend(changes.as_object().unwrap().clone());
        let response = client.rpc(
            "tools/call",
            json!({"name": "generate", "arguments": arguments}),
        );
        assert_schema_rejected(&response);
    }
    let source_dir = tempfile::tempdir().unwrap();
    let input = source_dir.path().join("source.png");
    fs::write(&input, fixture(ImageFormat::Png)).unwrap();
    for changes in [
        json!({"width": 1024}),
        json!({"height": 1024}),
        json!({"aspect_ratio": "1:1"}),
    ] {
        let mut arguments = json!({
            "model": "gpt-image-smoke", "input_path": input, "local_path": output, "prompt": EDIT_PROMPT,
        });
        arguments
            .as_object_mut()
            .unwrap()
            .extend(changes.as_object().unwrap().clone());
        let response = client.rpc(
            "tools/call",
            json!({"name": "modify", "arguments": arguments}),
        );
        assert_schema_rejected(&response);
    }
    client.finish();
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(entries(directory.path()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_existing_destinations_without_api_requests() {
    let server = MockServer::start().await;
    let metadata = metadata_proxy(0).await;
    let directory = tempfile::tempdir().unwrap();
    let mut client = StdioClient::start(&server, &metadata, directory.path(), "2025-11-25");
    let source_dir = tempfile::tempdir().unwrap();
    let input = source_dir.path().join("source.png");
    let input_bytes = fixture(ImageFormat::Png);
    fs::write(&input, &input_bytes).unwrap();
    for editing in [false, true] {
        for suffix in ["", ".prompt.json"] {
            for conflict in [Conflict::File, Conflict::Symlink, Conflict::Directory] {
                let output_dir = tempfile::tempdir_in(directory.path()).unwrap();
                let output = output_dir.path().join("asset.png");
                let existing = output_dir.path().join(format!("asset.png{suffix}"));
                conflict.create(&existing);
                let result = if editing {
                    client.modify(&input, &output, json!({}))
                } else {
                    client.generate(&output, "gpt-image-smoke", None, None)
                };
                assert_error(&result, "already exists");
                conflict.assert_preserved(&existing);
                assert_eq!(fs::read(&input).unwrap(), input_bytes);
            }
        }
    }
    client.finish();
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(metadata.received_requests().await.unwrap().is_empty());
    assert!(entries(directory.path()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rolls_back_when_destinations_appear_during_generation_or_modification() {
    let server = MockServer::start().await;
    mount_catalog(&server).await;
    let metadata = metadata_proxy(1).await;
    let pending: Arc<Mutex<Option<(PathBuf, Conflict)>>> = Arc::new(Mutex::new(None));
    for mock in [generation_mock("gpt-image-smoke"), modification_mock()] {
        let conflict = pending.clone();
        let response = generation_response("gpt-image-smoke", &fixture(ImageFormat::Png));
        mock.respond_with(move |_: &Request| {
            let (path, kind) = conflict.lock().unwrap().take().unwrap();
            kind.create(&path);
            response.clone()
        })
        .expect(6)
        .mount(&server)
        .await;
    }
    let directory = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let input = source_dir.path().join("source.webp");
    let input_bytes = fixture(ImageFormat::WebP);
    fs::write(&input, &input_bytes).unwrap();
    let mask = source_dir.path().join("mask.png");
    image::RgbaImage::new(128, 96).save(&mask).unwrap();
    let mask_bytes = fs::read(&mask).unwrap();
    let mut client = StdioClient::start(&server, &metadata, directory.path(), "2025-11-25");
    assert_guidance(&client.tool("guidance", json!({})));
    for editing in [false, true] {
        for (suffix, expected) in [
            ("", "Cannot save image without overwriting"),
            (".prompt.json", "Prompt save failed"),
        ] {
            for kind in [Conflict::File, Conflict::Symlink, Conflict::Directory] {
                let output_dir = tempfile::tempdir_in(directory.path()).unwrap();
                let output = output_dir.path().join("asset.png");
                let existing = output_dir.path().join(format!("asset.png{suffix}"));
                *pending.lock().unwrap() = Some((existing.clone(), kind));
                let result = if editing {
                    client.modify(&input, &output, json!({"mask_path": mask}))
                } else {
                    client.generate(&output, "gpt-image-smoke", None, None)
                };
                assert_error(&result, expected);
                assert!(pending.lock().unwrap().is_none());
                kind.assert_preserved(&existing);
                assert_eq!(fs::read(&input).unwrap(), input_bytes);
                assert_eq!(fs::read(&mask).unwrap(), mask_bytes);
            }
        }
    }
    client.finish();
    assert_eq!(server.received_requests().await.unwrap().len(), 13);
    assert!(entries(directory.path()).is_empty());
    assert_eq!(entries(source_dir.path()), BTreeSet::from([input, mask]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_invalid_edits_without_requests_or_input_changes() {
    let server = MockServer::start().await;
    let metadata = metadata_proxy(0).await;
    let directory = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let input = source_dir.path().join("source.png");
    let input_bytes = fixture(ImageFormat::Png);
    fs::write(&input, &input_bytes).unwrap();
    let mask = source_dir.path().join("mask.png");
    image::RgbaImage::new(128, 96).save(&mask).unwrap();
    let mask_bytes = fs::read(&mask).unwrap();
    let output = directory.path().join("edited.png");
    let mut client = StdioClient::start(&server, &metadata, directory.path(), "2025-11-25");
    for (options, error) in [
        (json!({"model": "gemini-image-smoke"}), "only OpenAI"),
        (json!({"model": "iq-image"}), "only OpenAI"),
        (json!({"prompt": "\n "}), "prompt must contain"),
        (json!({"size": "2K"}), "size"),
        (json!({"size": ""}), "size"),
        (json!({"input_path": "relative.png"}), "absolute"),
        (json!({"input_path": source_dir.path()}), "regular file"),
        (
            json!({"input_path": source_dir.path().join("missing.png")}),
            "Cannot resolve",
        ),
        (json!({"mask_path": "relative.png"}), "absolute"),
        (json!({"mask_path": input}), "alpha channel"),
        (json!({"local_path": "relative.png"}), "absolute"),
        (json!({"local_path": input}), "already exists"),
        (
            json!({"local_path": mask, "mask_path": mask}),
            "already exists",
        ),
        (
            json!({"local_path": directory.path().join("edited.gif")}),
            "must end in",
        ),
    ] {
        assert_error(&client.modify(&input, &output, options), error);
        assert!(entries(directory.path()).is_empty());
        assert_eq!(fs::read(&input).unwrap(), input_bytes);
        assert_eq!(fs::read(&mask).unwrap(), mask_bytes);
    }
    let invalid = source_dir.path().join("invalid.png");
    for (bytes, error) in [
        (Vec::new(), "must not be empty"),
        (b"not an image".to_vec(), "Unknown input image format"),
        (b"GIF89a".to_vec(), "PNG, JPEG, or WebP"),
        (b"\x89PNG\r\n\x1a\n".to_vec(), "Invalid input image"),
    ] {
        fs::write(&invalid, &bytes).unwrap();
        assert_error(&client.modify(&invalid, &output, json!({})), error);
        assert_eq!(fs::read(&invalid).unwrap(), bytes);
        assert!(entries(directory.path()).is_empty());
    }
    for (field, limit) in [("input_path", 50_000_000), ("mask_path", 4_000_000)] {
        fs::File::create(&invalid).unwrap().set_len(limit).unwrap();
        let mut options = json!({});
        options[field] = json!(invalid);
        assert_error(
            &client.modify(&input, &output, options),
            "must contain fewer than",
        );
        assert_eq!(fs::metadata(&invalid).unwrap().len(), limit);
        assert!(entries(directory.path()).is_empty());
    }
    let mut base = json!({"model": "gpt-image-smoke", "input_path": input, "local_path": output, "prompt": EDIT_PROMPT});
    for field in ["model", "input_path", "local_path", "prompt"] {
        let mut arguments = base.clone();
        arguments.as_object_mut().unwrap().remove(field);
        let response = client.rpc(
            "tools/call",
            json!({"name": "modify", "arguments": arguments}),
        );
        assert_schema_rejected(&response);
    }
    for changes in [
        json!({"width": 1024}),
        json!({"height": 1024}),
        json!({"aspect_ratio": "1:2"}),
        json!({"unexpected": true}),
    ] {
        let mut arguments = base.clone();
        arguments
            .as_object_mut()
            .unwrap()
            .extend(changes.as_object().unwrap().clone());
        let response = client.rpc(
            "tools/call",
            json!({"name": "modify", "arguments": arguments}),
        );
        assert_schema_rejected(&response);
    }
    base["unexpected"] = json!(true);
    let response = client.rpc("tools/call", json!({"name": "modify", "arguments": base}));
    assert_schema_rejected(&response);
    client.finish();
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(metadata.received_requests().await.unwrap().is_empty());
    assert!(entries(directory.path()).is_empty());
    assert_eq!(fs::read(input).unwrap(), input_bytes);
    assert_eq!(fs::read(mask).unwrap(), mask_bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_errors_do_not_retry_leak_keys_or_leave_output_files() {
    let server = MockServer::start().await;
    mount_catalog(&server).await;
    let metadata = metadata_proxy(1).await;
    let directory = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let input = source_dir.path().join("source.jpg");
    let input_bytes = fixture(ImageFormat::Jpeg);
    fs::write(&input, &input_bytes).unwrap();
    let mask = source_dir.path().join("mask.png");
    image::RgbaImage::new(128, 96).save(&mask).unwrap();
    let mask_bytes = fs::read(&mask).unwrap();
    let output = directory.path().join("edited.png");
    let mut client = StdioClient::start(&server, &metadata, directory.path(), "2025-11-25");
    assert_guidance(&client.tool("guidance", json!({})));
    assert_error(
        &client.modify(&input, &output, json!({"model": "gpt-image-missing"})),
        "live image catalog",
    );
    let cases = [
        (
            ResponseTemplate::new(429)
                .set_body_json(json!({"error": {"message": "local-test-key quota exceeded"}})),
            "429",
        ),
        (
            ResponseTemplate::new(500)
                .set_body_json(json!({"error": {"message": "local-test-key upstream failure"}})),
            "500",
        ),
        (
            ResponseTemplate::new(307)
                .insert_header("Location", format!("{}/redirect", server.uri())),
            "307",
        ),
        (
            ResponseTemplate::new(200).set_body_string("invalid JSON"),
            "invalid JSON",
        ),
        (
            ResponseTemplate::new(200).set_body_json(json!({"error": {"message": "refused"}})),
            "Provider returned an error",
        ),
        (
            ResponseTemplate::new(200).set_body_json(json!({"data": []})),
            "Expected one image",
        ),
        (
            ResponseTemplate::new(200).set_body_json(json!({"data": [{"b64_json": "!"}]})),
            "Invalid image base64",
        ),
        (
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"url": format!("{}/remote.png", server.uri())}]})),
            "Remote image URLs are not fetched",
        ),
        (
            ResponseTemplate::new(200).set_body_json(
                json!({"data": [{"b64_json": STANDARD.encode(b"\x89PNG\r\n\x1a\n") }]}),
            ),
            "invalid image",
        ),
    ];
    let expected_requests = cases.len() + 1;
    for (response, error) in cases {
        let mock = modification_mock()
            .respond_with(response)
            .expect(1)
            .mount_as_scoped(&server)
            .await;
        assert_error(
            &client.modify(&input, &output, json!({"mask_path": mask})),
            error,
        );
        assert!(entries(directory.path()).is_empty());
        assert_eq!(fs::read(&input).unwrap(), input_bytes);
        assert_eq!(fs::read(&mask).unwrap(), mask_bytes);
        drop(mock);
    }
    client.finish();
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        expected_requests
    );
    assert_eq!(entries(source_dir.path()), BTreeSet::from([input, mask]));
    assert!(entries(directory.path()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reports_only_discovery_errors_then_recovers_and_caches() {
    let server = MockServer::start().await;
    let failure = Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": {"message": "local-test-key rejected"},
        })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;
    let metadata = metadata_proxy(2).await;
    let directory = tempfile::tempdir().unwrap();
    let mut client = StdioClient::start(&server, &metadata, directory.path(), "2025-11-25");
    let result = client.tool("guidance", json!({}));
    assert_error(&result, "401");
    assert!(
        !result
            .to_string()
            .contains("Preserve every explicit user requirement")
    );
    drop(failure);
    mount_catalog(&server).await;
    for _ in 0..2 {
        assert_guidance(&client.tool("guidance", json!({})));
    }
    client.finish();
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    assert!(entries(directory.path()).is_empty());
}
