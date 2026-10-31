//! LDBP/1 线协议：`uint32 大端长度 + UTF-8 JSON`。
//!
//! ```text
//! +------------------+---------------------------+
//! | uint32 长度 (BE) | UTF-8 JSON 报文体        |
//! +------------------+---------------------------+
//! ```

use crate::error::{self, DbError, Result};
use crate::json::{self, Json};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// 常量时间字符串比较（认证令牌用）。
///
/// 普通 `==` 在第一个不同字节处短路返回，攻击者可以按响应时间逐字节
/// 猜出 token 前缀。这里固定遍历到较长者末尾，只累积差异、不提前返回。
/// 长度本身不算秘密，可以直接比较。
pub fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff: u8 = if a.len() == b.len() { 0 } else { 1 };
    let width = a.len().max(b.len());
    for i in 0..width {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

pub fn encode_frame(message: &Json) -> Result<Vec<u8>> {
    let text = message.to_string();
    let bytes = text.as_bytes();
    if bytes.len() > MAX_FRAME {
        return Err(DbError::new(error::PROTOCOL, "报文超过 16MB 上限"));
    }
    let mut out = Vec::with_capacity(4 + bytes.len());
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(out)
}

pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> FrameDecoder {
        FrameDecoder { buffer: Vec::new() }
    }

    /// 追加一段字节，返回其中解析出的完整报文（可能 0 个或多个）
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Json>> {
        self.buffer.extend_from_slice(chunk);
        let mut messages = Vec::new();

        loop {
            if self.buffer.len() < 4 {
                break;
            }
            let len = u32::from_be_bytes([
                self.buffer[0],
                self.buffer[1],
                self.buffer[2],
                self.buffer[3],
            ]) as usize;
            if len > MAX_FRAME {
                return Err(DbError::new(error::PROTOCOL, "收到的帧超过 16MB 上限"));
            }
            if self.buffer.len() < 4 + len {
                break;
            }
            let payload = &self.buffer[4..4 + len];
            let text = std::str::from_utf8(payload)
                .map_err(|_| DbError::new(error::PROTOCOL, "报文体不是合法 UTF-8"))?;
            let message = json::parse(text)
                .map_err(|_| DbError::new(error::PROTOCOL, "报文体不是合法 JSON"))?;
            messages.push(message);
            self.buffer.drain(..4 + len);
        }

        Ok(messages)
    }
}

impl Default for FrameDecoder {
    fn default() -> Self {
        FrameDecoder::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let message = Json::Obj(vec![
            ("type".to_string(), Json::str("query")),
            ("id".to_string(), Json::Num(7.0)),
        ]);
        let frame = encode_frame(&message).unwrap();
        let expected = ((frame.len() - 4) as u32).to_be_bytes();
        assert_eq!(&frame[..4], &expected, "帧头应为 4 字节大端长度");

        let mut decoder = FrameDecoder::new();
        let out = decoder.push(&frame).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].get("type").unwrap().as_str(), Some("query"));
    }

    #[test]
    fn split_across_chunks() {
        let message = Json::Obj(vec![("type".to_string(), Json::str("ping"))]);
        let frame = encode_frame(&message).unwrap();
        let (a, b) = frame.split_at(3);

        let mut decoder = FrameDecoder::new();
        assert_eq!(decoder.push(a).unwrap().len(), 0, "半帧不应产出报文");
        assert_eq!(decoder.push(b).unwrap().len(), 1);
    }

    #[test]
    fn multiple_frames_in_one_chunk() {
        let one = encode_frame(&Json::Obj(vec![("n".to_string(), Json::Num(1.0))])).unwrap();
        let two = encode_frame(&Json::Obj(vec![("n".to_string(), Json::Num(2.0))])).unwrap();
        let mut merged = one.clone();
        merged.extend_from_slice(&two);

        let mut decoder = FrameDecoder::new();
        let out = decoder.push(&merged).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].get("n").unwrap().as_i64(), Some(2));
    }

    #[test]
    fn oversized_length_is_rejected() {
        let mut decoder = FrameDecoder::new();
        let mut header = vec![0u8; 4];
        header.copy_from_slice(&(MAX_FRAME as u32 + 1).to_be_bytes());
        assert!(decoder.push(&header).is_err());
    }
}
