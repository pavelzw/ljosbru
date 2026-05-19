use anyhow::{Context, bail};
use qrcode::{EcLevel, QrCode, Version, bits::Bits};

use super::transfer::{EncodedFrame, Transfer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EmissionPlan {
    batch_size: usize,
}

impl EmissionPlan {
    pub(super) fn single_frames() -> Self {
        Self { batch_size: 1 }
    }
}

pub(super) struct FrameEmitter<'a> {
    transfer: &'a Transfer,
    next_sequence: u32,
    batch_size: usize,
}

impl<'a> FrameEmitter<'a> {
    pub(super) fn new(transfer: &'a Transfer, plan: &EmissionPlan) -> Self {
        Self {
            transfer,
            next_sequence: 1,
            batch_size: plan.batch_size.max(1),
        }
    }

    pub(super) fn next_batch(&mut self) -> anyhow::Result<Option<Vec<EncodedFrame>>> {
        if self.next_sequence > self.transfer.total_chunks() {
            return Ok(None);
        }

        let batch_size = u32::try_from(self.batch_size).unwrap_or(u32::MAX);
        let start = self.next_sequence;
        let end = start
            .saturating_add(batch_size.saturating_sub(1))
            .min(self.transfer.total_chunks());
        let frames = (start..=end)
            .map(|sequence| self.transfer.build_frame(sequence))
            .collect::<anyhow::Result<Vec<_>>>()?;
        self.next_sequence = end + 1;
        Ok(Some(frames))
    }
}

pub(super) fn byte_mode_qr_code(frame: &[u8]) -> anyhow::Result<QrCode> {
    for version in 1..=40 {
        let mut bits = Bits::new(Version::Normal(version));
        if bits
            .push_byte_data(frame)
            .and_then(|_| bits.push_terminator(EcLevel::M))
            .is_ok()
        {
            return QrCode::with_bits(bits, EcLevel::M).context("failed to construct QR code");
        }
    }

    bail!("data too long")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::{
        encode::{Compression, transfer::Transfer},
        frame::HEADER_LEN,
    };

    fn test_transfer() -> Transfer {
        let tempdir = tempfile::tempdir().unwrap();
        let input_path = tempdir.path().join("input.bin");
        fs::write(&input_path, b"hello world").unwrap();
        Transfer::prepare(&input_path, HEADER_LEN + 5, Compression::None).unwrap()
    }

    #[test]
    fn emitter_returns_batches_in_order() {
        let transfer = test_transfer();
        let plan = EmissionPlan { batch_size: 2 };
        let mut emitter = FrameEmitter::new(&transfer, &plan);

        assert_eq!(
            emitter
                .next_batch()
                .unwrap()
                .unwrap()
                .iter()
                .map(|frame| frame.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            emitter
                .next_batch()
                .unwrap()
                .unwrap()
                .iter()
                .map(|frame| frame.sequence)
                .collect::<Vec<_>>(),
            vec![3]
        );
        assert!(emitter.next_batch().unwrap().is_none());
    }
}
