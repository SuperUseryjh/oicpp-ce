//! 编码探测与解码（对应 main.js 的 detectEncoding / decodeBufferAuto）
//! 以及 base64 辅助。

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16,
    Gbk,
}

/// 对应 main.js 的 detectEncoding
pub fn detect_encoding(bytes: &[u8]) -> Encoding {
    if bytes.len() >= 3 && bytes[0] == 0xEF && bytes[1] == 0xBB && bytes[2] == 0xBF {
        return Encoding::Utf8;
    }
    if bytes.len() >= 2
        && ((bytes[0] == 0xFF && bytes[1] == 0xFE) || (bytes[0] == 0xFE && bytes[1] == 0xFF))
    {
        return Encoding::Utf16;
    }
    if is_valid_utf8(bytes) {
        Encoding::Utf8
    } else {
        Encoding::Gbk
    }
}

/// 严格 UTF-8 校验（不接受非法序列）
pub fn is_valid_utf8(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok()
}

/// 对应 main.js 的 decodeBufferAuto
pub fn decode_auto(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    match detect_encoding(bytes) {
        Encoding::Utf8 | Encoding::Utf16 => match std::str::from_utf8(bytes) {
            Ok(s) => s.trim_start_matches('\u{feff}').to_string(),
            Err(_) => decode_gbk(bytes).unwrap_or_else(|| String::from_utf8_lossy(bytes).to_string()),
        },
        Encoding::Gbk => {
            decode_gbk(bytes).unwrap_or_else(|| String::from_utf8_lossy(bytes).to_string())
        }
    }
}

/// 按 GBK 解码
pub fn decode_gbk(bytes: &[u8]) -> Option<String> {
    let (text, _, had_errors) = encoding_rs::GBK.decode(bytes);
    if had_errors {
        None
    } else {
        Some(text.to_string())
    }
}

/// 判定二进制内容（对应 main.js 的 isBinaryBuffer 逻辑）
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .any(|b| *b == 0 || (*b < 32 && *b != 9 && *b != 10 && *b != 13))
}

pub fn b64_encode(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub fn b64_decode(data: &str) -> Result<Vec<u8>, String> {
    B64.decode(data.as_bytes()).map_err(|e| e.to_string())
}

/// 内容指纹：`<字节长度>:<sha1 hex>`（对应 main.js writeUtf8FileIfChanged）
pub fn fingerprint(content: &str) -> String {
    use sha1::{Digest, Sha1};
    let bytes = content.as_bytes();
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}:{}", bytes.len(), hex)
}

/// SHA-256 十六进制摘要（用于编译缓存签名）
pub fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
