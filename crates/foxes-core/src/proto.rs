//! 最小的 protobuf wire 格式读取器（不需要 schema），用来读 Antigravity 的步骤元数据。

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Field<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

/// 逐个读出 `(字段号, 值)`；遇到损坏的数据就停下，不会 panic。
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn varint(&mut self) -> Option<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = *self.buf.get(self.pos)?;
            self.pos += 1;
            if shift < 64 {
                result |= ((byte & 0x7f) as u64) << shift;
            }
            if byte & 0x80 == 0 {
                return Some(result);
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(len)?;
        let bytes = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(bytes)
    }

    fn read(&mut self) -> Option<(u32, Field<'a>)> {
        let key = self.varint()?;
        let number = (key >> 3) as u32;
        if number == 0 {
            return None;
        }
        let field = match key & 7 {
            0 => Field::Varint(self.varint()?),
            1 => Field::Fixed64(u64::from_le_bytes(self.take(8)?.try_into().ok()?)),
            2 => {
                let len = self.varint()? as usize;
                Field::Bytes(self.take(len)?)
            }
            5 => Field::Fixed32(u32::from_le_bytes(self.take(4)?.try_into().ok()?)),
            _ => return None,
        };
        Some((number, field))
    }
}

impl<'a> Iterator for Reader<'a> {
    type Item = (u32, Field<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let item = self.read();
        if item.is_none() {
            self.pos = self.buf.len();
        }
        item
    }
}

pub fn first_bytes(buf: &[u8], number: u32) -> Option<&[u8]> {
    Reader::new(buf).find_map(|(field, value)| match value {
        Field::Bytes(bytes) if field == number => Some(bytes),
        _ => None,
    })
}

pub fn first_varint(buf: &[u8], number: u32) -> Option<u64> {
    Reader::new(buf).find_map(|(field, value)| match value {
        Field::Varint(v) if field == number => Some(v),
        _ => None,
    })
}

pub fn first_string(buf: &[u8], number: u32) -> Option<String> {
    first_bytes(buf, number).and_then(|bytes| std::str::from_utf8(bytes).ok()).map(str::to_owned)
}

/// 按路径逐层取嵌套消息，例如 `nested(buf, &[1, 19])`。
pub fn nested<'a>(buf: &'a [u8], path: &[u32]) -> Option<&'a [u8]> {
    let mut current = buf;
    for &number in path {
        current = first_bytes(current, number)?;
    }
    Some(current)
}

/// 读 `google.protobuf.Timestamp` 样式的 `{1: 秒, 2: 纳秒}`，返回 epoch 毫秒。
pub fn timestamp_ms(buf: &[u8]) -> Option<i64> {
    let mut seconds = None;
    let mut nanos = 0u64;
    for (number, value) in Reader::new(buf) {
        match (number, value) {
            (1, Field::Varint(v)) => seconds = Some(v),
            (2, Field::Varint(v)) => nanos = v,
            _ => {}
        }
    }
    let seconds = seconds?;
    // 合理范围：2001 年 ~ 2100 年，挡掉被误当成时间戳的其他消息
    if !(1_000_000_000..4_102_444_800).contains(&seconds) {
        return None;
    }
    Some(seconds as i64 * 1_000 + (nanos / 1_000_000) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(mut value: u64, out: &mut Vec<u8>) {
        while value >= 0x80 {
            out.push((value as u8 & 0x7f) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }

    fn field_varint(number: u32, value: u64, out: &mut Vec<u8>) {
        varint((number as u64) << 3, out);
        varint(value, out);
    }

    fn field_bytes(number: u32, bytes: &[u8], out: &mut Vec<u8>) {
        varint(((number as u64) << 3) | 2, out);
        varint(bytes.len() as u64, out);
        out.extend_from_slice(bytes);
    }

    #[test]
    fn reads_nested_messages() {
        let mut time = Vec::new();
        field_varint(1, 1_793_684_332, &mut time);
        field_varint(2, 97_094_200, &mut time);

        let mut usage = Vec::new();
        field_varint(2, 3335, &mut usage);
        field_varint(3, 1418, &mut usage);
        field_varint(5, 31369, &mut usage);

        let mut message = Vec::new();
        field_bytes(1, &time, &mut message);
        field_varint(3, 2, &mut message);
        field_bytes(9, &usage, &mut message);

        assert_eq!(timestamp_ms(first_bytes(&message, 1).unwrap()), Some(1_793_684_332_097));
        assert_eq!(first_varint(&message, 3), Some(2));
        assert_eq!(first_varint(nested(&message, &[9]).unwrap(), 5), Some(31369));
        assert_eq!(first_varint(&message, 42), None);
    }

    #[test]
    fn stops_on_truncated_input() {
        let mut message = Vec::new();
        field_bytes(4, b"hello world", &mut message);
        message.truncate(message.len() - 3);
        assert_eq!(Reader::new(&message).count(), 0);
        assert_eq!(Reader::new(&[0xff, 0xff, 0xff]).count(), 0);
    }
}
