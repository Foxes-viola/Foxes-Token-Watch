//! 把首尾相接的多个 zstd 帧切开。
//!
//! Deepseek Harness 的会话文件每次追加一帧，文件末尾可能有一帧还没写完，
//! 所以要自己走一遍帧头和块头，只交出完整的帧，剩下的等下次再读。

use std::ops::Range;

const FRAME_MAGIC: u32 = 0xFD2F_B528;
const SKIPPABLE_MAGIC: u32 = 0x184D_2A50;

#[derive(Debug, Default)]
pub struct Frames {
    /// 每个完整数据帧在缓冲区里的范围
    pub ranges: Vec<Range<usize>>,
    /// 已经确认完整的字节数（下次从这里接着读）
    pub consumed: usize,
    /// 遇到了不是 zstd 帧的数据
    pub corrupt: bool,
}

fn read_u32(buf: &[u8], pos: usize) -> Option<u32> {
    let bytes = buf.get(pos..pos.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// 返回从 `pos` 开始的一帧的结束位置；`Ok(None)` 表示数据还不够。
fn frame_end(buf: &[u8], pos: usize) -> Result<Option<(usize, bool)>, ()> {
    let Some(magic) = read_u32(buf, pos) else {
        return Ok(None);
    };
    if magic & 0xFFFF_FFF0 == SKIPPABLE_MAGIC {
        let Some(size) = read_u32(buf, pos + 4) else {
            return Ok(None);
        };
        let end = pos + 8 + size as usize;
        return Ok(if end <= buf.len() { Some((end, false)) } else { None });
    }
    if magic != FRAME_MAGIC {
        return Err(());
    }
    let Some(&descriptor) = buf.get(pos + 4) else {
        return Ok(None);
    };
    let content_size_flag = descriptor >> 6;
    let single_segment = (descriptor >> 5) & 1 == 1;
    let has_checksum = (descriptor >> 2) & 1 == 1;
    let dict_id_size = [0usize, 1, 2, 4][(descriptor & 3) as usize];
    let content_size_bytes = if content_size_flag == 0 {
        usize::from(single_segment)
    } else {
        [0usize, 2, 4, 8][content_size_flag as usize]
    };
    let mut cursor = pos + 5 + usize::from(!single_segment) + dict_id_size + content_size_bytes;
    loop {
        let Some(header) = buf.get(cursor..cursor + 3) else {
            return Ok(None);
        };
        let header = header[0] as usize | (header[1] as usize) << 8 | (header[2] as usize) << 16;
        let last = header & 1 == 1;
        let block_type = (header >> 1) & 3;
        let size = header >> 3;
        if block_type == 3 {
            return Err(());
        }
        // RLE 块只存 1 个字节
        cursor += 3 + if block_type == 1 { 1 } else { size };
        if cursor > buf.len() {
            return Ok(None);
        }
        if last {
            break;
        }
    }
    if has_checksum {
        cursor += 4;
        if cursor > buf.len() {
            return Ok(None);
        }
    }
    Ok(Some((cursor, true)))
}

pub fn complete_frames(buf: &[u8]) -> Frames {
    let mut frames = Frames::default();
    let mut pos = 0;
    while pos < buf.len() {
        match frame_end(buf, pos) {
            Ok(Some((end, is_data))) => {
                if is_data {
                    frames.ranges.push(pos..end);
                }
                pos = end;
                frames.consumed = end;
            }
            Ok(None) => break,
            Err(()) => {
                frames.corrupt = true;
                break;
            }
        }
    }
    frames
}

pub fn decompress(frame: &[u8]) -> std::io::Result<Vec<u8>> {
    zstd::stream::decode_all(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compress(data: &[u8]) -> Vec<u8> {
        zstd::stream::encode_all(data, 3).unwrap()
    }

    #[test]
    fn splits_concatenated_frames() {
        let first = compress(b"{\"a\":1}\n");
        let second = compress(&b"{\"b\":2}\n".repeat(500));
        let mut joined = first.clone();
        joined.extend_from_slice(&second);

        let frames = complete_frames(&joined);
        assert!(!frames.corrupt);
        assert_eq!(frames.ranges, vec![0..first.len(), first.len()..joined.len()]);
        assert_eq!(frames.consumed, joined.len());
        assert_eq!(decompress(&joined[frames.ranges[0].clone()]).unwrap(), b"{\"a\":1}\n");
        assert_eq!(decompress(&joined[frames.ranges[1].clone()]).unwrap().len(), 8 * 500);
    }

    #[test]
    fn leaves_partial_frame_for_later() {
        let first = compress(b"first frame");
        let second = compress(b"second frame that is still being written");
        let mut joined = first.clone();
        joined.extend_from_slice(&second[..second.len() - 2]);

        let frames = complete_frames(&joined);
        assert!(!frames.corrupt);
        assert_eq!(frames.ranges.len(), 1);
        assert_eq!(frames.consumed, first.len());
    }

    #[test]
    fn flags_garbage() {
        let frames = complete_frames(b"this is not zstd at all");
        assert!(frames.corrupt);
        assert!(frames.ranges.is_empty());
    }
}
