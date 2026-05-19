use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use anyhow::{Context, bail};
use image::Luma;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;

use super::{QrSink, emit::byte_mode_qr_code, transfer::EncodedFrame};
use crate::progress::human_bytes_per_second;

const QR_MODULE_PIXELS: u32 = 8;

#[derive(Debug)]
pub(super) struct PngSink {
    output: PathBuf,
    qr_size: usize,
    filename_width: usize,
    confirmation_mode: ConfirmationMode,
    jobs: Vec<PngJob>,
}

#[derive(Debug)]
struct PngJob {
    frame: EncodedFrame,
    output_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmationMode {
    Prompt,
    AssumeYes,
}

impl ConfirmationMode {
    fn from_assume_yes(assume_yes: bool) -> Self {
        if assume_yes {
            Self::AssumeYes
        } else {
            Self::Prompt
        }
    }
}

impl PngSink {
    pub(super) fn new(
        output: PathBuf,
        qr_size: usize,
        filename_width: usize,
        assume_cleanup: bool,
    ) -> Self {
        Self {
            output,
            qr_size,
            filename_width,
            confirmation_mode: ConfirmationMode::from_assume_yes(assume_cleanup),
            jobs: Vec::new(),
        }
    }

    pub(super) fn prepare(&mut self) -> anyhow::Result<()> {
        prepare_output_dir(&self.output, self.confirmation_mode)
    }

    pub(super) fn emit_batch(&mut self, frames: Vec<EncodedFrame>) -> anyhow::Result<()> {
        for frame in frames {
            let output_path = self.output.join(format!(
                "{:0width$}.png",
                frame.sequence,
                width = self.filename_width
            ));
            self.jobs.push(PngJob { frame, output_path });
        }
        Ok(())
    }

    pub(super) fn finish(&mut self) -> anyhow::Result<()> {
        write_png_jobs(&self.jobs, self.qr_size)
    }
}

impl QrSink for PngSink {
    fn prepare(&mut self) -> anyhow::Result<()> {
        PngSink::prepare(self)
    }

    fn emit_batch(&mut self, frames: Vec<EncodedFrame>) -> anyhow::Result<()> {
        PngSink::emit_batch(self, frames)
    }

