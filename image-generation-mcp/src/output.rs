use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use image::{GenericImageView, ImageFormat, ImageReader};
use serde_json::{Value, json};
use tempfile::{Builder, NamedTempFile, TempPath};

const EXTENSIONS: [&str; 4] = ["png", "jpg", "jpeg", "webp"];

pub struct Destination {
    parent: PathBuf,
    stem: OsString,
    requested_extension: String,
    image_file: NamedTempFile,
    prompt_file: NamedTempFile,
    _reservation: TempPath,
}

pub struct Decoded {
    pub bytes: Vec<u8>,
    pub revised_prompt: Option<String>,
}

pub fn decode(response: &Value) -> Result<Decoded> {
    ensure!(response.is_object(), "Expected a JSON response object");
    ensure!(
        response.get("error").is_none_or(Value::is_null),
        "Provider returned an error"
    );
    let mut images = Vec::new();
    if let Some(data) = response.get("data") {
        for item in data.as_array().context("Invalid image data array")? {
            images.push((
                item.get("b64_json").and_then(Value::as_str),
                item.get("url").and_then(Value::as_str),
                item.get("revised_prompt").and_then(Value::as_str),
            ));
        }
    }
    if let Some(choices) = response.get("choices") {
        for choice in choices.as_array().context("Invalid choices array")? {
            if let Some(items) = choice.pointer("/message/images") {
                for item in items.as_array().context("Invalid message images array")? {
                    images.push((
                        None,
                        item.pointer("/image_url/url").and_then(Value::as_str),
                        None,
                    ));
                }
            }
        }
    }
    ensure!(
        images.len() == 1,
        "Expected one image, received {}. No automatic retry.",
        images.len()
    );
    let (encoded, url, revised_prompt) = images[0];
    let encoded = match encoded {
        Some(encoded) => encoded,
        None => {
            let (header, data) = url.and_then(|url| url.split_once(',')).context(
                "No inline image. Remote image URLs are not fetched or sent credentials.",
            )?;
            ensure!(
                header.starts_with("data:image/") && header.ends_with(";base64"),
                "Expected a base64 image data URL"
            );
            data
        }
    };
    Ok(Decoded {
        bytes: STANDARD.decode(encoded).context("Invalid image base64")?,
        revised_prompt: revised_prompt.map(str::to_owned),
    })
}

fn require_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!(
            "Destination already exists: {}. Choose another path.",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("Cannot inspect {}", path.display())),
    }
}

fn variant_path(parent: &Path, stem: &OsStr, extension: &str) -> PathBuf {
    let mut name = stem.to_os_string();
    name.push(".");
    name.push(extension);
    parent.join(name)
}

fn sidecar_path(path: &Path) -> PathBuf {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(".prompt.json");
    PathBuf::from(sidecar)
}

