use std::{fs, ops::Range, path::Path};

use anyhow::{Context, bail};

use super::Compression;
use crate::frame::{Frame, HEADER_LEN, MAX_FRAME_BYTES_PER_QR, build_frame};

#[derive(Debug)]
pub(super) struct Transfer {
    original_len: u64,
    encoded_len: u64,
    encoded: Vec<u8>,
    stream_hash: blake3::Hash,
    compression: Compression,
    chunks: Vec<Range<usize>>,
    total_chunks: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EncodedFrame {
    pub(super) sequence: u32,
    pub(super) total_chunks: u32,
    pub(super) payload_len: usize,
    pub(super) bytes: Vec<u8>,
}

impl Transfer {
    pub(super) fn prepare(
        filename: &Path,
        qr_size: usize,
        compression: Compression,
    ) -> anyhow::Result<Self> {
        Self::validate_qr_size(qr_size)?;

        let input = fs::read(filename)
            .with_context(|| format!("failed to read input file {}", filename.display()))?;
        let original_len = input.len() as u64;
        let encoded = apply_compression(input, &compression)?;
        let encoded_len = encoded.len() as u64;
        let stream_hash = blake3::hash(&encoded);
        let chunk_size = qr_size - HEADER_LEN;
        let chunks = chunk_ranges(encoded.len(), chunk_size)?;
        let total_chunks = chunks
            .len()
            .try_into()
            .context("too many QR chunks to encode")?;

        Ok(Self {
            original_len,
            encoded_len,
            encoded,
            stream_hash,
            compression,
            chunks,
            total_chunks,
        })
    }

    pub(super) fn validate_qr_size(qr_size: usize) -> anyhow::Result<()> {
        if qr_size == 0 {
            bail!("--qr-size must be greater than 0");
        }

        if qr_size <= HEADER_LEN {
            bail!(
                "--qr-size must be greater than {HEADER_LEN} byte(s) to leave room for QR payload data"
            );
        }

        if qr_size > MAX_FRAME_BYTES_PER_QR {
            bail!(
                "--qr-size must be at most {MAX_FRAME_BYTES_PER_QR} byte(s) with the current QR framing and error correction settings"
            );
        }

        Ok(())
    }

    pub(super) fn build_frame(&self, sequence: u32) -> anyhow::Result<EncodedFrame> {
        if sequence == 0 {
            bail!("QR frame sequence must be greater than 0");
        }
        if sequence > self.total_chunks {
            bail!(
                "QR frame sequence {sequence} exceeds total chunk count {}",
                self.total_chunks
            );
        }

        let range = self
            .chunks
            .get((sequence - 1) as usize)
            .context("QR frame sequence does not have a matching chunk")?;
        let bytes = build_frame(Frame {
            sequence,
            total_chunks: self.total_chunks,
            original_len: self.original_len,
            encoded_len: self.encoded_len,
            stream_hash: self.stream_hash,
            compression: self.compression.frame_compression(),
            chunk: self.encoded[range.clone()].to_vec(),
        })?;

        Ok(EncodedFrame {
            sequence,
            total_chunks: self.total_chunks,
            payload_len: range.len(),
            bytes,
        })
    }

    pub(super) fn original_len(&self) -> u64 {
        self.original_len
    }

    pub(super) fn encoded_len(&self) -> u64 {
        self.encoded_len
    }

    pub(super) fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    pub(super) fn total_chunks(&self) -> u32 {
        self.total_chunks
    }

    pub(super) fn filename_width(&self) -> usize {
        self.total_chunks.to_string().len().max(6)
    }
}

fn apply_compression(input: Vec<u8>, compression: &Compression) -> anyhow::Result<Vec<u8>> {
    match compression {
        Compression::None => Ok(input),
        Compression::Zstd(level) => zstd::stream::encode_all(input.as_slice(), *level as i32)
            .with_context(|| format!("failed to compress input with zstd level {level}")),
    }
}

fn chunk_ranges(len: usize, chunk_size: usize) -> anyhow::Result<Vec<Range<usize>>> {
    if chunk_size == 0 {
        bail!("chunk size must be greater than 0");
    }

    if len == 0 {
        return Ok(std::iter::once(0..0).collect());
    }

    let chunk_count = len.div_ceil(chunk_size);
    Ok((0..chunk_count)
        .map(|index| {
            let start = index * chunk_size;
            let end = (start + chunk_size).min(len);
            start..end
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use qrcode::{EcLevel, Version};

    use super::*;
    use crate::{
        encode::emit::byte_mode_qr_code,
        frame::{MAGIC, VERSION as FRAME_VERSION},
    };

    #[test]
    fn builds_expected_frame_header() {
        let tempdir = tempfile::tempdir().unwrap();
        let input_path = tempdir.path().join("input.bin");
        fs::write(&input_path, b"abcdef").unwrap();
        let transfer = Transfer::prepare(&input_path, HEADER_LEN + 3, Compression::None).unwrap();
        let frame = transfer.build_frame(2).unwrap().bytes;
        let stream_hash = blake3::hash(b"abcdef");

        assert_eq!(&frame[0..8], MAGIC);
        assert_eq!(frame[8], FRAME_VERSION);
        assert_eq!(frame[9], 0);
        assert_eq!(u16::from_be_bytes(frame[10..12].try_into().unwrap()), 72);
        assert_eq!(u32::from_be_bytes(frame[12..16].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(frame[16..20].try_into().unwrap()), 2);
        assert_eq!(u64::from_be_bytes(frame[20..28].try_into().unwrap()), 6);
        assert_eq!(u64::from_be_bytes(frame[28..36].try_into().unwrap()), 6);
        assert_eq!(&frame[36..68], stream_hash.as_bytes());
        assert_eq!(u32::from_be_bytes(frame[68..72].try_into().unwrap()), 3);
        assert_eq!(&frame[72..], b"def");
    }

    #[test]
    fn byte_mode_qr_accepts_maximum_frame_size() {
        let frame = vec![0_u8; MAX_FRAME_BYTES_PER_QR];
        let code = byte_mode_qr_code(&frame).unwrap();
        assert_eq!(code.version(), Version::Normal(40));
        assert_eq!(code.error_correction_level(), EcLevel::M);
    }

    #[test]
    fn chunks_exact_multiple_and_partial_tail() {
        assert_eq!(chunk_ranges(10, 5).unwrap(), vec![0..5, 5..10]);
        assert_eq!(chunk_ranges(11, 5).unwrap(), vec![0..5, 5..10, 10..11]);
    }

    #[test]
    fn empty_input_creates_one_empty_chunk() {
        assert_eq!(chunk_ranges(0, 5).unwrap(), vec![0..0]);
    }
}
