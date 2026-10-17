//! 显式附图与受控工具截图；只在内存中保留，不把图片编码当成文字或写进会话。

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use super::{Message, Role, ThinkError};

pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_IMAGES: usize = 8;
pub const MAX_TOTAL_IMAGE_BYTES: usize = 16 * 1024 * 1024;
pub const IMAGE_NOTICE: &str = "【附图说明：原图仅在当前进程中保留；恢复会话后若无图片内容块，须重新附图，不能凭历史描述声称再次看过原图。】";

#[derive(Clone, PartialEq, Eq)]
pub struct ImageInput {
    data_url: Arc<str>,
    bytes: usize,
    mime: &'static str,
}

impl std::fmt::Debug for ImageInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageInput")
            .field("mime", &self.mime)
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

impl ImageInput {
    pub fn from_path(path: &Path) -> Result<Self, ThinkError> {
        let meta =
            std::fs::metadata(path).map_err(|e| invalid(format!("无法读取图片文件：{e}")))?;
        if !meta.is_file() || meta.len() > MAX_IMAGE_BYTES as u64 {
            return Err(invalid("图片必须是普通文件，且单张不超过 8 MiB"));
        }
        let file =
            std::fs::File::open(path).map_err(|e| invalid(format!("无法打开图片文件：{e}")))?;
        let mut bytes = Vec::new();
        file.take(MAX_IMAGE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| invalid(format!("无法读取图片：{e}")))?;
        Self::from_bytes(&bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ThinkError> {
        if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
            return Err(invalid("图片不能为空，且单张不超过 8 MiB"));
        }
        // 只核对文件签名，不在本机实现图片解码器；完整解码由提供方校验。
        let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            "image/png"
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            "image/jpeg"
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            "image/gif"
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            "image/webp"
        } else {
            return Err(invalid("图片文件签名不支持；可用 PNG、JPEG、GIF、WebP"));
        };
        let data_url = format!("data:{mime};base64,{}", base64(bytes));
        Ok(Self {
            data_url: Arc::from(data_url),
            bytes: bytes.len(),
            mime,
        })
    }

    pub fn byte_len(&self) -> usize {
        self.bytes
    }

    /// MCP 图片块使用标准 base64；先限长，再验证填充和尾部位，错误不回显原文。
    pub fn from_base64(encoded: &str) -> Result<Self, ThinkError> {
        if encoded.is_empty()
            || encoded.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4
            || !encoded.len().is_multiple_of(4)
        {
            return Err(invalid("工具图片编码长度非法"));
        }
        let mut bytes = Vec::with_capacity(encoded.len() / 4 * 3);
        for (i, block) in encoded.as_bytes().as_chunks::<4>().0.iter().enumerate() {
            let last = i == encoded.len() / 4 - 1;
            let n = block.iter().take_while(|&&c| c != b'=').count();
            if n < 2 || (n < 4 && !last) || block[n..].iter().any(|&c| c != b'=') {
                return Err(invalid("工具图片编码填充非法"));
            }
            let mut v = [0u8; 4];
            for j in 0..n {
                v[j] = match block[j] {
                    b'A'..=b'Z' => block[j] - b'A',
                    b'a'..=b'z' => block[j] - b'a' + 26,
                    b'0'..=b'9' => block[j] - b'0' + 52,
                    b'+' => 62,
                    b'/' => 63,
                    _ => return Err(invalid("工具图片编码含非法字符")),
                };
            }
            if (n == 2 && v[1] & 15 != 0) || (n == 3 && v[2] & 3 != 0) {
                return Err(invalid("工具图片编码尾部非法"));
            }
            bytes.push((v[0] << 2) | (v[1] >> 4));
            if n > 2 {
                bytes.push((v[1] << 4) | (v[2] >> 2));
            }
            if n > 3 {
                bytes.push((v[2] << 6) | v[3]);
            }
        }
        Self::from_bytes(&bytes)
    }

    pub(super) fn wire_block(&self) -> serde_json::Value {
        serde_json::json!({"type": "image_url", "image_url": {"url": &*self.data_url}})
    }
}

