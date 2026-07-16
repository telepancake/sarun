//! Streaming framing for the direct binary `ui.sock` protocol.
//!
//! Message identities and value codecs are generated from the Prolog
//! relation. This module owns only blocking stream I/O: the leading version
//! atom, exact one-atom reads under a hard cap, and typed decode/encode calls.

use std::io::{self, Read, Write};

use crate::generated_wire::{
    ActionSuccess, ConnectionMode, RequestEnvelope, SubscriptionEvent, TransportResponse,
    LIMIT_FRAME_BYTES, WIRE_PROTOCOL_VERSION,
};
use crate::wire::{put_u64, DecodeError, WireValue};

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn decode_error(error: DecodeError) -> io::Error {
    invalid_data(format!("invalid binary socket value: {error:?}"))
}

/// Read exactly one canonical tv atom without reading bytes belonging to the
/// next atom or a subsequent raw-stream handoff.
pub fn read_encoded_atom<R: Read>(reader: &mut R, maximum: usize) -> io::Result<Vec<u8>> {
    let mut first = [0u8; 1];
    reader.read_exact(&mut first)?;
    let tag = first[0];
    if tag < 0xc0 {
        return Ok(first.to_vec());
    }

    let (prefix, length) = if tag < 0xf8 {
        (Vec::new(), usize::from(tag - 0xc0))
    } else {
        let width = usize::from(tag - 0xf8);
        if width == 0 {
            return Err(invalid_data("zero-width long atom length"));
        }
        let mut bytes = vec![0u8; width];
        reader.read_exact(&mut bytes)?;
        if bytes.last() == Some(&0) {
            return Err(invalid_data("non-minimal long atom length"));
        }
        let mut length = 0usize;
        for (index, byte) in bytes.iter().enumerate() {
            length = length
                .checked_add(usize::from(*byte) << (index * 8))
                .ok_or_else(|| invalid_data("atom length overflow"))?;
        }
        if length <= 55 {
            return Err(invalid_data("non-canonical long atom length"));
        }
        (bytes, length)
    };

    if length > maximum {
        return Err(invalid_data(format!(
            "socket atom payload {length} exceeds limit {maximum}"
        )));
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload)?;
    let mut encoded = Vec::with_capacity(1 + prefix.len() + payload.len());
    encoded.push(tag);
    encoded.extend_from_slice(&prefix);
    encoded.extend_from_slice(&payload);
    Ok(encoded)
}

pub fn read_atom<R: Read, T: WireValue>(reader: &mut R) -> io::Result<T> {
    let encoded = read_encoded_atom(reader, LIMIT_FRAME_BYTES)?;
    let mut input = encoded.as_slice();
    let value = T::decode_atom(&mut input).map_err(decode_error)?;
    if !input.is_empty() {
        return Err(invalid_data("typed decoder left trailing atom bytes"));
    }
    Ok(value)
}

pub fn write_atom<W: Write, T: WireValue>(writer: &mut W, value: &T) -> io::Result<()> {
    let mut encoded = Vec::new();
    value.encode_atom(&mut encoded).map_err(decode_error)?;
    writer.write_all(&encoded)
}

pub fn write_request<W: Write>(writer: &mut W, request: &RequestEnvelope) -> io::Result<()> {
    write_versioned(writer, request)
}

pub fn write_versioned<W: Write, T: WireValue>(writer: &mut W, value: &T) -> io::Result<()> {
    let mut version = Vec::new();
    put_u64(&mut version, WIRE_PROTOCOL_VERSION);
    writer.write_all(&version)?;
    write_atom(writer, value)?;
    writer.flush()
}

pub fn read_request<R: Read>(reader: &mut R) -> io::Result<RequestEnvelope> {
    read_versioned(reader)
}

pub fn read_versioned<R: Read, T: WireValue>(reader: &mut R) -> io::Result<T> {
    let version: u64 = read_atom(reader)?;
    if version != WIRE_PROTOCOL_VERSION {
        return Err(invalid_data(format!(
            "unsupported ui.sock version {version}; expected {WIRE_PROTOCOL_VERSION}"
        )));
    }
    read_atom(reader)
}

pub fn write_mode<W: Write>(writer: &mut W, mode: &ConnectionMode) -> io::Result<()> {
    write_atom(writer, mode)?;
    writer.flush()
}

pub fn read_mode<R: Read>(reader: &mut R) -> io::Result<ConnectionMode> {
    read_atom(reader)
}

pub fn write_event<W: Write>(writer: &mut W, event: &SubscriptionEvent) -> io::Result<()> {
    write_atom(writer, event)?;
    writer.flush()
}

pub fn read_event<R: Read>(reader: &mut R) -> io::Result<SubscriptionEvent> {
    read_atom(reader)
}

pub fn action_reply(success: ActionSuccess) -> ConnectionMode {
    ConnectionMode::Reply {
        response: TransportResponse::Action { value: success },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated_wire::{ActionRequest, RequestEnvelope, TransportRequest};
    use std::io::Cursor;

    struct Fragmented<R> {
        inner: R,
        chunk: usize,
    }

    impl<R: Read> Read for Fragmented<R> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let limit = output.len().min(self.chunk);
            self.inner.read(&mut output[..limit])
        }
    }

    #[test]
    fn request_version_and_fragmentation_roundtrip() {
        for request in [
            RequestEnvelope::Action(ActionRequest::Ping),
            RequestEnvelope::Transport(TransportRequest::Subscribe),
        ] {
            let mut encoded = Vec::new();
            write_request(&mut encoded, &request).unwrap();
            let mut reader = Fragmented {
                inner: Cursor::new(encoded),
                chunk: 1,
            };
            assert_eq!(read_request(&mut reader).unwrap(), request);
        }
    }

    #[test]
    fn reply_and_event_atoms_accept_fragmented_streams() {
        let mode = action_reply(ActionSuccess::Ping { value: () });
        let mut encoded = Vec::new();
        write_mode(&mut encoded, &mode).unwrap();
        let mut reader = Fragmented {
            inner: Cursor::new(encoded),
            chunk: 1,
        };
        assert_eq!(read_mode(&mut reader).unwrap(), mode);

        let event = SubscriptionEvent::Pong;
        let mut encoded = Vec::new();
        write_event(&mut encoded, &event).unwrap();
        assert_eq!(read_event(&mut Cursor::new(encoded)).unwrap(), event);
    }

    #[test]
    fn malformed_or_noncanonical_atoms_fail_closed() {
        assert_eq!(
            read_encoded_atom(&mut Cursor::new(vec![0xf8]), LIMIT_FRAME_BYTES)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        // A one-byte wrapped payload is valid: compounds deliberately retain
        // their wrapper even when their encoded child is a self byte.
        assert_eq!(
            read_encoded_atom(&mut Cursor::new(vec![0xc1, 1]), LIMIT_FRAME_BYTES).unwrap(),
            vec![0xc1, 1]
        );
        assert_eq!(
            read_encoded_atom(&mut Cursor::new(vec![0xf9, 1, 0]), LIMIT_FRAME_BYTES)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        let mut wrong_version = Vec::new();
        put_u64(&mut wrong_version, WIRE_PROTOCOL_VERSION + 1);
        ActionRequest::Ping.encode_atom(&mut wrong_version).unwrap();
        assert_eq!(
            read_request(&mut Cursor::new(wrong_version))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
