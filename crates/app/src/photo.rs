//! 매물 사진 검증과 위치 메타데이터 제거.
//!
//! 원본 픽셀은 다시 인코딩하지 않는다. JPEG는 EXIF의 GPS IFD만 지워 방향 정보를 남기고 XMP는 통째로 뺀다.
//! PNG는 eXIf 청크와 XMP 텍스트 청크를 뺀다. GIF에는 위치 메타데이터 규격이 없다.

use std::io::Cursor;
use std::sync::Arc;

use base64::Engine;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use rust_i18n::t;
use sha2::{Digest, Sha256};

pub const MAX_BYTES: u64 = 20 * 1024 * 1024;
pub const MAX_PER_LISTING: usize = 10;
pub const ACCEPT: &str = ".png,.jpg,.jpeg,.gif";
const MAX_DIMENSION: u32 = 12_000;
const MAX_DECODE_ALLOC: u64 = 512 * 1024 * 1024;
const THUMBNAIL_SIZE: u32 = 320;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhotoKind {
    Png,
    Jpeg,
    Gif,
}

impl PhotoKind {
    pub fn extension(self) -> &'static str {
        match self {
            PhotoKind::Png => "png",
            PhotoKind::Jpeg => "jpg",
            PhotoKind::Gif => "gif",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            PhotoKind::Png => "image/png",
            PhotoKind::Jpeg => "image/jpeg",
            PhotoKind::Gif => "image/gif",
        }
    }

    pub fn from_mime(mime: &str) -> Option<Self> {
        [PhotoKind::Png, PhotoKind::Jpeg, PhotoKind::Gif]
            .into_iter()
            .find(|kind| kind.mime() == mime)
    }

    fn image_format(self) -> ImageFormat {
        match self {
            PhotoKind::Png => ImageFormat::Png,
            PhotoKind::Jpeg => ImageFormat::Jpeg,
            PhotoKind::Gif => ImageFormat::Gif,
        }
    }
}

/// 저장할 준비가 끝난 사진. 위치 메타데이터는 이미 제거했다.
#[derive(Debug, PartialEq)]
pub struct PreparedPhoto {
    pub id: String,
    pub kind: PhotoKind,
    pub bytes: Vec<u8>,
    /// `bytes`의 SHA-256 소문자 hex.
    pub sha256: String,
    /// 방향 보정 후 크기.
    pub width: u32,
    pub height: u32,
    pub thumbnail: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PhotoError {
    #[error("{}", t!("photo_error.too_large", max_mb = MAX_BYTES / 1024 / 1024))]
    TooLarge,
    #[error("{}", t!("photo_error.unsupported_format"))]
    UnsupportedFormat,
    #[error("{}", t!("photo_error.undecodable", reason = .0))]
    Undecodable(String),
    #[error("{}", t!("photo_error.read_failed", reason = .0))]
    ReadFailed(String),
    #[error("{}", t!("error.internal", reason = .0))]
    Internal(String),
}

/// 형식·실제 디코딩·크기·해상도를 확인하고 위치 메타데이터를 지운다. CPU를 쓰므로 UI 스레드 밖에서 부른다.
pub fn prepare(bytes: &[u8]) -> Result<PreparedPhoto, PhotoError> {
    if bytes.len() as u64 > MAX_BYTES {
        return Err(PhotoError::TooLarge);
    }
    let kind = match image::guess_format(bytes) {
        Ok(ImageFormat::Png) => PhotoKind::Png,
        Ok(ImageFormat::Jpeg) => PhotoKind::Jpeg,
        Ok(ImageFormat::Gif) => PhotoKind::Gif,
        _ => return Err(PhotoError::UnsupportedFormat),
    };
    let stripped = match kind {
        PhotoKind::Jpeg => strip_jpeg(bytes)?,
        PhotoKind::Png => strip_png(bytes)?,
        PhotoKind::Gif => bytes.to_vec(),
    };
    let image = decode(&stripped, kind)?;
    let thumbnail = thumbnail_data_url(&image)?;
    Ok(PreparedPhoto {
        id: uuid::Uuid::now_v7().to_string(),
        kind,
        sha256: sha256_hex(&stripped),
        width: image.width(),
        height: image.height(),
        bytes: stripped,
        thumbnail,
    })
}

/// 저장된 사진 파일에서 표시용 썸네일 data URL을 만든다.
pub async fn thumbnail_from_file(
    path: std::path::PathBuf,
    kind: PhotoKind,
) -> Result<String, PhotoError> {
    tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(&path).map_err(|e| PhotoError::ReadFailed(e.to_string()))?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(PhotoError::TooLarge);
        }
        thumbnail_data_url(&decode(&bytes, kind)?)
    })
    .await
    .map_err(|e| PhotoError::Internal(e.to_string()))?
}

