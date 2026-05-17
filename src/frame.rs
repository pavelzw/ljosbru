use std::fmt;

use anyhow::{Context, bail};

pub(crate) const MAGIC: &[u8; 8] = b"LJOSBRU1";
pub(crate) const VERSION: u8 = 1;
pub(crate) const HEADER_LEN: usize = 72;
pub(crate) const MAX_FRAME_BYTES_PER_QR: usize = 2331;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameCompression {
    None,
    Zstd,
}

impl FrameCompression {
    pub(crate) fn code(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Zstd => 1,
        }
    }

    fn from_code(code: u8) -> anyhow::Result<Self> {
        match code {
            0 => Ok(Self::None),
            1 => Ok(Self::Zstd),
            _ => bail!("unsupported compression code {code}"),
        }
    }
}

impl fmt::Display for FrameCompression {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("none"),
            Self::Zstd => formatter.write_str("zstd"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) sequence: u32,
    pub(crate) total_chunks: u32,
    pub(crate) original_len: u64,
    pub(crate) encoded_len: u64,
    pub(crate) stream_hash: blake3::Hash,
    pub(crate) compression: FrameCompression,
    pub(crate) chunk: Vec<u8>,
}

pub(crate) fn build_frame(frame: Frame) -> anyhow::Result<Vec<u8>> {
    if frame.sequence == 0 {
        bail!("QR frame sequence must be greater than 0");
    }

    if frame.total_chunks == 0 {
        bail!("QR frame total chunk count must be greater than 0");
    }

    if frame.sequence > frame.total_chunks {
        bail!(
            "QR frame sequence {} exceeds total chunk count {}",
            frame.sequence,
            frame.total_chunks
        );
    }

    let chunk_len: u32 = frame
        .chunk
        .len()
        .try_into()
        .context("QR chunk is too large")?;
    let mut bytes = Vec::with_capacity(HEADER_LEN + frame.chunk.len());
    bytes.extend_from_slice(MAGIC);
    bytes.push(VERSION);
    bytes.push(frame.compression.code());
    bytes.extend_from_slice(&(HEADER_LEN as u16).to_be_bytes());
    bytes.extend_from_slice(&frame.sequence.to_be_bytes());
    bytes.extend_from_slice(&frame.total_chunks.to_be_bytes());
    bytes.extend_from_slice(&frame.original_len.to_be_bytes());
    bytes.extend_from_slice(&frame.encoded_len.to_be_bytes());
    bytes.extend_from_slice(frame.stream_hash.as_bytes());
    bytes.extend_from_slice(&chunk_len.to_be_bytes());
    bytes.extend_from_slice(&frame.chunk);
    Ok(bytes)
}

pub(crate) fn parse_frame(bytes: &[u8]) -> anyhow::Result<Frame> {
    if bytes.len() < HEADER_LEN {
        bail!(
            "QR frame is too short: got {} byte(s), expected at least {HEADER_LEN}",
            bytes.len()
        );
    }

    if &bytes[0..8] != MAGIC {
        bail!("QR frame does not contain ljosbru magic");
    }

    let version = bytes[8];
    if version != VERSION {
        bail!("unsupported ljosbru frame version {version}");
    }

    let compression = FrameCompression::from_code(bytes[9])?;
    let header_len = u16::from_be_bytes(bytes[10..12].try_into().unwrap()) as usize;
    if header_len != HEADER_LEN {
        bail!("unsupported ljosbru frame header length {header_len}");
    }

    let sequence = u32::from_be_bytes(bytes[12..16].try_into().unwrap());
    let total_chunks = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let original_len = u64::from_be_bytes(bytes[20..28].try_into().unwrap());
    let encoded_len = u64::from_be_bytes(bytes[28..36].try_into().unwrap());
    let stream_hash =
        blake3::Hash::from_bytes(bytes[36..68].try_into().context("invalid stream hash")?);
    let chunk_len = u32::from_be_bytes(bytes[68..72].try_into().unwrap()) as usize;

    if sequence == 0 {
        bail!("QR frame sequence must be greater than 0");
    }

    if total_chunks == 0 {
        bail!("QR frame total chunk count must be greater than 0");
    }

    if sequence > total_chunks {
        bail!("QR frame sequence {sequence} exceeds total chunk count {total_chunks}");
    }

    let payload_len = bytes.len() - HEADER_LEN;
    if payload_len != chunk_len {
        bail!("QR frame chunk length mismatch: header says {chunk_len}, payload has {payload_len}");
    }

    Ok(Frame {
        sequence,
        total_chunks,
        original_len,
        encoded_len,
        stream_hash,
        compression,
        chunk: bytes[HEADER_LEN..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_built_frame() {
        let encoded = b"abcdef";
        let frame = Frame {
            sequence: 2,
            total_chunks: 3,
            original_len: 10,
            encoded_len: encoded.len() as u64,
            stream_hash: blake3::hash(encoded),
            compression: FrameCompression::Zstd,
            chunk: b"cd".to_vec(),
        };

        let bytes = build_frame(frame.clone()).unwrap();
        let parsed = parse_frame(&bytes).unwrap();

        assert_eq!(parsed, frame);
        assert_eq!(&bytes[0..8], MAGIC);
        assert_eq!(bytes[8], VERSION);
        assert_eq!(bytes[9], 1);
        assert_eq!(u16::from_be_bytes(bytes[10..12].try_into().unwrap()), 72);
    }

    #[test]
    fn rejects_invalid_frame_header() {
        assert!(
            parse_frame(b"short")
                .unwrap_err()
                .to_string()
                .contains("too short")
        );

        let frame = Frame {
            sequence: 1,
            total_chunks: 1,
            original_len: 1,
            encoded_len: 1,
            stream_hash: blake3::hash(b"a"),
            compression: FrameCompression::None,
            chunk: b"a".to_vec(),
        };
        let mut bytes = build_frame(frame).unwrap();

        bytes[0] = b'X';
        assert!(
            parse_frame(&bytes)
                .unwrap_err()
                .to_string()
                .contains("magic")
        );
        bytes[0] = MAGIC[0];

        bytes[8] = 2;
        assert!(
            parse_frame(&bytes)
                .unwrap_err()
                .to_string()
                .contains("version")
        );
        bytes[8] = VERSION;

        bytes[9] = 9;
        assert!(
            parse_frame(&bytes)
                .unwrap_err()
                .to_string()
                .contains("compression")
        );
    }

    #[test]
    fn rejects_invalid_sequence_and_chunk_length() {
        let frame = Frame {
            sequence: 1,
            total_chunks: 1,
            original_len: 1,
            encoded_len: 1,
            stream_hash: blake3::hash(b"a"),
            compression: FrameCompression::None,
            chunk: b"a".to_vec(),
        };
        let mut bytes = build_frame(frame).unwrap();

        bytes[15] = 0;
        assert!(
            parse_frame(&bytes)
                .unwrap_err()
                .to_string()
                .contains("sequence")
        );
        bytes[15] = 1;

        bytes[71] = 2;
        assert!(
            parse_frame(&bytes)
                .unwrap_err()
                .to_string()
                .contains("chunk length mismatch")
        );
    }
}
