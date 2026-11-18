//! 宽松的反序列化工具。
//!
//! 各家的记录格式会随版本变化，同名字段在不同事件里类型也可能不同。这里的类型都遵守同一条规则：
//! 类型对得上就取值，对不上就当作没有，绝不让整行解析失败。

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;

use serde::de::value::MapAccessDeserializer;
use serde::de::{Deserialize, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

fn drain_seq<'de, A: SeqAccess<'de>>(mut seq: A) -> Result<(), A::Error> {
    while seq.next_element::<IgnoredAny>()?.is_some() {}
    Ok(())
}

fn drain_map<'de, A: MapAccess<'de>>(mut map: A) -> Result<(), A::Error> {
    while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
    Ok(())
}

struct NumberVisitor;

impl<'de> Visitor<'de> for NumberVisitor {
    type Value = f64;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("anything")
    }
    fn visit_bool<E>(self, _: bool) -> Result<f64, E> {
        Ok(0.0)
    }
    fn visit_i64<E>(self, v: i64) -> Result<f64, E> {
        Ok(v as f64)
    }
    fn visit_u64<E>(self, v: u64) -> Result<f64, E> {
        Ok(v as f64)
    }
    fn visit_f64<E>(self, v: f64) -> Result<f64, E> {
        Ok(if v.is_finite() { v } else { 0.0 })
    }
    fn visit_str<E>(self, v: &str) -> Result<f64, E> {
        Ok(v.trim().parse::<f64>().ok().filter(|n| n.is_finite()).unwrap_or(0.0))
    }
    fn visit_unit<E>(self) -> Result<f64, E> {
        Ok(0.0)
    }
    fn visit_none<E>(self) -> Result<f64, E> {
        Ok(0.0)
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<f64, D::Error> {
        d.deserialize_any(NumberVisitor)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<f64, A::Error> {
        drain_seq(seq)?;
        Ok(0.0)
    }
    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<f64, A::Error> {
        drain_map(map)?;
        Ok(0.0)
    }
}

/// 数字；null、缺失、负数、非数字都当 0。
pub fn u64_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let value = d.deserialize_any(NumberVisitor)?;
    Ok(if value > 0.0 { value as u64 } else { 0 })
}

pub fn i64_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    Ok(d.deserialize_any(NumberVisitor)? as i64)
}

pub fn f64_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    d.deserialize_any(NumberVisitor)
}

/// 时刻：数字按 epoch 毫秒（小于 1e11 的按秒），字符串按 ISO-8601；其他当 0。
#[derive(Clone, Copy, Debug, Default)]
pub struct TimeMs(pub i64);

impl<'de> Deserialize<'de> for TimeMs {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        fn from_number(value: f64) -> TimeMs {
            if !value.is_finite() || value <= 0.0 {
                TimeMs(0)
            } else if value < 1e11 {
                TimeMs((value * 1000.0) as i64)
            } else {
                TimeMs(value as i64)
            }
        }
        impl<'de> Visitor<'de> for V {
            type Value = TimeMs;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("anything")
            }
            fn visit_i64<E>(self, v: i64) -> Result<TimeMs, E> {
                Ok(from_number(v as f64))
            }
            fn visit_u64<E>(self, v: u64) -> Result<TimeMs, E> {
                Ok(from_number(v as f64))
            }
            fn visit_f64<E>(self, v: f64) -> Result<TimeMs, E> {
                Ok(from_number(v))
            }
            fn visit_str<E>(self, v: &str) -> Result<TimeMs, E> {
                Ok(match crate::timeutil::parse_iso_ms(v) {
                    Some(ms) => TimeMs(ms),
                    None => from_number(v.trim().parse::<f64>().unwrap_or(0.0)),
                })
            }
            fn visit_bool<E>(self, _: bool) -> Result<TimeMs, E> {
                Ok(TimeMs(0))
            }
            fn visit_unit<E>(self) -> Result<TimeMs, E> {
                Ok(TimeMs(0))
            }
            fn visit_none<E>(self) -> Result<TimeMs, E> {
                Ok(TimeMs(0))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<TimeMs, A::Error> {
                drain_seq(seq)?;
                Ok(TimeMs(0))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<TimeMs, A::Error> {
                drain_map(map)?;
                Ok(TimeMs(0))
            }
        }
        d.deserialize_any(V)
    }
}

/// 数字数组的和与个数（单个数字也算）；其他类型当作空。
#[derive(Clone, Copy, Debug, Default)]
pub struct NumSum {
    pub sum: f64,
    pub count: u32,
}

