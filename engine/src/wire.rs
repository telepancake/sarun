//! Frugal binary atoms shared by sarun's direct Rust transports.
//!
//! This is the Rust implementation of the format defined by
//! `tv/wire/wire.h`: self-byte scalars, one-byte inline lengths through 55
//! bytes, and bounded little-endian long lengths. Payloads are opaque bytes;
//! compounds are outer atoms containing a sequence of inner atoms.

use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    Truncated,
    LengthOverflow,
    TooLarge,
    IntegerTooWide,
    IntegerOutOfRange,
    InvalidValue,
    InvalidUtf8,
    TrailingFields,
    TooFewItems,
    TooManyItems,
}

fn length_bytes(mut length: usize) -> usize {
    let mut bytes = 0;
    while length != 0 {
        bytes += 1;
        length >>= 8;
    }
    bytes
}

pub fn encoded_len(payload: &[u8]) -> Result<usize, DecodeError> {
    let length = payload.len();
    if length == 1 && payload[0] < 0xc0 {
        return Ok(1);
    }
    if length <= 55 {
        return Ok(1 + length);
    }
    let length_bytes = length_bytes(length);
    if length_bytes > 7 {
        return Err(DecodeError::LengthOverflow);
    }
    1usize
        .checked_add(length_bytes)
        .and_then(|prefix| prefix.checked_add(length))
        .ok_or(DecodeError::LengthOverflow)
}

fn put_length_prefix(output: &mut Vec<u8>, length: usize, allow_self_byte: Option<u8>) {
    if let Some(byte) = allow_self_byte
        && length == 1
        && byte < 0xc0
    {
        output.push(byte);
    } else if length <= 55 {
        output.push(0xc0 + length as u8);
    } else {
        let bytes = length_bytes(length);
        output.push(0xf8 + bytes as u8);
        for shift in 0..bytes {
            output.push((length >> (shift * 8)) as u8);
        }
    }
}

pub fn put_atom(output: &mut Vec<u8>, payload: &[u8]) -> Result<(), DecodeError> {
    output.reserve(encoded_len(payload)?);
    put_length_prefix(output, payload.len(), payload.first().copied());
    if !(payload.len() == 1 && payload[0] < 0xc0) {
        output.extend_from_slice(payload);
    }
    Ok(())
}

pub fn put_u64(output: &mut Vec<u8>, mut value: u64) {
    let mut bytes = [0u8; 8];
    let mut length = 0;
    while value != 0 {
        bytes[length] = value as u8;
        length += 1;
        value >>= 8;
    }
    put_atom(output, &bytes[..length]).expect("an eight-byte integer always fits");
}

#[allow(dead_code)]
pub fn put_i64(output: &mut Vec<u8>, value: i64) {
    put_u64(output, ((value as u64) << 1) ^ ((value >> 63) as u64));
}

/// Encode one compound atom from raw child payloads without back-patching.
pub fn put_many(output: &mut Vec<u8>, fields: &[&[u8]]) -> Result<(), DecodeError> {
    let payload_length = fields.iter().try_fold(0usize, |total, field| {
        total
            .checked_add(encoded_len(field)?)
            .ok_or(DecodeError::LengthOverflow)
    })?;
    if length_bytes(payload_length) > 7 {
        return Err(DecodeError::LengthOverflow);
    }
    // tv compounds always use an explicit inline/long wrapper, even when the
    // encoded children happen to occupy one self-byte.
    put_length_prefix(output, payload_length, None);
    for field in fields {
        put_atom(output, field)?;
    }
    Ok(())
}

/// Wrap an already atom-encoded sequence as one compound.  Unlike `put_atom`,
/// this always emits an explicit wrapper even when the payload is one self
/// byte; that distinction is part of tv/wire's compound abstraction.
pub fn put_compound_payload(
    output: &mut Vec<u8>,
    encoded_fields: &[u8],
) -> Result<(), DecodeError> {
    if length_bytes(encoded_fields.len()) > 7 {
        return Err(DecodeError::LengthOverflow);
    }
    put_length_prefix(output, encoded_fields.len(), None);
    output.extend_from_slice(encoded_fields);
    Ok(())
}