pub(super) fn invalid(detail: impl Into<String>) -> ThinkError {
    ThinkError::BadRequest {
        status: 0,
        detail: detail.into(),
    }
}

pub(super) fn validate_messages<'a>(
    messages: impl IntoIterator<Item = &'a Message>,
) -> Result<(), ThinkError> {
    let mut count = 0;
    let mut bytes = 0;
    for message in messages {
        if !message.images.is_empty() && message.role != Role::User {
            return Err(invalid("图片仅允许出现在 user 消息中"));
        }
        count += message.images.len();
        bytes += message
            .images
            .iter()
            .map(|image| image.bytes)
            .sum::<usize>();
        if count > MAX_IMAGES || bytes > MAX_TOTAL_IMAGE_BYTES {
            return Err(invalid(
                "图片上下文最多 8 张、合计 16 MiB；请减少图片或新开会话",
            ));
        }
    }
    Ok(())
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[(a >> 2) as usize] as char);
        out.push(TABLE[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_base64_is_bounded_and_requires_canonical_encoding() {
        let data = b"GIF89a";
        assert_eq!(
            ImageInput::from_base64(&base64(data)).unwrap(),
            ImageInput::from_bytes(data).unwrap()
        );
        for bad in [
            "",
            "R0lGODlh!",
            "R0lGODlh\n",
            "Zg==AAAA",
            "Zh==",
            "Zm9=",
            "====",
            "R0lGODl_",
        ] {
            assert!(ImageInput::from_base64(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn base64_matches_standard_vectors_including_binary_and_padding() {
        for (input, expected) in [
            (&b""[..], ""),
            (&b"f"[..], "Zg=="),
            (&b"fo"[..], "Zm8="),
            (&b"foo"[..], "Zm9v"),
            (&b"foobar"[..], "Zm9vYmFy"),
            (&b"\x00\xff\x80\x01"[..], "AP+AAQ=="),
        ] {
            assert_eq!(base64(input), expected);
        }
    }

    #[test]
    fn actual_signature_selects_mime_and_debug_does_not_expose_data() {
        for (bytes, mime) in [
            (&b"\x89PNG\r\n\x1a\n"[..], "image/png"),
            (&b"\xff\xd8\xff"[..], "image/jpeg"),
            (&b"GIF89a"[..], "image/gif"),
            (&b"RIFF0000WEBP"[..], "image/webp"),
        ] {
            let image = ImageInput::from_bytes(bytes).unwrap();
            assert_eq!(image.mime, mime);
            assert!(!format!("{image:?}").contains(&*image.data_url));
        }
    }

    #[test]
    fn empty_unknown_and_oversized_images_are_rejected() {
        for bytes in [
            vec![],
            b"not an image".to_vec(),
            vec![0; MAX_IMAGE_BYTES + 1],
        ] {
            assert!(ImageInput::from_bytes(&bytes).is_err());
        }
        assert!(ImageInput::from_path(Path::new(".")).is_err());
        assert!(ImageInput::from_path(Path::new("/nonexistent/yunxi-test.png")).is_err());
    }

    #[test]
    fn role_count_and_total_bytes_fail_closed() {
        let image = ImageInput::from_bytes(b"GIF89a").unwrap();
        let mut message = Message::user_with_images("图", vec![image.clone()]);
        assert!(validate_messages(&[message.clone()]).is_ok());
        message.role = Role::Tool;
        assert!(validate_messages(&[message]).is_err());
        assert!(
            validate_messages(&[Message::user_with_images("", vec![image; MAX_IMAGES + 1])])
                .is_err()
        );
        let mut bytes = vec![0; MAX_IMAGE_BYTES];
        bytes[..6].copy_from_slice(b"GIF89a");
        let large = ImageInput::from_bytes(&bytes).unwrap();
        assert!(
            validate_messages(&[Message::user_with_images("", vec![large.clone(); 2])]).is_ok()
        );
        assert!(validate_messages(&[Message::user_with_images("", vec![large; 3])]).is_err());
    }
}