impl<'de> Deserialize<'de> for NumSum {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = NumSum;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("anything")
            }
            fn visit_i64<E>(self, v: i64) -> Result<NumSum, E> {
                Ok(NumSum { sum: v as f64, count: 1 })
            }
            fn visit_u64<E>(self, v: u64) -> Result<NumSum, E> {
                Ok(NumSum { sum: v as f64, count: 1 })
            }
            fn visit_f64<E>(self, v: f64) -> Result<NumSum, E> {
                Ok(NumSum { sum: if v.is_finite() { v } else { 0.0 }, count: 1 })
            }
            fn visit_str<E>(self, _: &str) -> Result<NumSum, E> {
                Ok(NumSum::default())
            }
            fn visit_bool<E>(self, _: bool) -> Result<NumSum, E> {
                Ok(NumSum::default())
            }
            fn visit_unit<E>(self) -> Result<NumSum, E> {
                Ok(NumSum::default())
            }
            fn visit_none<E>(self) -> Result<NumSum, E> {
                Ok(NumSum::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<NumSum, A::Error> {
                let mut total = NumSum::default();
                while let Some(item) = seq.next_element::<NumSum>()? {
                    total.sum += item.sum;
                    total.count += item.count;
                }
                Ok(total)
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<NumSum, A::Error> {
                drain_map(map)?;
                Ok(NumSum::default())
            }
        }
        d.deserialize_any(V)
    }
}

/// 错误码：字符串或数字都接受，只保留短的标识符样式（报错正文一律丢弃）。
#[derive(Clone, Debug, Default)]
pub struct ShortCode(pub String);

pub fn is_short_code(text: &str) -> bool {
    !text.is_empty() && text.len() <= 48 && text.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}

impl<'de> Deserialize<'de> for ShortCode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = ShortCode;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("anything")
            }
            fn visit_str<E>(self, v: &str) -> Result<ShortCode, E> {
                Ok(ShortCode(if is_short_code(v) { v.to_owned() } else { String::new() }))
            }
            fn visit_i64<E>(self, v: i64) -> Result<ShortCode, E> {
                Ok(ShortCode(v.to_string()))
            }
            fn visit_u64<E>(self, v: u64) -> Result<ShortCode, E> {
                Ok(ShortCode(v.to_string()))
            }
            fn visit_f64<E>(self, _: f64) -> Result<ShortCode, E> {
                Ok(ShortCode::default())
            }
            fn visit_bool<E>(self, _: bool) -> Result<ShortCode, E> {
                Ok(ShortCode::default())
            }
            fn visit_unit<E>(self) -> Result<ShortCode, E> {
                Ok(ShortCode::default())
            }
            fn visit_none<E>(self) -> Result<ShortCode, E> {
                Ok(ShortCode::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<ShortCode, A::Error> {
                drain_seq(seq)?;
                Ok(ShortCode::default())
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<ShortCode, A::Error> {
                drain_map(map)?;
                Ok(ShortCode::default())
            }
        }
        d.deserialize_any(V)
    }
}

/// 布尔；其他类型都当 false。
#[derive(Clone, Copy, Debug, Default)]
pub struct Flag(pub bool);