impl Destination {
    pub fn prepare(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "local_path must be absolute");
        let parent = path
            .parent()
            .context("Output needs a parent directory")?
            .canonicalize()
            .context("Output directory must already exist")?;
        let name = path.file_name().context("Output needs a file name")?;
        let path = parent.join(name);
        let requested_extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .filter(|extension| EXTENSIONS.contains(&extension.as_str()))
            .context("local_path must end in .png, .jpg, .jpeg, or .webp")?;
        let stem = path
            .file_stem()
            .context("Output needs a file name")?
            .to_os_string();
        let mut lock_name = stem.clone();
        lock_name.push(".image-generation.lock");
        let lock = parent.join(lock_name);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .context("Cannot reserve output path. Another generation may be in progress.")?;
        let reservation = TempPath::try_from_path(lock)?;
        require_absent(&path)?;
        require_absent(&sidecar_path(&path))?;
        for extension in EXTENSIONS {
            let candidate = variant_path(&parent, &stem, extension);
            require_absent(&candidate)?;
            require_absent(&sidecar_path(&candidate))?;
        }
        let image_file = Builder::new()
            .prefix(".image-generation-")
            .tempfile_in(&parent)?;
        let prompt_file = Builder::new()
            .prefix(".image-prompt-")
            .tempfile_in(&parent)?;
        Ok(Self {
            parent,
            stem,
            requested_extension,
            image_file,
            prompt_file,
            _reservation: reservation,
        })
    }

    pub fn save(mut self, decoded: Decoded, mut metadata: Value) -> Result<Value> {
        let format = image::guess_format(&decoded.bytes).context("Unknown image format")?;
        let extension = match format {
            ImageFormat::Png => "png",
            ImageFormat::WebP => "webp",
            ImageFormat::Jpeg if self.requested_extension == "jpeg" => "jpeg",
            ImageFormat::Jpeg => "jpg",
            _ => bail!("Unsupported provider image format"),
        };
        let mut reader = ImageReader::with_format(Cursor::new(&decoded.bytes), format);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(16384);
        limits.max_image_height = Some(16384);
        limits.max_alloc = Some(512 * 1024 * 1024);
        reader.limits(limits);
        let image = reader
            .decode()
            .context("Provider returned an invalid image")?;
        let (width, height) = image.dimensions();
        ensure!(width > 0 && height > 0, "Empty provider image");
        let path = variant_path(&self.parent, &self.stem, extension);
        let sidecar = sidecar_path(&path);
        let mime = format.to_mime_type();
        let bytes = decoded.bytes.len();
        self.image_file.write_all(&decoded.bytes)?;
        metadata["revised_prompt"] = json!(decoded.revised_prompt);
        metadata["output"] = json!({
            "path": path, "mime_type": mime, "width": width, "height": height, "bytes": bytes,
        });
        serde_json::to_writer_pretty(&mut self.prompt_file, &metadata)?;
        self.prompt_file.write_all(b"\n")?;
        self.image_file.as_file().sync_all()?;
        self.prompt_file.as_file().sync_all()?;
        let Self {
            image_file,
            prompt_file,
            _reservation,
            ..
        } = self;
        let saved_image = image_file
            .persist_noclobber(&path)
            .map_err(|error| error.error)
            .context("Cannot save image without overwriting")?;
        if let Err(error) = prompt_file
            .persist_noclobber(&sidecar)
            .map_err(|error| error.error)
        {
            drop(saved_image);
            fs::remove_file(&path).context("Prompt save failed and image rollback failed")?;
            return Err(error).context("Prompt save failed; the partial image was removed");
        }
        Ok(json!({
            "path": path, "prompt_file": sidecar, "width": width, "height": height,
            "mime_type": mime, "bytes": bytes, "model": metadata["request"]["model"],
        }))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn fixture(format: ImageFormat) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(8, 6, image::Rgb([80, 160, 200]));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, format).unwrap();
        bytes.into_inner()
    }

    #[test]
    fn handles_both_providers_and_rejects_invalid_results() {
        let bytes = fixture(ImageFormat::Png);
        let b64 = STANDARD.encode(&bytes);
        let openai = json!({"data": [{"b64_json": b64, "revised_prompt": "revised"}]});
        let gemini = json!({"choices": [{"message": {"images": [{"image_url": {"url": format!("data:image/png;base64,{b64}")}}]}}]});
        assert_eq!(decode(&openai).unwrap().bytes, bytes);
        assert_eq!(decode(&gemini).unwrap().bytes, bytes);
        assert_eq!(
            decode(&openai).unwrap().revised_prompt.as_deref(),
            Some("revised")
        );
        for response in [
            json!({}),
            json!(null),
            json!({"data": null}),
            json!({"error": "refused"}),
            json!({"data": [{"b64_json": "!"}]}),
            json!({"data": [{"url": "https://example.com/image"}]}),
            json!({"data": [{"b64_json": b64}, {"b64_json": b64}]}),
        ] {
            assert!(decode(&response).is_err());
        }
    }

    #[test]
    fn saves_exact_bytes_and_actual_dimensions() {
        for (extension, format) in [
            ("png", ImageFormat::Png),
            ("jpg", ImageFormat::Jpeg),
            ("webp", ImageFormat::WebP),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("image.{extension}"));
            let bytes = fixture(format);
            let destination = Destination::prepare(&path).unwrap();
            let result = destination
                .save(
                    Decoded {
                        bytes: bytes.clone(),
                        revised_prompt: Some("revised".to_string()),
                    },
                    json!({"request": {"model": "gemini-image"}}),
                )
                .unwrap();
            assert_eq!(result["path"], path.to_str().unwrap());
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(result["width"], 8);
            assert_eq!(result["height"], 6);
            assert_eq!(result["mime_type"], format.to_mime_type());
            assert_eq!(result["bytes"], bytes.len());
            assert_eq!(result["model"], "gemini-image");
            assert!(result.get("original_path").is_none());
            assert!(result.get("resized").is_none());
            assert!(result.get("converted").is_none());
            let sidecar = dir.path().join(format!("image.{extension}.prompt.json"));
            assert_eq!(result["prompt_file"], sidecar.to_str().unwrap());
            let metadata: Value = serde_json::from_slice(&fs::read(&sidecar).unwrap()).unwrap();
            assert_eq!(metadata["revised_prompt"], "revised");
            assert_eq!(metadata["output"]["path"], path.to_str().unwrap());
            assert_eq!(metadata["output"]["width"], 8);
            assert_eq!(metadata["output"]["height"], 6);
            assert_eq!(metadata["output"]["bytes"], bytes.len());
            assert!(metadata.get("original").is_none());
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
            assert!(!dir.path().join("image.image-generation.lock").exists());
        }
    }

    #[test]
    fn corrects_extension_to_match_provider_format() {
        for (requested, provider_format, expected_extension) in [
            ("png", ImageFormat::Jpeg, "jpg"),
            ("jpg", ImageFormat::Png, "png"),
            ("jpeg", ImageFormat::WebP, "webp"),
            ("webp", ImageFormat::Png, "png"),
            ("PNG", ImageFormat::Png, "png"),
            ("JPEG", ImageFormat::Jpeg, "jpeg"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let requested_path = dir.path().join(format!("image.{requested}"));
            let bytes = fixture(provider_format);
            let destination = Destination::prepare(&requested_path).unwrap();
            let result = destination
                .save(
                    Decoded {
                        bytes: bytes.clone(),
                        revised_prompt: None,
                    },
                    json!({}),
                )
                .unwrap();
            let expected_path = dir.path().join(format!("image.{expected_extension}"));
            assert_eq!(result["path"], expected_path.to_str().unwrap());
            assert!(!requested_path.exists());
            assert_eq!(fs::read(&expected_path).unwrap(), bytes);
            let sidecar = dir
                .path()
                .join(format!("image.{expected_extension}.prompt.json"));
            assert_eq!(result["prompt_file"], sidecar.to_str().unwrap());
            assert!(sidecar.exists());
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
        }
    }

    #[test]
    fn preserves_jpeg_extension_when_requested_and_provided() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.jpeg");
        let bytes = fixture(ImageFormat::Jpeg);
        let result = Destination::prepare(&path)
            .unwrap()
            .save(
                Decoded {
                    bytes: bytes.clone(),
                    revised_prompt: None,
                },
                json!({}),
            )
            .unwrap();
        assert_eq!(result["path"], path.to_str().unwrap());
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn refuses_when_any_extension_variant_or_sidecar_exists() {
        for conflict in [
            "image.png",
            "image.jpg",
            "image.jpeg",
            "image.webp",
            "image.png.prompt.json",
            "image.jpg.prompt.json",
            "image.jpeg.prompt.json",
            "image.webp.prompt.json",
        ] {
            for symlink in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let conflict_path = dir.path().join(conflict);
                if symlink {
                    std::os::unix::fs::symlink(dir.path().join("missing"), &conflict_path).unwrap();
                } else {
                    fs::write(&conflict_path, b"keep").unwrap();
                }
                let path = dir.path().join("image.png");
                let error = Destination::prepare(&path).err().unwrap();
                assert!(error.to_string().contains("already exists"));
                assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
            }
        }
    }

    #[test]
    fn refuses_existing_requested_paths_with_uppercase_extensions() {
        for suffix in ["", ".prompt.json"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("image.PNG");
            let conflict = dir.path().join(format!("image.PNG{suffix}"));
            std::os::unix::fs::symlink(dir.path().join("missing"), &conflict).unwrap();
            assert!(Destination::prepare(&path).is_err());
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
            assert_eq!(fs::read_link(conflict).unwrap(), dir.path().join("missing"));
        }
    }

    #[test]
    fn reserves_a_shared_lock_across_extension_variants() {
        let dir = tempfile::tempdir().unwrap();
        let destination = Destination::prepare(&dir.path().join("image.png")).unwrap();
        for extension in EXTENSIONS {
            let error = Destination::prepare(&dir.path().join(format!("image.{extension}")))
                .err()
                .unwrap();
            assert!(error.to_string().contains("reserve"));
        }
        assert!(Destination::prepare(&dir.path().join("other.png")).is_ok());
        let lock = dir.path().join("image.image-generation.lock");
        assert!(lock.exists());
        drop(destination);
        assert!(!lock.exists());
        assert!(Destination::prepare(&dir.path().join("image.jpg")).is_ok());
    }

    #[test]
    fn rolls_back_image_when_sidecar_persist_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        let destination = Destination::prepare(&path).unwrap();
        let sidecar = dir.path().join("image.png.prompt.json");
        fs::write(&sidecar, b"keep").unwrap();
        let error = destination
            .save(
                Decoded {
                    bytes: fixture(ImageFormat::Png),
                    revised_prompt: None,
                },
                json!({}),
            )
            .unwrap_err();
        assert!(error.to_string().contains("Prompt save failed"));
        assert!(!path.exists());
        assert_eq!(fs::read(&sidecar).unwrap(), b"keep");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        assert!(!dir.path().join("image.image-generation.lock").exists());
    }

    #[test]
    fn fails_to_save_when_final_path_appears_after_preflight() {
        let dir = tempfile::tempdir().unwrap();
        let requested = dir.path().join("image.png");
        let destination = Destination::prepare(&requested).unwrap();
        let final_path = dir.path().join("image.jpg");
        fs::write(&final_path, b"keep").unwrap();
        let error = destination
            .save(
                Decoded {
                    bytes: fixture(ImageFormat::Jpeg),
                    revised_prompt: None,
                },
                json!({}),
            )
            .unwrap_err();
        assert!(error.to_string().contains("Cannot save image"));
        assert_eq!(fs::read(&final_path).unwrap(), b"keep");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        assert!(!dir.path().join("image.image-generation.lock").exists());
    }

    #[test]
    fn rejects_provider_images_exceeding_safety_limits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        let oversized = image::RgbImage::from_pixel(16385, 1, image::Rgb([1, 2, 3]));
        let mut bytes = Cursor::new(Vec::new());
        oversized.write_to(&mut bytes, ImageFormat::Png).unwrap();
        let bytes = bytes.into_inner();
        let error = Destination::prepare(&path)
            .unwrap()
            .save(
                Decoded {
                    bytes,
                    revised_prompt: None,
                },
                json!({}),
            )
            .unwrap_err();
        assert!(error.to_string().contains("invalid image"));
        assert!(!path.exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn rejects_non_absolute_and_unsupported_extensions() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Destination::prepare(Path::new("relative.png")).is_err());
        assert!(Destination::prepare(&dir.path().join("image.gif")).is_err());
    }

    #[test]
    fn rejects_unsupported_provider_formats() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        let error = Destination::prepare(&path)
            .unwrap()
            .save(
                Decoded {
                    bytes: b"GIF89a".to_vec(),
                    revised_prompt: None,
                },
                json!({}),
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Unsupported provider image format")
        );
        assert!(!path.exists());
    }
}
