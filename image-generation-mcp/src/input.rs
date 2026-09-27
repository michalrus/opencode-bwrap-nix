use std::{
    fs::{self, File},
    io::{Cursor, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use image::{GenericImageView, ImageFormat, ImageReader};
use serde_json::{Value, json};

const MAX_IMAGE_BYTES: u64 = 50_000_000;
const MAX_MASK_BYTES: u64 = 4_000_000;

pub struct InputImage {
    path: PathBuf,
    bytes: Vec<u8>,
    format: ImageFormat,
    pub width: u32,
    pub height: u32,
}

impl InputImage {
    pub fn load(path: &Path, source: Option<&Self>) -> Result<Self> {
        ensure!(path.is_absolute(), "Input image paths must be absolute");
        let path = path
            .canonicalize()
            .context("Cannot resolve input image path")?;
        let limit = if source.is_some() {
            MAX_MASK_BYTES
        } else {
            MAX_IMAGE_BYTES
        };
        let metadata = fs::metadata(&path).context("Cannot inspect input image")?;
        ensure!(metadata.is_file(), "Input image must be a regular file");
        ensure!(
            metadata.len() > 0 && metadata.len() < limit,
            "Input image must contain fewer than {limit} bytes and must not be empty"
        );
        let file = File::open(&path).context("Cannot open input image")?;
        ensure!(
            file.metadata()?.is_file(),
            "Input image must be a regular file"
        );
        let mut bytes = Vec::new();
        file.take(limit)
            .read_to_end(&mut bytes)
            .context("Cannot read input image")?;
        ensure!(
            !bytes.is_empty() && (bytes.len() as u64) < limit,
            "Input image must contain fewer than {limit} bytes and must not be empty"
        );
        let format = image::guess_format(&bytes).context("Unknown input image format")?;
        ensure!(
            matches!(
                format,
                ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
            ),
            "Input image must be PNG, JPEG, or WebP"
        );
        if source.is_some() {
            ensure!(format == ImageFormat::Png, "Mask must be a PNG image");
        }
        let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(16384);
        limits.max_image_height = Some(16384);
        limits.max_alloc = Some(512 * 1024 * 1024);
        reader.limits(limits);
        let image = reader
            .decode()
            .context("Invalid input image or image exceeds decoding limits")?;
        let (width, height) = image.dimensions();
        ensure!(width > 0 && height > 0, "Input image must not be empty");
        if let Some(source) = source {
            ensure!(
                (width, height) == (source.width, source.height),
                "Mask dimensions must match the input image"
            );
            ensure!(
                image.color().has_alpha(),
                "Mask must contain an alpha channel"
            );
            let transparent = match &image {
                image::DynamicImage::ImageLumaA8(image) => {
                    image.pixels().any(|pixel| pixel[1] == 0)
                }
                image::DynamicImage::ImageLumaA16(image) => {
                    image.pixels().any(|pixel| pixel[1] == 0)
                }
                image::DynamicImage::ImageRgba8(image) => image.pixels().any(|pixel| pixel[3] == 0),
                image::DynamicImage::ImageRgba16(image) => {
                    image.pixels().any(|pixel| pixel[3] == 0)
                }
                _ => false,
            };
            ensure!(
                transparent,
                "Mask must contain fully transparent pixels to mark the area to edit"
            );
        }
        Ok(Self {
            path,
            bytes,
            format,
            width,
            height,
        })
    }

    pub fn data_url(&self) -> String {
        format!(
            "data:{};base64,{}",
            self.format.to_mime_type(),
            STANDARD.encode(&self.bytes)
        )
    }

    pub fn metadata(&self) -> Value {
        json!({
            "path": self.path, "mime_type": self.format.to_mime_type(),
            "width": self.width, "height": self.height, "bytes": self.bytes.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_exact_bytes_and_detects_format_from_content() {
        let directory = tempfile::tempdir().unwrap();
        for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP] {
            let path = directory.path().join("source.dat");
            let bytes = crate::output::tests::fixture(format);
            fs::write(&path, &bytes).unwrap();
            let input = InputImage::load(&path, None).unwrap();
            assert_eq!(input.bytes, bytes);
            assert_eq!(input.format, format);
            assert_eq!((input.width, input.height), (8, 6));
            assert_eq!(
                input.data_url(),
                format!(
                    "data:{};base64,{}",
                    format.to_mime_type(),
                    STANDARD.encode(&bytes)
                )
            );
            assert_eq!(
                input.metadata(),
                json!({
                    "path": path, "mime_type": format.to_mime_type(), "width": 8, "height": 6, "bytes": bytes.len(),
                })
            );
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
        let link = directory.path().join("source-link");
        std::os::unix::fs::symlink(directory.path().join("source.dat"), &link).unwrap();
        assert_eq!(
            InputImage::load(&link, None).unwrap().path,
            directory.path().join("source.dat")
        );
    }

    #[test]
    fn rejects_invalid_and_oversized_files_before_decoding() {
        let directory = tempfile::tempdir().unwrap();
        assert!(InputImage::load(Path::new("relative.png"), None).is_err());
        assert!(InputImage::load(directory.path(), None).is_err());
        assert!(InputImage::load(&directory.path().join("missing"), None).is_err());
        assert!(InputImage::load(Path::new("/dev/null"), None).is_err());
        let path = directory.path().join("image.png");
        for bytes in [b"".as_slice(), b"invalid", b"GIF89a", b"\x89PNG\r\n\x1a\n"] {
            fs::write(&path, bytes).unwrap();
            assert!(InputImage::load(&path, None).is_err());
        }
        File::create(&path)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES)
            .unwrap();
        assert!(
            InputImage::load(&path, None)
                .err()
                .unwrap()
                .to_string()
                .contains("50000000 bytes")
        );
        image::RgbImage::new(16385, 1).save(&path).unwrap();
        assert!(InputImage::load(&path, None).is_err());
    }

    #[test]
    fn validates_mask_format_dimensions_alpha_and_transparency() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.jpg");
        fs::write(
            &source_path,
            crate::output::tests::fixture(ImageFormat::Jpeg),
        )
        .unwrap();
        let source = InputImage::load(&source_path, None).unwrap();
        let path = directory.path().join("mask.png");
        fs::write(&path, crate::output::tests::fixture(ImageFormat::Jpeg)).unwrap();
        assert!(
            InputImage::load(&path, Some(&source))
                .err()
                .unwrap()
                .to_string()
                .contains("PNG")
        );
        image::RgbaImage::new(4, 4).save(&path).unwrap();
        assert!(
            InputImage::load(&path, Some(&source))
                .err()
                .unwrap()
                .to_string()
                .contains("dimensions")
        );
        image::RgbImage::new(8, 6).save(&path).unwrap();
        assert!(
            InputImage::load(&path, Some(&source))
                .err()
                .unwrap()
                .to_string()
                .contains("alpha channel")
        );
        for alpha in [1, 255] {
            image::RgbaImage::from_pixel(8, 6, image::Rgba([0, 0, 0, alpha]))
                .save(&path)
                .unwrap();
            assert!(
                InputImage::load(&path, Some(&source))
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("fully transparent")
            );
        }
        let mut mask = image::RgbaImage::from_pixel(8, 6, image::Rgba([0, 0, 0, 255]));
        mask.put_pixel(2, 3, image::Rgba([255, 255, 255, 0]));
        mask.save(&path).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(InputImage::load(&path, Some(&source)).unwrap().bytes, bytes);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        File::create(&path)
            .unwrap()
            .set_len(MAX_MASK_BYTES)
            .unwrap();
        assert!(
            InputImage::load(&path, Some(&source))
                .err()
                .unwrap()
                .to_string()
                .contains("4000000 bytes")
        );
    }
}