impl<'de> Deserialize<'de> for Flag {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Flag;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("anything")
            }
            fn visit_bool<E>(self, v: bool) -> Result<Flag, E> {
                Ok(Flag(v))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Flag, E> {
                Ok(Flag(false))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Flag, E> {
                Ok(Flag(false))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Flag, E> {
                Ok(Flag(false))
            }
            fn visit_str<E>(self, _: &str) -> Result<Flag, E> {
                Ok(Flag(false))
            }
            fn visit_unit<E>(self) -> Result<Flag, E> {
                Ok(Flag(false))
            }
            fn visit_none<E>(self) -> Result<Flag, E> {
                Ok(Flag(false))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Flag, A::Error> {
                drain_seq(seq)?;
                Ok(Flag(false))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Flag, A::Error> {
                drain_map(map)?;
                Ok(Flag(false))
            }
        }
        d.deserialize_any(V)
    }
}

/// 字符串（能借用就借用）；其他类型当作没有。
#[derive(Clone, Debug, Default)]
pub struct Text<'a>(pub Option<Cow<'a, str>>);

impl<'a> Text<'a> {
    pub fn as_str(&self) -> &str {
        self.0.as_deref().unwrap_or("")
    }

    pub fn is_empty(&self) -> bool {
        self.as_str().is_empty()
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Text<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<'a>(PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
            type Value = Text<'a>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("anything")
            }
            fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Text<'a>, E> {
                Ok(Text(Some(Cow::Borrowed(v))))
            }
            fn visit_str<E>(self, v: &str) -> Result<Text<'a>, E> {
                Ok(Text(Some(Cow::Owned(v.to_owned()))))
            }
            fn visit_string<E>(self, v: String) -> Result<Text<'a>, E> {
                Ok(Text(Some(Cow::Owned(v))))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Text<'a>, E> {
                Ok(Text(None))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Text<'a>, E> {
                Ok(Text(None))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Text<'a>, E> {
                Ok(Text(None))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Text<'a>, E> {
                Ok(Text(None))
            }
            fn visit_unit<E>(self) -> Result<Text<'a>, E> {
                Ok(Text(None))
            }
            fn visit_none<E>(self) -> Result<Text<'a>, E> {
                Ok(Text(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Text<'a>, A::Error> {
                drain_seq(seq)?;
                Ok(Text(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Text<'a>, A::Error> {
                drain_map(map)?;
                Ok(Text(None))
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

/// 文本折算成 token 的近似权重：ASCII 每字节算 1，其他字符每个算 5。
///
/// 实测一个汉字约 1.5 个 token、英文和代码约 0.3 token / 字节，比例大致 5 : 1。
/// 只用来比较几段文本各占多少，不用来算绝对数量。
pub fn text_weight(text: &str) -> u64 {
    let mut ascii = 0u64;
    let mut wide = 0u64;
    for &byte in text.as_bytes() {
        if byte < 0x80 {
            ascii += 1;
        } else if byte >= 0xC0 {
            wide += 1;
        }
    }
    ascii + 5 * wide
}

/// 只关心字符串有多少分量、以什么开头，不保留内容。嵌套的数组会把里面所有字符串加起来。
#[derive(Clone, Copy, Debug, Default)]
pub struct TextStat {
    /// 所有字符串的 [`text_weight`] 之和
    pub len: u64,
    pub count: u32,
    /// 第一个字符串是否以 `[Request interrupted` 开头（Claude 的“用户中断”标记）
    pub interrupted: bool,
}

struct TextStatVisitor<'s>(&'s mut TextStat);

impl<'de, 's> Visitor<'de> for TextStatVisitor<'s> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("anything")
    }
    fn visit_str<E>(self, v: &str) -> Result<(), E> {
        if self.0.count == 0 {
            self.0.interrupted = v.starts_with("[Request interrupted");
        }
        self.0.len += text_weight(v);
        self.0.count += 1;
        Ok(())
    }
    fn visit_bool<E>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_none<E>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq.next_element_seed(TextStatSeed(&mut *self.0))?.is_some() {}
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<(), A::Error> {
        drain_map(map)
    }
}

struct TextStatSeed<'s>(&'s mut TextStat);

impl<'de, 's> serde::de::DeserializeSeed<'de> for TextStatSeed<'s> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(TextStatVisitor(self.0))
    }
}

impl<'de> Deserialize<'de> for TextStat {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut stat = TextStat::default();
        d.deserialize_any(TextStatVisitor(&mut stat))?;
        Ok(stat)
    }
}

/// 任意 JSON 值的“体量”：所有键和字符串的 [`text_weight`] 之和，标量每个算 4。只用来估算占比，不保留内容。
#[derive(Clone, Copy, Debug, Default)]
pub struct Weight(pub u64);

struct WeightVisitor<'s>(&'s mut u64);

impl<'de, 's> Visitor<'de> for WeightVisitor<'s> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("anything")
    }
    fn visit_str<E>(self, v: &str) -> Result<(), E> {
        *self.0 += text_weight(v);
        Ok(())
    }
    fn visit_bool<E>(self, _: bool) -> Result<(), E> {
        *self.0 += 4;
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> Result<(), E> {
        *self.0 += 4;
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> Result<(), E> {
        *self.0 += 4;
        Ok(())
    }
    fn visit_f64<E>(self, _: f64) -> Result<(), E> {
        *self.0 += 4;
        Ok(())
    }
    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_none<E>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq.next_element_seed(WeightSeed(&mut *self.0))?.is_some() {}
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while map.next_key_seed(WeightSeed(&mut *self.0))?.is_some() {
            map.next_value_seed(WeightSeed(&mut *self.0))?;
        }
        Ok(())
    }
}

struct WeightSeed<'s>(&'s mut u64);

impl<'de, 's> serde::de::DeserializeSeed<'de> for WeightSeed<'s> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(WeightVisitor(self.0))
    }
}

impl<'de> Deserialize<'de> for Weight {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut total = 0u64;
        d.deserialize_any(WeightVisitor(&mut total))?;
        Ok(Weight(total))
    }
}

/// 值是对象时才按 `T` 解析，其他类型当作没有。
#[derive(Clone, Debug)]
pub struct Obj<T>(pub Option<T>);

impl<T> Default for Obj<T> {
    fn default() -> Self {
        Obj(None)
    }
}

