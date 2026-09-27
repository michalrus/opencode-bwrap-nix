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
use image::{GenericImageView, ImageFormat};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
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

    fn generate(&mut self, path: &Path, model: &str, dimensions: (u32, u32)) -> Value {
        self.tool(
            "generate",
            json!({
                "model": model, "local_path": path, "prompt": PROMPT,
                "width": dimensions.0, "height": dimensions.1,
            }),
        )
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

async fn mount_catalog(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer local-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": MODELS.iter().map(|(model, _, _)| json!({"id": model})).collect::<Vec<_>>(),
        })))
        .expect(1)
        .mount(server)
        .await;
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
    assert_eq!(
        data["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["model"].as_str().unwrap())
            .collect::<BTreeSet<_>>(),
        MODELS.iter().map(|(model, _, _)| *model).collect(),
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

fn generation_response(model: &str, bytes: &[u8]) -> ResponseTemplate {
    let encoded = STANDARD.encode(bytes);
    let response = if model.starts_with("gpt-") {
        json!({"data": [{"b64_json": encoded}]})
    } else {
        json!({"choices": [{"message": {"images": [{"image_url": {
            "url": format!("data:image/png;base64,{encoded}"),
        }}]}}]})
    };
    ResponseTemplate::new(200).set_body_json(response)
}

fn generation_mock(model: &str) -> wiremock::MockBuilder {
    let (endpoint, body) = if model.starts_with("gpt-") {
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
        assert_eq!(
            generation["inputSchema"]["properties"]
                .as_object()
                .unwrap()
                .len(),
            5
        );
        assert_eq!(
            generation["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|field| field.as_str().unwrap())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["model", "local_path", "width", "height", "prompt"])
        );
        let modification = tools.iter().find(|tool| tool["name"] == "modify").unwrap();
        let schema = &modification["inputSchema"];
        assert_eq!(
            schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "model",
                "input_path",
                "local_path",
                "prompt",
                "width",
                "height",
                "mask_path"
            ]),
        );
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
            assert!(response["error"].is_object() || response["result"]["isError"] == true);
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
async fn saves_all_formats_with_exact_originals_and_metadata() {
    let server = MockServer::start().await;
    mount_catalog(&server).await;
    let metadata = metadata_proxy(1).await;
    let directory = tempfile::tempdir().unwrap();
    let mut client = StdioClient::start(&server, &metadata, directory.path(), "2025-11-25");
    assert_guidance(&client.tool("guidance", json!({})));
    for (model, source_format, source_extension) in MODELS {
        let bytes = fixture(source_format);
        generation_mock(model)
            .respond_with(generation_response(model, &bytes))
            .expect(8)
            .mount(&server)
            .await;
        for (extension, format) in [
            ("png", ImageFormat::Png),
            ("jpg", ImageFormat::Jpeg),
            ("jpeg", ImageFormat::Jpeg),
            ("webp", ImageFormat::WebP),
        ] {
            for (width, height) in [(128, 96), (64, 128)] {
                let output_dir = tempfile::tempdir_in(directory.path()).unwrap();
                let output = output_dir.path().join(format!("asset.{extension}"));
                let result = client.generate(&output, model, (width, height));
                let result = structured(&result);
                let original = output_dir
                    .path()
                    .join(format!("asset.{extension}.original.{source_extension}"));
                let prompt_file = output_dir
                    .path()
                    .join(format!("asset.{extension}.prompt.json"));
                assert_eq!(result["path"], output.to_str().unwrap());
                assert_eq!(result["original_path"], original.to_str().unwrap());
                assert_eq!(result["prompt_file"], prompt_file.to_str().unwrap());
                assert_eq!(fs::read(&original).unwrap(), bytes);
                assert_eq!(image::open(&original).unwrap().dimensions(), (128, 96));
                let saved = fs::read(&output).unwrap();
                assert_eq!(image::guess_format(&saved).unwrap(), format);
                assert_eq!(image::open(&output).unwrap().dimensions(), (width, height));
                assert_eq!(result["mime_type"], format.to_mime_type());
                assert_eq!(result["bytes"], saved.len());
                assert_eq!(result["width"], width);
                assert_eq!(result["height"], height);
                assert_eq!(result["source_width"], 128);
                assert_eq!(result["source_height"], 96);
                assert_eq!(result["converted"], format != source_format);
                assert_eq!(result["resized"], (width, height) != (128, 96));
                if format == source_format && (width, height) == (128, 96) {
                    assert_eq!(saved, bytes);
                }
                let sidecar: Value =
                    serde_json::from_slice(&fs::read(&prompt_file).unwrap()).unwrap();
                assert_eq!(
                    sidecar["original"],
                    json!({
                        "path": original, "mime_type": source_format.to_mime_type(),
                        "width": 128, "height": 96, "bytes": bytes.len(),
                    })
                );
                assert_eq!(
                    sidecar["requested_dimensions"],
                    json!({"width": width, "height": height})
                );
                let prompt = if model.starts_with("gpt-") {
                    &sidecar["request"]["prompt"]
                } else {
                    &sidecar["request"]["messages"][0]["content"]
                };
                assert_eq!(prompt, PROMPT);
                assert_eq!(
                    entries(output_dir.path()),
                    BTreeSet::from([output, original, prompt_file])
                );
            }
        }
    }
    client.finish();
    assert!(entries(directory.path()).is_empty());
    assert_eq!(server.received_requests().await.unwrap().len(), 25);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn modifies_actual_images_and_masks_with_exact_outputs_and_metadata() {
    let cases = [
        (
            "gpt-image-1",
            ImageFormat::Png,
            ImageFormat::Jpeg,
            "jpg",
            "webp",
            ImageFormat::WebP,
            Some("high"),
        ),
        (
            "gpt-image-1.5",
            ImageFormat::Jpeg,
            ImageFormat::WebP,
            "webp",
            "png",
            ImageFormat::Png,
            Some("high"),
        ),
        (
            "gpt-image-2",
            ImageFormat::WebP,
            ImageFormat::Png,
            "png",
            "jpeg",
            ImageFormat::Jpeg,
            None,
        ),
    ];
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer local-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": cases.iter().map(|case| json!({"id": case.0})).collect::<Vec<_>>(),
        })))
        .expect(1)
        .mount(&server)
        .await;
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
    for (
        model,
        input_format,
        response_format,
        response_extension,
        extension,
        output_format,
        fidelity,
    ) in cases
    {
        for masked in [false, true] {
            for dimensions in [None, Some((64, 128))] {
                let output_dir = tempfile::tempdir_in(directory.path()).unwrap();
                let input = output_dir.path().join("source.dat");
                let input_bytes = fixture(input_format);
                fs::write(&input, &input_bytes).unwrap();
                let mask = output_dir.path().join("mask.png");
                let mut options = json!({"model": model});
                let (width, height) = dimensions.unwrap_or((128, 96));
                if dimensions.is_some() {
                    options["width"] = json!(width);
                    options["height"] = json!(height);
                }
                let mut expected_request = json!({
                    "model": model, "prompt": EDIT_PROMPT, "n": 1,
                    "size": if dimensions.is_some() { "1024x1536" } else { "auto" },
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
                let original = output_dir
                    .path()
                    .join(format!("edited.{extension}.original.{response_extension}"));
                let prompt_file = output_dir
                    .path()
                    .join(format!("edited.{extension}.prompt.json"));
                assert_eq!(result["path"], output.to_str().unwrap());
                assert_eq!(result["original_path"], original.to_str().unwrap());
                assert_eq!(result["prompt_file"], prompt_file.to_str().unwrap());
                assert_eq!(result["model"], model);
                assert_eq!(result["width"], width);
                assert_eq!(result["height"], height);
                assert_eq!(result["mime_type"], output_format.to_mime_type());
                assert_eq!(result["bytes"], fs::metadata(&output).unwrap().len());
                assert_eq!(image::open(&output).unwrap().dimensions(), (width, height));
                assert_eq!(
                    image::guess_format(&fs::read(&output).unwrap()).unwrap(),
                    output_format
                );
                assert_eq!(fs::read(&original).unwrap(), response_bytes);
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
                assert_eq!(
                    sidecar["requested_dimensions"],
                    json!({"width": width, "height": height})
                );
                assert_eq!(
                    sidecar["input"],
                    json!({
                        "path": input, "mime_type": input_format.to_mime_type(),
                        "width": 128, "height": 96, "bytes": input_bytes.len(),
                    })
                );
                assert_eq!(
                    sidecar["original"],
                    json!({
                        "path": original, "mime_type": response_format.to_mime_type(),
                        "width": 128, "height": 96, "bytes": response_bytes.len(),
                    })
                );
                let mut expected_files = BTreeSet::from([input, output, original, prompt_file]);
                if let Some(mask_bytes) = mask_bytes {
                    assert_eq!(fs::read(&mask).unwrap(), mask_bytes);
                    assert_eq!(
                        sidecar["mask"],
                        json!({
                            "path": mask, "mime_type": "image/png", "width": 128, "height": 96, "bytes": mask_bytes.len(),
                        })
                    );
                    expected_files.insert(mask);
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
        for suffix in [
            "",
            ".prompt.json",
            ".original.png",
            ".original.jpg",
            ".original.webp",
        ] {
            for conflict in [Conflict::File, Conflict::Symlink, Conflict::Directory] {
                let output_dir = tempfile::tempdir_in(directory.path()).unwrap();
                let output = output_dir.path().join("asset.png");
                let existing = output_dir.path().join(format!("asset.png{suffix}"));
                conflict.create(&existing);
                let result = if editing {
                    client.modify(&input, &output, json!({}))
                } else {
                    client.generate(&output, "gpt-image-smoke", (128, 96))
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
        .expect(9)
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
            (".original.png", "Original save failed"),
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
                    client.generate(&output, "gpt-image-smoke", (128, 96))
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
    assert_eq!(server.received_requests().await.unwrap().len(), 19);
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
        (json!({"width": 128}), "Provide both width and height"),
        (json!({"height": 96}), "Provide both width and height"),
        (json!({"width": 63, "height": 96}), "between 64 and 4096"),
        (json!({"width": 4097, "height": 96}), "between 64 and 4096"),
        (json!({"width": 64, "height": 4096}), "Aspect ratio"),
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
    for (width, height) in [(63, 96), (4097, 96), (64, 1024)] {
        image::RgbImage::new(width, height).save(&invalid).unwrap();
        let bytes = fs::read(&invalid).unwrap();
        assert_error(
            &client.modify(&invalid, &output, json!({})),
            "Invalid modification output",
        );
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
        assert!(response["error"].is_object() || response["result"]["isError"] == true);
    }
    base["unexpected"] = json!(true);
    let response = client.rpc("tools/call", json!({"name": "modify", "arguments": base}));
    assert!(response["error"].is_object() || response["result"]["isError"] == true);
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
