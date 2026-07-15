//! Frugal binary atoms shared by sarun's direct Rust transports.
//!
//! This is the Rust implementation of the format defined by
//! `tv/wire/wire.h`: self-byte scalars, one-byte inline lengths through 55
//! bytes, and bounded little-endian long lengths. Payloads are opaque bytes;
//! compounds are outer atoms containing a sequence of inner atoms.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    Truncated,
    LengthOverflow,
    TooLarge,
    IntegerTooWide,
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

/// Take one atom without copying. On failure `input` is not advanced.
pub fn get_atom<'a>(
    input: &mut &'a [u8],
    maximum_length: usize,
) -> Result<&'a [u8], DecodeError> {
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
        assert_eq!(integers, vec![0xc0, 1, 191, 0xc1, 192, 0xc1, 255, 0xc2, 0, 1]);
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
}