pub async fn prepare_async(bytes: Vec<u8>) -> Result<Arc<PreparedPhoto>, PhotoError> {
    tokio::task::spawn_blocking(move || prepare(&bytes).map(Arc::new))
        .await
        .map_err(|e| PhotoError::Internal(e.to_string()))?
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn decode(bytes: &[u8], kind: PhotoKind) -> Result<DynamicImage, PhotoError> {
    let undecodable = |e: image::ImageError| PhotoError::Undecodable(e.to_string());
    let mut reader = ImageReader::with_format(Cursor::new(bytes), kind.image_format());
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(undecodable)?;
    let orientation = decoder.orientation().map_err(undecodable)?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(undecodable)?;
    image.apply_orientation(orientation);
    Ok(image)
}

fn thumbnail_data_url(image: &DynamicImage) -> Result<String, PhotoError> {
    let thumb = DynamicImage::ImageRgb8(image.thumbnail(THUMBNAIL_SIZE, THUMBNAIL_SIZE).to_rgb8());
    let mut out = Cursor::new(Vec::new());
    thumb
        .write_to(&mut out, ImageFormat::Jpeg)
        .map_err(|e| PhotoError::Internal(e.to_string()))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(out.into_inner());
    Ok(format!("data:image/jpeg;base64,{encoded}"))
}

fn malformed() -> PhotoError {
    PhotoError::Undecodable("malformed metadata".to_owned())
}

fn strip_jpeg(src: &[u8]) -> Result<Vec<u8>, PhotoError> {
    if !src.starts_with(&[0xFF, 0xD8]) {
        return Err(malformed());
    }
    let mut out = Vec::with_capacity(src.len());
    out.extend_from_slice(&src[..2]);
    let mut pos = 2;
    loop {
        if src.get(pos) != Some(&0xFF) {
            return Err(malformed());
        }
        let mut marker_pos = pos + 1;
        while src.get(marker_pos) == Some(&0xFF) {
            marker_pos += 1;
        }
        let marker = *src.get(marker_pos).ok_or_else(malformed)?;
        let header_end = marker_pos + 1;
        // SOS 이후는 압축 데이터라 그대로 둔다.
        if marker == 0xDA {
            out.extend_from_slice(&src[pos..]);
            return Ok(out);
        }
        if marker == 0xD9 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            out.extend_from_slice(&src[pos..header_end]);
            if marker == 0xD9 {
                return Ok(out);
            }
            pos = header_end;
            continue;
        }
        let len_bytes = src.get(header_end..header_end + 2).ok_or_else(malformed)?;
        let len = usize::from(u16::from_be_bytes([len_bytes[0], len_bytes[1]]));
        if len < 2 {
            return Err(malformed());
        }
        let end = header_end + len;
        let segment = src.get(pos..end).ok_or_else(malformed)?;
        let payload = &src[header_end + 2..end];
        if marker == 0xE1 && payload.starts_with(b"Exif\0\0") {
            let mut exif = segment.to_vec();
            let tiff_start = segment.len() - payload.len() + 6;
            // 구조를 해석할 수 없는 EXIF는 위치가 남을 수 있으므로 방향 정보를 잃더라도 뺀다.
            if wipe_gps(&mut exif[tiff_start..]).is_some() {
                out.extend_from_slice(&exif);
            }
        } else if marker == 0xE1 && payload.starts_with(b"http://ns.adobe.com/") {
            // XMP에도 위치가 들어갈 수 있다.
        } else {
            out.extend_from_slice(segment);
        }
        pos = end;
    }
}