/// Take one atom without copying. On failure `input` is not advanced.
pub fn get_atom<'a>(input: &mut &'a [u8], maximum_length: usize) -> Result<&'a [u8], DecodeError> {
    let source = *input;
    let Some(&tag) = source.first() else {
        return Err(DecodeError::Truncated);
    };
    let (prefix, length, self_byte) = if tag < 0xc0 {
        (0usize, 1usize, true)
    } else if tag < 0xf8 {
        (1usize, (tag - 0xc0) as usize, false)
    } else {
        let bytes = (tag - 0xf8) as usize;
        if source.len() < 1 + bytes {
            return Err(DecodeError::Truncated);
        }
        let mut length = 0usize;
        for (index, byte) in source[1..1 + bytes].iter().enumerate() {
            length = length
                .checked_add((*byte as usize) << (index * 8))
                .ok_or(DecodeError::LengthOverflow)?;
        }
        (1 + bytes, length, false)
    };
    if length > maximum_length {
        return Err(DecodeError::TooLarge);
    }
    if self_byte {
        *input = &source[1..];
        return Ok(&source[..1]);
    }
    let end = prefix
        .checked_add(length)
        .ok_or(DecodeError::LengthOverflow)?;
    if source.len() < end {
        return Err(DecodeError::Truncated);
    }
    *input = &source[end..];
    Ok(&source[prefix..end])
}

pub fn get_u64(input: &mut &[u8]) -> Result<u64, DecodeError> {
    let payload = get_atom(input, 8)?;
    u64_from_payload(payload)
}

pub fn u64_from_payload(payload: &[u8]) -> Result<u64, DecodeError> {
    if payload.len() > 8 {
        return Err(DecodeError::IntegerTooWide);
    }
    Ok(payload
        .iter()
        .enumerate()
        .fold(0u64, |value, (index, byte)| {
            value | ((*byte as u64) << (index * 8))
        }))
}

pub fn get_i64(input: &mut &[u8]) -> Result<i64, DecodeError> {
    let value = get_u64(input)?;
    Ok(((value >> 1) as i64) ^ -((value & 1) as i64))
}

/// A concrete schema value encoded as exactly one tv atom.
pub trait WireValue: Sized {
    fn encode_atom(&self, output: &mut Vec<u8>) -> Result<(), DecodeError>;
    fn decode_atom(input: &mut &[u8]) -> Result<Self, DecodeError>;
}

macro_rules! unsigned_wire_value {
    ($type:ty) => {
        impl WireValue for $type {
            fn encode_atom(&self, output: &mut Vec<u8>) -> Result<(), DecodeError> {
                put_u64(output, *self as u64);
                Ok(())
            }

            fn decode_atom(input: &mut &[u8]) -> Result<Self, DecodeError> {
                get_u64(input)?
                    .try_into()
                    .map_err(|_| DecodeError::IntegerOutOfRange)
            }
        }
    };
}

macro_rules! signed_wire_value {
    ($type:ty) => {
        impl WireValue for $type {
            fn encode_atom(&self, output: &mut Vec<u8>) -> Result<(), DecodeError> {
                put_i64(output, *self as i64);
                Ok(())
            }

            fn decode_atom(input: &mut &[u8]) -> Result<Self, DecodeError> {
                get_i64(input)?
                    .try_into()
                    .map_err(|_| DecodeError::IntegerOutOfRange)
            }
        }
    };
}

unsigned_wire_value!(u16);
unsigned_wire_value!(u32);
unsigned_wire_value!(u64);
signed_wire_value!(i32);
signed_wire_value!(i64);

impl WireValue for f64 {
    fn encode_atom(&self, output: &mut Vec<u8>) -> Result<(), DecodeError> {
        put_atom(output, &self.to_le_bytes())
    }

    fn decode_atom(input: &mut &[u8]) -> Result<Self, DecodeError> {
        let payload = get_atom(input, 8)?;
        let bytes: [u8; 8] = payload.try_into().map_err(|_| DecodeError::InvalidValue)?;
        Ok(Self::from_le_bytes(bytes))
    }
}

impl WireValue for bool {
    fn encode_atom(&self, output: &mut Vec<u8>) -> Result<(), DecodeError> {
        put_u64(output, u64::from(*self));
        Ok(())
    }

