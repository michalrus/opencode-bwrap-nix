use std::{
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use image::{GenericImageView, ImageFormat, ImageReader, imageops::FilterType};
use serde_json::{Value, json};
use tempfile::{Builder, NamedTempFile, TempPath};

pub struct Destination {
    path: PathBuf,
    sidecar: PathBuf,
    format: ImageFormat,
    image_file: NamedTempFile,
    original_file: NamedTempFile,
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

fn original_path(path: &Path, format: ImageFormat) -> Result<PathBuf> {
    let extension = match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpg",
        ImageFormat::WebP => "webp",
        _ => bail!("Unsupported provider image format"),
    };
    let mut original = path.as_os_str().to_os_string();
    original.push(format!(".original.{extension}"));
    Ok(PathBuf::from(original))
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
        let format = match path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("png") => ImageFormat::Png,
            Some("jpg" | "jpeg") => ImageFormat::Jpeg,
            Some("webp") => ImageFormat::WebP,
            _ => bail!("local_path must end in .png, .jpg, .jpeg, or .webp"),
        };
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(".prompt.json");
        let sidecar = PathBuf::from(sidecar);
        require_absent(&path)?;
        require_absent(&sidecar)?;
        for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP] {
            require_absent(&original_path(&path, format)?)?;
        }
        let mut lock = path.as_os_str().to_os_string();
        lock.push(".image-generation.lock");
        let lock = PathBuf::from(lock);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .context("Cannot reserve output path. Another generation may be in progress.")?;
        let reservation = TempPath::try_from_path(lock)?;
        let image_file = Builder::new()
            .prefix(".image-generation-")
            .tempfile_in(&parent)?;
        let original_file = Builder::new()
            .prefix(".image-original-")
            .tempfile_in(&parent)?;
        let prompt_file = Builder::new()
            .prefix(".image-prompt-")
            .tempfile_in(&parent)?;
        Ok(Self {
            path,
            sidecar,
            format,
            image_file,
            original_file,
            prompt_file,
            _reservation: reservation,
        })
    }

    pub fn save(
        mut self,
        decoded: Decoded,
        width: u32,
        height: u32,
        mut metadata: Value,
    ) -> Result<Value> {
        ensure!(
            (1..=4096).contains(&width) && (1..=4096).contains(&height),
            "Output dimensions must be between 1 and 4096 pixels"
        );
        let source_format = image::guess_format(&decoded.bytes).context("Unknown image format")?;
        let original_path = original_path(&self.path, source_format)?;
        let mut reader = ImageReader::with_format(Cursor::new(&decoded.bytes), source_format);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(16384);
        limits.max_image_height = Some(16384);
        limits.max_alloc = Some(512 * 1024 * 1024);
        reader.limits(limits);
        let mut image = reader
            .decode()
            .context("Provider returned an invalid image")?;
        let source_dimensions = image.dimensions();
        let (source_width, source_height) = source_dimensions;
        ensure!(
            source_width > 0 && source_height > 0,
            "Empty provider image"
        );
        let resized = source_dimensions != (width, height);
        if resized {
            let (crop_width, crop_height) = if u64::from(source_width) * u64::from(height)
                > u64::from(source_height) * u64::from(width)
            {
                (
                    ((u64::from(source_height) * u64::from(width)) / u64::from(height)).max(1)
                        as u32,
                    source_height,
                )
            } else {
                (
                    source_width,
                    ((u64::from(source_width) * u64::from(height)) / u64::from(width)).max(1)
                        as u32,
                )
            };
            if (crop_width, crop_height) != source_dimensions {
                image = image.crop_imm(
                    (source_width - crop_width) / 2,
                    (source_height - crop_height) / 2,
                    crop_width,
                    crop_height,
                );
            }
            image = image.resize_exact(width, height, FilterType::Lanczos3);
        }
        let converted = source_format != self.format;
        if !resized && !converted {
            self.image_file.write_all(&decoded.bytes)?;
        } else if self.format == ImageFormat::Jpeg {
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut self.image_file, 95)
                .encode_image(&image.to_rgb8())?;
        } else {
            image.write_to(&mut self.image_file, self.format)?;
        }
        self.original_file.write_all(&decoded.bytes)?;
        let mime = self.format.to_mime_type();
        metadata["revised_prompt"] = json!(decoded.revised_prompt);
        metadata["original"] = json!({
            "path": original_path,
            "mime_type": source_format.to_mime_type(),
            "width": source_width, "height": source_height,
            "bytes": decoded.bytes.len(),
        });
        metadata["output"] = json!({
            "path": self.path, "mime_type": mime, "width": width, "height": height,
            "source_width": source_dimensions.0, "source_height": source_dimensions.1,
            "source_mime_type": source_format.to_mime_type(),
            "resized": resized, "converted": converted,
            "resize_method": if resized { Some("lanczos3_center_crop") } else { None },
        });
        serde_json::to_writer_pretty(&mut self.prompt_file, &metadata)?;
        self.prompt_file.write_all(b"\n")?;
        self.image_file.as_file().sync_all()?;
        self.original_file.as_file().sync_all()?;
        self.prompt_file.as_file().sync_all()?;
        let bytes = self.image_file.as_file().metadata()?.len();
        let Self {
            path,
            sidecar,
            image_file,
            original_file,
            prompt_file,
            _reservation,
            ..
        } = self;
        let saved_image = image_file
            .persist_noclobber(&path)
            .map_err(|error| error.error)
            .context("Cannot save image without overwriting")?;
        let saved_original = match original_file
            .persist_noclobber(&original_path)
            .map_err(|error| error.error)
        {
            Ok(file) => file,
            Err(error) => {
                drop(saved_image);
                fs::remove_file(&path).context("Original save failed and image rollback failed")?;
                return Err(error).context("Original save failed; the partial image was removed");
            }
        };
        if let Err(error) = prompt_file
            .persist_noclobber(&sidecar)
            .map_err(|error| error.error)
        {
            drop(saved_image);
            drop(saved_original);
            let image_cleanup = fs::remove_file(&path);
            let original_cleanup = fs::remove_file(&original_path);
            image_cleanup.context("Prompt save failed and image rollback failed")?;
            original_cleanup.context("Prompt save failed and original rollback failed")?;
            return Err(error)
                .context("Prompt save failed; the partial image and original were removed");
        }
        Ok(json!({
            "path": path, "original_path": original_path, "prompt_file": sidecar, "width": width, "height": height,
            "mime_type": mime, "bytes": bytes, "model": metadata["request"]["model"],
            "source_width": source_dimensions.0, "source_height": source_dimensions.1,
            "resized": resized, "converted": converted,
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
    fn converts_resizes_and_saves_exact_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        let destination = Destination::prepare(&path).unwrap();
        assert!(Destination::prepare(&path).is_err());
        let original = fixture(ImageFormat::Jpeg);
        let result = destination.save(Decoded { bytes: original.clone(), revised_prompt: None }, 4, 4,
            json!({"request": {"model": "gemini-image", "messages": [{"content": "exact prompt"}]}})).unwrap();
        assert_eq!(image::open(&path).unwrap().dimensions(), (4, 4));
        assert_eq!(result["converted"], true);
        let original_path = dir.path().join("image.png.original.jpg");
        assert_eq!(result["original_path"], original_path.to_str().unwrap());
        assert_eq!(fs::read(&original_path).unwrap(), original);
        assert_eq!(image::open(&original_path).unwrap().dimensions(), (8, 6));
        let metadata: Value =
            serde_json::from_slice(&fs::read(result["prompt_file"].as_str().unwrap()).unwrap())
                .unwrap();
        assert_eq!(
            metadata["request"]["messages"][0]["content"],
            "exact prompt"
        );
        assert_eq!(metadata["original"]["path"], result["original_path"]);
        assert_eq!(metadata["original"]["mime_type"], "image/jpeg");
        assert_eq!(metadata["original"]["width"], 8);
        assert_eq!(metadata["original"]["height"], 6);
        assert_eq!(metadata["original"]["bytes"], original.len());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);
        assert!(Destination::prepare(&path).is_err());
        assert!(!dir.path().join("image.png.image-generation.lock").exists());
    }

    #[test]
    fn preserves_native_bytes_and_converts_all_extensions() {
        for (source_extension, source_format) in [
            ("png", ImageFormat::Png),
            ("jpg", ImageFormat::Jpeg),
            ("webp", ImageFormat::WebP),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let bytes = fixture(source_format);
            for (extension, format) in [
                ("png", ImageFormat::Png),
                ("jpg", ImageFormat::Jpeg),
                ("jpeg", ImageFormat::Jpeg),
                ("webp", ImageFormat::WebP),
            ] {
                let path = dir.path().join(format!("image.{extension}"));
                let result = Destination::prepare(&path)
                    .unwrap()
                    .save(
                        Decoded {
                            bytes: bytes.clone(),
                            revised_prompt: None,
                        },
                        8,
                        6,
                        json!({}),
                    )
                    .unwrap();
                let saved = fs::read(&path).unwrap();
                let original_path = dir
                    .path()
                    .join(format!("image.{extension}.original.{source_extension}"));
                assert_eq!(result["original_path"], original_path.to_str().unwrap());
                assert_eq!(fs::read(&original_path).unwrap(), bytes);
                let metadata: Value = serde_json::from_slice(
                    &fs::read(result["prompt_file"].as_str().unwrap()).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    metadata["original"],
                    json!({
                        "path": original_path,
                        "mime_type": source_format.to_mime_type(),
                        "width": 8, "height": 6, "bytes": bytes.len(),
                    })
                );
                assert_eq!(image::guess_format(&saved).unwrap(), format);
                assert_eq!(image::open(&path).unwrap().dimensions(), (8, 6));
                assert_eq!(result["resized"], false);
                assert_eq!(result["converted"], source_format != format);
                if format == source_format {
                    assert_eq!(saved, bytes);
                }
            }
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 12);
        }
    }

    #[test]
    fn refuses_existing_originals_before_generation() {
        for extension in ["png", "jpg", "webp"] {
            for symlink in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("image.png");
                let original_path = dir.path().join(format!("image.png.original.{extension}"));
                if symlink {
                    std::os::unix::fs::symlink(dir.path().join("missing"), &original_path).unwrap();
                } else {
                    fs::write(&original_path, b"keep").unwrap();
                }
                let error = Destination::prepare(&path).err().unwrap();
                assert!(error.to_string().contains("already exists"));
                if symlink {
                    assert_eq!(
                        fs::read_link(&original_path).unwrap(),
                        dir.path().join("missing")
                    );
                } else {
                    assert_eq!(fs::read(&original_path).unwrap(), b"keep");
                }
                assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
            }
        }
    }

    #[test]
    fn rolls_back_and_preserves_paths_taken_during_generation() {
        for (suffix, expected_error) in [
            ("", "Cannot save image without overwriting"),
            (".original.jpg", "Original save failed"),
            (".prompt.json", "Prompt save failed"),
        ] {
            for kind in ["file", "symlink", "directory"] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("image.jpg");
                let conflict = dir.path().join(format!("image.jpg{suffix}"));
                let destination = Destination::prepare(&path).unwrap();
                match kind {
                    "symlink" => {
                        std::os::unix::fs::symlink(dir.path().join("missing"), &conflict).unwrap();
                    }
                    "directory" => fs::create_dir(&conflict).unwrap(),
                    _ => fs::write(&conflict, b"keep").unwrap(),
                }
                let error = destination
                    .save(
                        Decoded {
                            bytes: fixture(ImageFormat::Jpeg),
                            revised_prompt: None,
                        },
                        8,
                        6,
                        json!({}),
                    )
                    .unwrap_err();
                assert!(error.to_string().contains(expected_error), "{error:#}");
                match kind {
                    "symlink" => {
                        assert_eq!(
                            fs::read_link(&conflict).unwrap(),
                            dir.path().join("missing")
                        );
                    }
                    "directory" => assert!(conflict.is_dir()),
                    _ => assert_eq!(fs::read(&conflict).unwrap(), b"keep"),
                }
                let remaining: Vec<_> = fs::read_dir(dir.path())
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .collect();
                assert_eq!(remaining, [conflict]);
            }
        }
    }

    #[test]
    fn drops_reservations_and_preserves_competing_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        drop(Destination::prepare(&path).unwrap());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(
            Destination::prepare(&path)
                .unwrap()
                .save(
                    Decoded {
                        bytes: b"invalid".to_vec(),
                        revised_prompt: None
                    },
                    8,
                    6,
                    json!({})
                )
                .is_err()
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        let destination = Destination::prepare(&path).unwrap();
        fs::write(&path, b"keep").unwrap();
        assert!(
            destination
                .save(
                    Decoded {
                        bytes: fixture(ImageFormat::Png),
                        revised_prompt: None
                    },
                    8,
                    6,
                    json!({})
                )
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"keep");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn crops_thin_source_before_resizing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        let mut source = image::RgbImage::from_pixel(16384, 1, image::Rgb([255, 0, 0]));
        source.put_pixel(8191, 0, image::Rgb([0, 255, 0]));
        let mut bytes = Cursor::new(Vec::new());
        source.write_to(&mut bytes, ImageFormat::Png).unwrap();
        let bytes = bytes.into_inner();
        let saved = Destination::prepare(&path)
            .unwrap()
            .save(
                Decoded {
                    bytes: bytes.clone(),
                    revised_prompt: None,
                },
                64,
                64,
                json!({}),
            )
            .unwrap();
        let original_path = saved["original_path"].as_str().unwrap();
        assert_eq!(fs::read(original_path).unwrap(), bytes);
        assert_eq!(image::open(original_path).unwrap().to_rgb8(), source);
        let result = image::open(&path).unwrap().to_rgb8();
        assert_eq!(result.dimensions(), (64, 64));
        assert!(
            result
                .pixels()
                .all(|pixel| *pixel == image::Rgb([0, 255, 0]))
        );
    }

    #[test]
    fn refuses_symlinks_and_cleans_up_failed_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        std::os::unix::fs::symlink(dir.path().join("missing"), &path).unwrap();
        assert!(Destination::prepare(&path).is_err());
        fs::remove_file(&path).unwrap();
        let destination = Destination::prepare(&path).unwrap();
        fs::write(dir.path().join("image.png.prompt.json"), b"keep").unwrap();
        assert!(
            destination
                .save(
                    Decoded {
                        bytes: fixture(ImageFormat::Png),
                        revised_prompt: None
                    },
                    8,
                    6,
                    json!({})
                )
                .is_err()
        );
        assert!(!path.exists());
        assert_eq!(
            fs::read(dir.path().join("image.png.prompt.json")).unwrap(),
            b"keep"
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