/// TIFF 구조를 유지한 채 GPS IFD의 항목과 값을 0으로 덮는다.
fn wipe_gps(tiff: &mut [u8]) -> Option<()> {
    let little = match tiff.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |buf: &[u8], at: usize| -> Option<usize> {
        let b: [u8; 2] = buf.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(usize::from(if little {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        }))
    };
    let u32_at = |buf: &[u8], at: usize| -> Option<usize> {
        let b: [u8; 4] = buf.get(at..at.checked_add(4)?)?.try_into().ok()?;
        usize::try_from(if little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
        .ok()
    };

    let ifd0 = u32_at(tiff, 4)?;
    let count = u16_at(tiff, ifd0)?;
    let mut gps = None;
    for i in 0..count {
        let entry = ifd0 + 2 + 12 * i;
        if u16_at(tiff, entry)? == 0x8825 {
            gps = Some(u32_at(tiff, entry + 8)?);
        }
    }
    let Some(gps) = gps else {
        return Some(());
    };

    let count = u16_at(tiff, gps)?;
    let mut ranges = Vec::with_capacity(count + 1);
    ranges.push(gps..gps + 2 + 12 * count);
    for i in 0..count {
        let entry = gps + 2 + 12 * i;
        let unit: usize = match u16_at(tiff, entry + 2)? {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 => 4,
            5 | 10 | 12 => 8,
            _ => return None,
        };
        let size = unit.checked_mul(u32_at(tiff, entry + 4)?)?;
        if size > 4 {
            let at = u32_at(tiff, entry + 8)?;
            ranges.push(at..at.checked_add(size)?);
        }
    }
    // 하나라도 범위를 벗어나면 일부만 지운 채 남기지 않는다.
    if ranges.iter().any(|r| r.end > tiff.len()) {
        return None;
    }
    for range in ranges {
        tiff[range].fill(0);
    }
    Some(())
}

fn strip_png(src: &[u8]) -> Result<Vec<u8>, PhotoError> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if !src.starts_with(SIGNATURE) {
        return Err(malformed());
    }
    let mut out = Vec::with_capacity(src.len());
    out.extend_from_slice(SIGNATURE);
    let mut pos = SIGNATURE.len();
    while pos < src.len() {
        let len_bytes: [u8; 4] = src
            .get(pos..pos + 4)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(malformed)?;
        let len = usize::try_from(u32::from_be_bytes(len_bytes)).map_err(|_| malformed())?;
        let end = pos
            .checked_add(12)
            .and_then(|v| v.checked_add(len))
            .filter(|end| *end <= src.len())
            .ok_or_else(malformed)?;
        let kind = &src[pos + 4..pos + 8];
        let data = &src[pos + 8..pos + 8 + len];
        let drop = kind == b"eXIf" || (kind == b"iTXt" && data.starts_with(b"XML:com.adobe.xmp\0"));
        if !drop {
            out.extend_from_slice(&src[pos..end]);
        }
        pos = end;
        if kind == b"IEND" {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb};

    fn encoded(format: ImageFormat, width: u32, height: u32) -> Vec<u8> {
        let image =
            DynamicImage::ImageRgb8(ImageBuffer::from_pixel(width, height, Rgb([200, 10, 10])));
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    /// 방향(6 = 90도 회전)과 위도 값을 가진 GPS IFD가 있는 big-endian EXIF.
    fn exif_tiff() -> Vec<u8> {
        let mut t = Vec::new();
        t.extend_from_slice(b"MM\0\x2a\0\0\0\x08");
        // IFD0: Orientation, GPS 포인터.
        t.extend_from_slice(&2u16.to_be_bytes());
        t.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0]);
        t.extend_from_slice(&[0x88, 0x25, 0, 4, 0, 0, 0, 1, 0, 0, 0, 38]);
        t.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(t.len(), 38);
        // GPS IFD: GPSLatitude RATIONAL x3, 값은 IFD 뒤 56번지.
        t.extend_from_slice(&1u16.to_be_bytes());
        t.extend_from_slice(&[0x00, 0x02, 0, 5, 0, 0, 0, 3, 0, 0, 0, 56]);
        t.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(t.len(), 56);
        for v in [37u32, 1, 33, 1, 4321, 100] {
            t.extend_from_slice(&v.to_be_bytes());
        }
        t
    }

    fn jpeg_with_exif(tiff: &[u8], xmp: bool) -> Vec<u8> {
        let base = encoded(ImageFormat::Jpeg, 4, 2);
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend_from_slice(tiff);
        let mut out = base[..2].to_vec();
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
        out.extend_from_slice(&payload);
        if xmp {
            let body = b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta exif:GPSLatitude='37'/>";
            out.extend_from_slice(&[0xFF, 0xE1]);
            out.extend_from_slice(&u16::try_from(body.len() + 2).unwrap().to_be_bytes());
            out.extend_from_slice(body);
        }
        out.extend_from_slice(&base[2..]);
        out
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn wipes_jpeg_gps_and_keeps_orientation() {
        let photo = prepare(&jpeg_with_exif(&exif_tiff(), true)).unwrap();
        assert_eq!(photo.kind, PhotoKind::Jpeg);
        assert!(!contains(&photo.bytes, &4321u32.to_be_bytes()));
        assert!(!contains(&photo.bytes, b"GPSLatitude"));
        assert!(contains(&photo.bytes, b"Exif\0\0"));
        // 방향이 남아 있으므로 4x2 원본이 2x4로 보정된다.
        assert_eq!((photo.width, photo.height), (2, 4));
        assert_eq!(photo.sha256, sha256_hex(&photo.bytes));
        assert!(photo.thumbnail.starts_with("data:image/jpeg;base64,"));
    }

    #[test]
    fn drops_unparseable_exif() {
        let mut tiff = exif_tiff();
        // GPS 값 위치를 파일 밖으로 돌린다.
        tiff[48..52].copy_from_slice(&9999u32.to_be_bytes());
        let photo = prepare(&jpeg_with_exif(&tiff, false)).unwrap();
        assert!(!contains(&photo.bytes, b"Exif\0\0"));
        assert!(!contains(&photo.bytes, &4321u32.to_be_bytes()));
    }

    #[test]
    fn drops_png_exif_chunk() {
        let base = encoded(ImageFormat::Png, 3, 3);
        let mut chunk = 4u32.to_be_bytes().to_vec();
        chunk.extend_from_slice(b"eXIfGPS!");
        chunk.extend_from_slice(&0u32.to_be_bytes());
        // 시그니처(8)와 IHDR(25) 뒤에 넣는다.
        let mut png = base[..33].to_vec();
        png.extend_from_slice(&chunk);
        png.extend_from_slice(&base[33..]);
        let photo = prepare(&png).unwrap();
        assert_eq!(photo.bytes, base);
        assert_eq!((photo.width, photo.height), (3, 3));
    }

    #[test]
    fn accepts_gif_and_rejects_other_formats() {
        assert_eq!(
            prepare(&encoded(ImageFormat::Gif, 2, 2)).unwrap().kind,
            PhotoKind::Gif
        );
        assert!(matches!(
            prepare(&encoded(ImageFormat::Bmp, 2, 2)),
            Err(PhotoError::UnsupportedFormat)
        ));
        assert!(matches!(
            prepare(b"not an image"),
            Err(PhotoError::UnsupportedFormat)
        ));
    }

    #[test]
    fn rejects_truncated_and_oversized() {
        let jpeg = encoded(ImageFormat::Jpeg, 8, 8);
        assert!(prepare(&jpeg[..jpeg.len() / 2]).is_err());
        let huge = vec![0u8; usize::try_from(MAX_BYTES).unwrap() + 1];
        assert!(matches!(prepare(&huge), Err(PhotoError::TooLarge)));
    }
}