impl<T> Obj<T> {
    pub fn get(&self) -> Option<&T> {
        self.0.as_ref()
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Obj<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = Obj<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("anything")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Obj<T>, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(|value| Obj(Some(value)))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Obj<T>, E> {
                Ok(Obj(None))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Obj<T>, E> {
                Ok(Obj(None))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Obj<T>, E> {
                Ok(Obj(None))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Obj<T>, E> {
                Ok(Obj(None))
            }
            fn visit_str<E>(self, _: &str) -> Result<Obj<T>, E> {
                Ok(Obj(None))
            }
            fn visit_unit<E>(self) -> Result<Obj<T>, E> {
                Ok(Obj(None))
            }
            fn visit_none<E>(self) -> Result<Obj<T>, E> {
                Ok(Obj(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Obj<T>, A::Error> {
                drain_seq(seq)?;
                Ok(Obj(None))
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

/// 数组里的每个对象按 `T` 解析后交给回调；非数组当作空。用来扫一遍内容块而不保留它们。
pub fn visit_objects<'de, D, T, F>(d: D, on_item: F) -> Result<(), D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
    F: FnMut(T),
{
    struct V<T, F>(F, PhantomData<T>);
    impl<'de, T: Deserialize<'de>, F: FnMut(T)> Visitor<'de> for V<T, F> {
        type Value = ();
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("anything")
        }
        fn visit_seq<A: SeqAccess<'de>>(mut self, mut seq: A) -> Result<(), A::Error> {
            while let Some(item) = seq.next_element::<Obj<T>>()? {
                if let Some(item) = item.0 {
                    (self.0)(item);
                }
            }
            Ok(())
        }
        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<(), A::Error> {
            drain_map(map)
        }
        fn visit_bool<E>(self, _: bool) -> Result<(), E> {
            Ok(())
        }
        fn visit_i64<E>(self, _: i64) -> Result<(), E> {
            Ok(())
        }
        fn visit_u64<E>(self, _: u64) -> Result<(), E> {
            Ok(())
        }
        fn visit_f64<E>(self, _: f64) -> Result<(), E> {
            Ok(())
        }
        fn visit_str<E>(self, _: &str) -> Result<(), E> {
            Ok(())
        }
        fn visit_unit<E>(self) -> Result<(), E> {
            Ok(())
        }
        fn visit_none<E>(self) -> Result<(), E> {
            Ok(())
        }
    }
    d.deserialize_any(V(on_item, PhantomData))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Inner<'a> {
        #[serde(borrow, default)]
        name: Text<'a>,
        #[serde(default, deserialize_with = "u64_lenient")]
        count: u64,
    }

    #[derive(Deserialize)]
    struct Outer<'a> {
        #[serde(borrow, default)]
        kind: Text<'a>,
        #[serde(borrow, default)]
        inner: Obj<Inner<'a>>,
        #[serde(default)]
        flag: Flag,
        #[serde(default)]
        text: TextStat,
    }

    #[test]
    fn takes_values_when_types_match() {
        let outer: Outer = serde_json::from_str(r#"{"kind":"a","inner":{"name":"x\ny","count":3,"extra":[1,{"k":2}]},"flag":true,"text":["ab",["cde"],5]}"#).unwrap();
        assert_eq!(outer.kind.as_str(), "a");
        let inner = outer.inner.0.unwrap();
        assert_eq!(inner.name.as_str(), "x\ny");
        assert_eq!(inner.count, 3);
        assert!(outer.flag.0);
        assert_eq!((outer.text.len, outer.text.count), (5, 2));
    }

    #[test]
    fn ignores_mismatched_types() {
        let outer: Outer = serde_json::from_str(r#"{"kind":{"nested":1},"inner":"just a string","flag":"yes","text":{"a":1}}"#).unwrap();
        assert!(outer.kind.is_empty());
        assert!(outer.inner.0.is_none());
        assert!(!outer.flag.0);
        assert_eq!(outer.text.count, 0);

        let inner: Inner = serde_json::from_str(r#"{"name":null,"count":null}"#).unwrap();
        assert!(inner.name.is_empty());
        assert_eq!(inner.count, 0);
        let inner: Inner = serde_json::from_str(r#"{"count":"12"}"#).unwrap();
        assert_eq!(inner.count, 12);
        let inner: Inner = serde_json::from_str(r#"{"count":-4.5}"#).unwrap();
        assert_eq!(inner.count, 0);
    }

    #[test]
    fn detects_interrupt_marker() {
        let stat: TextStat = serde_json::from_str(r#""[Request interrupted by user]""#).unwrap();
        assert!(stat.interrupted);
        let stat: TextStat = serde_json::from_str(r#""hello""#).unwrap();
        assert!(!stat.interrupted);
    }

    #[test]
    fn visits_objects_in_arrays() {
        let mut names = Vec::new();
        let mut de = serde_json::Deserializer::from_str(r#"[{"name":"a"},"skip",{"name":"b","count":2},7]"#);
        visit_objects(&mut de, |item: Inner| names.push(item.name.as_str().to_owned())).unwrap();
        assert_eq!(names, vec!["a", "b"]);
    }
}