    fn decode_atom(input: &mut &[u8]) -> Result<Self, DecodeError> {
        match get_u64(input)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError::InvalidValue),
        }
    }
}

impl WireValue for () {
    fn encode_atom(&self, output: &mut Vec<u8>) -> Result<(), DecodeError> {
        put_compound_payload(output, &[])
    }

    fn decode_atom(input: &mut &[u8]) -> Result<Self, DecodeError> {
        let fields = get_atom(input, 0)?;
        if fields.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::TrailingFields)
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BoundedBytes<const MAXIMUM: usize>(Vec<u8>);

impl<const MAXIMUM: usize> BoundedBytes<MAXIMUM> {
    pub fn new(value: Vec<u8>) -> Result<Self, DecodeError> {
        if value.len() > MAXIMUM {
            return Err(DecodeError::TooLarge);
        }
        Ok(Self(value))
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.0
    }
}

impl<const MAXIMUM: usize> WireValue for BoundedBytes<MAXIMUM> {
    fn encode_atom(&self, output: &mut Vec<u8>) -> Result<(), DecodeError> {
        if self.0.len() > MAXIMUM {
            return Err(DecodeError::TooLarge);
        }
        put_atom(output, &self.0)
    }

    fn decode_atom(input: &mut &[u8]) -> Result<Self, DecodeError> {
        Ok(Self(get_atom(input, MAXIMUM)?.to_vec()))
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BoundedText<const MAXIMUM: usize>(String);

impl<const MAXIMUM: usize> BoundedText<MAXIMUM> {
    pub fn new(value: String) -> Result<Self, DecodeError> {
        if value.len() > MAXIMUM {
            return Err(DecodeError::TooLarge);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl<const MAXIMUM: usize> WireValue for BoundedText<MAXIMUM> {
    fn encode_atom(&self, output: &mut Vec<u8>) -> Result<(), DecodeError> {
        if self.0.len() > MAXIMUM {
            return Err(DecodeError::TooLarge);
        }
        put_atom(output, self.0.as_bytes())
    }

    fn decode_atom(input: &mut &[u8]) -> Result<Self, DecodeError> {
        let payload = get_atom(input, MAXIMUM)?;
        let value = std::str::from_utf8(payload).map_err(|_| DecodeError::InvalidUtf8)?;
        Ok(Self(value.to_owned()))
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FixedBytes<const LENGTH: usize>(pub [u8; LENGTH]);

impl<const LENGTH: usize> WireValue for FixedBytes<LENGTH> {
    fn encode_atom(&self, output: &mut Vec<u8>) -> Result<(), DecodeError> {
        put_atom(output, &self.0)
    }

    fn decode_atom(input: &mut &[u8]) -> Result<Self, DecodeError> {
        let payload = get_atom(input, LENGTH)?;
        let value = payload.try_into().map_err(|_| DecodeError::InvalidValue)?;
        Ok(Self(value))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundedVec<T, const MINIMUM: usize, const MAXIMUM: usize>(Vec<T>);

impl<T, const MINIMUM: usize, const MAXIMUM: usize> BoundedVec<T, MINIMUM, MAXIMUM> {
    pub fn new(value: Vec<T>) -> Result<Self, DecodeError> {
        check_item_count::<MINIMUM, MAXIMUM>(value.len())?;
        Ok(Self(value))
    }

    pub fn as_slice(&self) -> &[T] {
        &self.0
    }

    pub fn into_inner(self) -> Vec<T> {
        self.0
    }
}

impl<T: WireValue, const MINIMUM: usize, const MAXIMUM: usize> WireValue
    for BoundedVec<T, MINIMUM, MAXIMUM>
{
    fn encode_atom(&self, output: &mut Vec<u8>) -> Result<(), DecodeError> {
        check_item_count::<MINIMUM, MAXIMUM>(self.0.len())?;
        let mut fields = Vec::new();
        put_u64(&mut fields, self.0.len() as u64);
        for item in &self.0 {
            item.encode_atom(&mut fields)?;
        }
        put_compound_payload(output, &fields)
    }

    fn decode_atom(input: &mut &[u8]) -> Result<Self, DecodeError> {
        let mut fields = get_atom(input, usize::MAX)?;
        let count: usize = get_u64(&mut fields)?
            .try_into()
            .map_err(|_| DecodeError::TooManyItems)?;
        check_item_count::<MINIMUM, MAXIMUM>(count)?;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(T::decode_atom(&mut fields)?);
        }
        require_empty(fields)?;
        Ok(Self(values))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundedMap<K, V, const MAXIMUM: usize>(BTreeMap<K, V>);

impl<K: Ord, V, const MAXIMUM: usize> BoundedMap<K, V, MAXIMUM> {
    pub fn new(value: BTreeMap<K, V>) -> Result<Self, DecodeError> {
        if value.len() > MAXIMUM {
            return Err(DecodeError::TooManyItems);
        }
        Ok(Self(value))
    }

    pub fn as_map(&self) -> &BTreeMap<K, V> {
        &self.0
    }

    pub fn into_inner(self) -> BTreeMap<K, V> {
        self.0
    }
}

impl<K: Ord + WireValue, V: WireValue, const MAXIMUM: usize> WireValue
    for BoundedMap<K, V, MAXIMUM>
{
    fn encode_atom(&self, output: &mut Vec<u8>) -> Result<(), DecodeError> {
        if self.0.len() > MAXIMUM {
            return Err(DecodeError::TooManyItems);
        }
        let mut fields = Vec::new();
        put_u64(&mut fields, self.0.len() as u64);
        for (key, value) in &self.0 {
            key.encode_atom(&mut fields)?;
            value.encode_atom(&mut fields)?;
        }
        put_compound_payload(output, &fields)
    }

    fn decode_atom(input: &mut &[u8]) -> Result<Self, DecodeError> {
        let mut fields = get_atom(input, usize::MAX)?;
        let count: usize = get_u64(&mut fields)?
            .try_into()
            .map_err(|_| DecodeError::TooManyItems)?;
        if count > MAXIMUM {
            return Err(DecodeError::TooManyItems);
        }
        let mut values = BTreeMap::new();
        for _ in 0..count {
            let key = K::decode_atom(&mut fields)?;
            let value = V::decode_atom(&mut fields)?;
            if values.insert(key, value).is_some() {
                return Err(DecodeError::InvalidValue);
            }
        }
        require_empty(fields)?;
        Ok(Self(values))
    }
}

impl<T: WireValue> WireValue for Option<T> {
    fn encode_atom(&self, output: &mut Vec<u8>) -> Result<(), DecodeError> {
        let mut fields = Vec::new();
        match self {
            None => put_u64(&mut fields, 0),
            Some(value) => {
                put_u64(&mut fields, 1);
                value.encode_atom(&mut fields)?;
            }
        }
        put_compound_payload(output, &fields)
    }

    fn decode_atom(input: &mut &[u8]) -> Result<Self, DecodeError> {
        let mut fields = get_atom(input, usize::MAX)?;
        let value = match get_u64(&mut fields)? {
            0 => None,
            1 => Some(T::decode_atom(&mut fields)?),
            _ => return Err(DecodeError::InvalidValue),
        };
        require_empty(fields)?;
        Ok(value)
    }
}

pub fn require_empty(input: &[u8]) -> Result<(), DecodeError> {
    if input.is_empty() {
        Ok(())
    } else {
        Err(DecodeError::TrailingFields)
    }
}

fn check_item_count<const MINIMUM: usize, const MAXIMUM: usize>(
    count: usize,
) -> Result<(), DecodeError> {
    if count < MINIMUM {
        Err(DecodeError::TooFewItems)
    } else if count > MAXIMUM {
        Err(DecodeError::TooManyItems)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(payload: &[u8]) -> Vec<u8> {
        let mut encoded = Vec::new();
        put_atom(&mut encoded, payload).unwrap();
        encoded
    }

    #[test]
    fn matches_tv_atom_boundary_encodings() {
        assert_eq!(atom(&[]), vec![0xc0]);
        assert_eq!(atom(&[0x2a]), vec![0x2a]);
        assert_eq!(atom(&[0xc0]), vec![0xc1, 0xc0]);
        assert_eq!(atom(&vec![0x11; 55]), [vec![0xf7], vec![0x11; 55]].concat());
        assert_eq!(
            atom(&vec![0x22; 56]),
            [vec![0xf9, 56], vec![0x22; 56]].concat(),
        );

        let mut integers = Vec::new();
        for value in [0, 1, 191, 192, 255, 256] {
            put_u64(&mut integers, value);
        }
        assert_eq!(
            integers,
            vec![0xc0, 1, 191, 0xc1, 192, 0xc1, 255, 0xc2, 0, 1]
        );
    }

    #[test]
    fn compounds_are_nested_zero_copy_atoms() {
        let mut encoded = Vec::new();
        put_many(&mut encoded, &[b"*", b"hello"]).unwrap();
        assert_eq!(encoded, b"\xc7*\xc5hello");
        let mut stream = encoded.as_slice();
        let mut compound = get_atom(&mut stream, 100).unwrap();
        assert!(stream.is_empty());
        assert_eq!(get_atom(&mut compound, 100).unwrap(), b"*");
        assert_eq!(get_atom(&mut compound, 100).unwrap(), b"hello");
        assert!(compound.is_empty());
    }

    #[test]
    fn fragmented_and_oversized_atoms_fail_without_consuming_input() {
        let encoded = atom(&vec![0x44; 256]);
        for length in 0..encoded.len() {
            let mut partial = &encoded[..length];
            assert_eq!(get_atom(&mut partial, 256), Err(DecodeError::Truncated));
            assert_eq!(partial, &encoded[..length]);
        }
        let mut complete = encoded.as_slice();
        assert_eq!(get_atom(&mut complete, 255), Err(DecodeError::TooLarge));
        assert_eq!(complete, encoded.as_slice());
        assert_eq!(get_atom(&mut complete, 256).unwrap(), vec![0x44; 256]);
    }

    fn roundtrip<T: WireValue + std::fmt::Debug + PartialEq>(value: T) {
        let mut encoded = Vec::new();
        value.encode_atom(&mut encoded).unwrap();
        let mut input = encoded.as_slice();
        assert_eq!(T::decode_atom(&mut input).unwrap(), value);
        assert!(input.is_empty());
    }

    #[test]
    fn bounded_typed_values_roundtrip_without_field_names() {
        roundtrip(BoundedText::<32>::new("hello".into()).unwrap());
        roundtrip(BoundedBytes::<8>::new(vec![0, 0xff, 7]).unwrap());
        roundtrip(FixedBytes::<4>([127, 0, 0, 1]));
        roundtrip(Some(BoundedVec::<u32, 1, 4>::new(vec![3, 5]).unwrap()));

        let mut values = BTreeMap::new();
        values.insert(
            BoundedText::<8>::new("key".into()).unwrap(),
            BoundedBytes::<8>::new(vec![1, 2]).unwrap(),
        );
        roundtrip(BoundedMap::<_, _, 2>::new(values).unwrap());
    }

    #[test]
    fn typed_decoders_fail_closed_on_bounds_tags_and_trailing_fields() {
        assert_eq!(
            BoundedVec::<u64, 1, 2>::new(vec![]),
            Err(DecodeError::TooFewItems),
        );
        assert_eq!(
            BoundedText::<2>::new("three".into()),
            Err(DecodeError::TooLarge),
        );

        let mut invalid_bool = Vec::new();
        put_u64(&mut invalid_bool, 2);
        assert_eq!(
            bool::decode_atom(&mut invalid_bool.as_slice()),
            Err(DecodeError::InvalidValue),
        );

        let mut invalid_text = Vec::new();
        put_atom(&mut invalid_text, &[0xff]).unwrap();
        assert_eq!(
            BoundedText::<8>::decode_atom(&mut invalid_text.as_slice()),
            Err(DecodeError::InvalidUtf8),
        );

        let mut option_fields = Vec::new();
        put_u64(&mut option_fields, 0);
        put_u64(&mut option_fields, 9);
        let mut encoded_option = Vec::new();
        put_compound_payload(&mut encoded_option, &option_fields).unwrap();
        assert_eq!(
            Option::<u64>::decode_atom(&mut encoded_option.as_slice()),
            Err(DecodeError::TrailingFields),
        );
    }
}