    fn finish(&mut self) -> anyhow::Result<()> {
        PngSink::finish(self)
    }
}

fn write_png_jobs(jobs: &[PngJob], qr_size: usize) -> anyhow::Result<()> {
    let progress = encode_progress_bar(jobs.len())?;
    let started_at = Instant::now();
    let payload_bytes_written = AtomicU64::new(0);

    let write_result: anyhow::Result<()> = jobs.par_iter().try_for_each(|job| {
        write_qr_png(
            &job.frame.bytes,
            &job.output_path,
            qr_size,
            job.frame.sequence,
            job.frame.total_chunks,
        )?;
        let bytes_written = payload_bytes_written
            .fetch_add(job.frame.payload_len as u64, Ordering::Relaxed)
            + job.frame.payload_len as u64;
        progress.set_message(human_bytes_per_second(
            bytes_written as f64 / started_at.elapsed().as_secs_f64().max(f64::EPSILON),
        ));
        progress.inc(1);
        Ok(())
    });
    progress.finish_and_clear();
    write_result
}

fn encode_progress_bar(total_chunks: usize) -> anyhow::Result<ProgressBar> {
    let progress = ProgressBar::new(total_chunks as u64);
    progress.set_style(
        ProgressStyle::with_template(
            "Writing QR codes [{bar:40.cyan/blue}] {pos}/{len} {msg} ({elapsed_precise})",
        )
        .context("failed to configure progress bar")?
        .progress_chars("##-"),
    );
    progress.set_message(human_bytes_per_second(0.0));
    Ok(progress)
}

fn write_qr_png(
    frame: &[u8],
    output_path: &Path,
    qr_size: usize,
    sequence: u32,
    total_chunks: u32,
) -> anyhow::Result<()> {
    let code = byte_mode_qr_code(frame).with_context(|| {
        format!(
            "failed to fit chunk {sequence}/{total_chunks} into a QR code; reduce --qr-size from {qr_size} byte(s)"
        )
    })?;
    let image = code
        .render::<Luma<u8>>()
        .quiet_zone(true)
        .module_dimensions(QR_MODULE_PIXELS, QR_MODULE_PIXELS)
        .build();
    image
        .save(output_path)
        .with_context(|| format!("failed to write QR image {}", output_path.display()))
}

fn prepare_output_dir(output: &Path, confirmation_mode: ConfirmationMode) -> anyhow::Result<()> {
    prepare_output_dir_with_confirmation(output, confirmation_mode, confirm_cleanup_interactive)
}

fn prepare_output_dir_with_confirmation<F>(
    output: &Path,
    confirmation_mode: ConfirmationMode,
    confirm_cleanup: F,
) -> anyhow::Result<()>
where
    F: FnOnce(&Path, usize) -> anyhow::Result<bool>,
{
    fs::create_dir_all(output)
        .with_context(|| format!("failed to create output directory {}", output.display()))?;

    let png_files = existing_png_files(output)?;
    if png_files.is_empty() {
        return Ok(());
    }

    let should_delete = match confirmation_mode {
        ConfirmationMode::Prompt => confirm_cleanup(output, png_files.len())?,
        ConfirmationMode::AssumeYes => true,
    };
    if !should_delete {
        bail!(
            "refusing to delete existing PNG files in {}; choose an empty output directory or confirm cleanup",
            output.display()
        );
    }

    for path in png_files {
        fs::remove_file(&path)
            .with_context(|| format!("failed to delete existing PNG file {}", path.display()))?;
    }

    Ok(())
}

fn existing_png_files(output: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut png_files = Vec::new();
    for entry in fs::read_dir(output)
        .with_context(|| format!("failed to read output directory {}", output.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", output.display()))?;
        if !entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?
            .is_file()
        {
            continue;
        }

        let path = entry.path();
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
        {
            png_files.push(path);
        }
    }
    png_files.sort();
    Ok(png_files)
}

fn confirm_cleanup_interactive(output: &Path, count: usize) -> anyhow::Result<bool> {
    use std::io::{self, IsTerminal};

    let stdin = io::stdin();
    if !stdin.is_terminal() {
        bail!(
            "found {count} existing PNG file(s) in {}, but stdin is not interactive",
            output.display()
        );
    }

    let mut stdin = stdin.lock();
    let mut stdout = io::stdout().lock();
    prompt_cleanup_confirmation(output, count, &mut stdin, &mut stdout)
}

fn prompt_cleanup_confirmation<R, W>(
    output: &Path,
    count: usize,
    reader: &mut R,
    writer: &mut W,
) -> anyhow::Result<bool>
where
    R: std::io::BufRead,
    W: std::io::Write,
{
    write!(
        writer,
        "Found {count} existing PNG file(s) in {}. Delete them before continuing? [y/N] ",
        output.display()
    )?;
    writer.flush()?;

    let mut response = String::new();
    reader.read_line(&mut response)?;
    let response = response.trim();
    Ok(response.eq_ignore_ascii_case("y") || response.eq_ignore_ascii_case("yes"))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::{
        encode::{
            Compression,
            emit::{EmissionPlan, FrameEmitter},
            transfer::Transfer,
        },
        frame::HEADER_LEN,
    };

    #[test]
    fn cleanup_prompt_defaults_to_no() {
        let mut input = Cursor::new(b"\n");
        let mut output = Vec::new();
        assert!(
            !prompt_cleanup_confirmation(Path::new("output"), 1, &mut input, &mut output).unwrap()
        );

        let mut input = Cursor::new(b"yes\n");
        assert!(
            prompt_cleanup_confirmation(Path::new("output"), 1, &mut input, &mut Vec::new())
                .unwrap()
        );
    }

    #[test]
    fn cleanup_removes_only_direct_png_files_after_confirmation() {
        let tempdir = tempfile::tempdir().unwrap();
        let output = tempdir.path();
        let stale_png = output.join("stale.png");
        let keep_txt = output.join("keep.txt");
        let nested = output.join("nested");
        let nested_png = nested.join("nested.png");
        fs::write(&stale_png, b"old").unwrap();
        fs::write(&keep_txt, b"keep").unwrap();
        fs::create_dir(&nested).unwrap();
        fs::write(&nested_png, b"nested").unwrap();

        let error =
            prepare_output_dir_with_confirmation(output, ConfirmationMode::Prompt, |_, count| {
                assert_eq!(count, 1);
                Ok(false)
            })
            .unwrap_err();
        assert!(error.to_string().contains("refusing to delete"));
        assert!(stale_png.exists());

        prepare_output_dir_with_confirmation(output, ConfirmationMode::Prompt, |_, count| {
            assert_eq!(count, 1);
            Ok(true)
        })
        .unwrap();

        assert!(!stale_png.exists());
        assert!(keep_txt.exists());
        assert!(nested_png.exists());
    }

    #[test]
    fn png_sink_writes_frames_from_the_shared_loop() {
        let tempdir = tempfile::tempdir().unwrap();
        let input_path = tempdir.path().join("input.bin");
        let output = tempdir.path().join("out");
        fs::create_dir(&output).unwrap();
        fs::write(&input_path, b"hello world").unwrap();
        let transfer = Transfer::prepare(&input_path, HEADER_LEN + 5, Compression::None).unwrap();
        let plan = EmissionPlan::single_frames();
        let mut emitter = FrameEmitter::new(&transfer, &plan);
        let mut sink = PngSink::new(
            output.clone(),
            HEADER_LEN + 5,
            transfer.filename_width(),
            true,
        );
        let mut emitted = 0;

        while let Some(batch) = emitter.next_batch().unwrap() {
            emitted += batch.len();
            sink.emit_batch(batch).unwrap();
        }
        sink.finish().unwrap();

        assert_eq!(emitted, 3);
        assert!(output.join("000001.png").exists());
        assert!(output.join("000002.png").exists());
        assert!(output.join("000003.png").exists());
    }
}
