use std::{
    fmt, fs,
    io::{self, BufRead, IsTerminal, Write},
    ops::Range,
    path::{Path, PathBuf},
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use anyhow::{Context, bail};
use clap::Args;
use image::Luma;
use indicatif::{ProgressBar, ProgressStyle};
use qrcode::{Color, EcLevel, QrCode, Version, bits::Bits};
use rayon::prelude::*;

use crate::frame::{Frame, FrameCompression, HEADER_LEN, MAX_FRAME_BYTES_PER_QR, build_frame};
use crate::progress::human_bytes_per_second;

const DEFAULT_ZSTD_LEVEL: u32 = 3;
const MIN_ZSTD_LEVEL: u32 = 1;
const MAX_ZSTD_LEVEL: u32 = 22;
const QR_MODULE_PIXELS: u32 = 8;
const TERMINAL_QUIET_ZONE_MODULES: usize = 4;
const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BLACK_ON_BLACK: &str = "\x1b[30;40m";
const ANSI_BLACK_ON_WHITE: &str = "\x1b[30;107m";
const ANSI_WHITE_ON_BLACK: &str = "\x1b[97;40m";
const ANSI_WHITE_ON_WHITE: &str = "\x1b[97;107m";
const ANSI_CLEAR_SCREEN: &str = "\x1b[2J\x1b[H";

#[derive(Debug, Args)]
pub struct EncodeArgs {
    filename: PathBuf,

    #[arg(
        long,
        value_name = "bytes",
        help = "Maximum QR data size in bytes, including ljosbru framing"
    )]
    qr_size: usize,

    #[arg(long, value_name = "zstd:<level>|none", default_value = "none")]
    compression: Compression,

    #[arg(
        long,
        value_name = "directory",
        default_value = "./ljosbru-output/",
        help = "Directory for generated PNGs; ignored with --terminal"
    )]
    output: PathBuf,

    #[arg(long, help = "Print QR codes to stdout instead of writing PNG files")]
    terminal: bool,

    #[arg(long, help = "Delete existing output PNG files without prompting")]
    yes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Compression {
    None,
    Zstd(u32),
}

impl Compression {
    fn frame_compression(&self) -> FrameCompression {
        match self {
            Self::None => FrameCompression::None,
            Self::Zstd(_) => FrameCompression::Zstd,
        }
    }
}

impl fmt::Display for Compression {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("none"),
            Self::Zstd(level) => write!(formatter, "zstd:{level}"),
        }
    }
}

impl FromStr for Compression {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "none" {
            return Ok(Self::None);
        }

        if value == "zstd" {
            return Ok(Self::Zstd(DEFAULT_ZSTD_LEVEL));
        }

        let Some(level) = value.strip_prefix("zstd:") else {
            return Err("expected `none`, `zstd`, or `zstd:<level>`".to_owned());
        };

        let level = level
            .parse()
            .map_err(|_| "expected zstd compression level to be an integer".to_owned())?;

        if !(MIN_ZSTD_LEVEL..=MAX_ZSTD_LEVEL).contains(&level) {
            return Err(format!(
                "expected zstd compression level to be between {MIN_ZSTD_LEVEL} and {MAX_ZSTD_LEVEL}"
            ));
        }

        Ok(Self::Zstd(level))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct EncodeSummary {
    original_len: u64,
    encoded_len: u64,
    chunk_count: usize,
    compression: Compression,
    output: PathBuf,
    terminal: bool,
}

type CleanupConfirmation = fn(&Path, usize) -> anyhow::Result<bool>;

pub fn encode(args: EncodeArgs) -> anyhow::Result<()> {
    let confirm_cleanup = cleanup_confirmation_for(&args);
    let summary = encode_with_cleanup(args, confirm_cleanup)?;
    let message = format!(
        "Encoded {} input byte(s) into {} payload byte(s) across {} QR code(s) with {} compression",
        summary.original_len, summary.encoded_len, summary.chunk_count, summary.compression,
    );
    if summary.terminal {
        eprintln!("{message} to terminal");
    } else {
        println!("{message} in {}", summary.output.display());
    }
    Ok(())
}

fn cleanup_confirmation_for(args: &EncodeArgs) -> CleanupConfirmation {
    if args.yes {
        confirm_cleanup_yes
    } else {
        confirm_cleanup_interactive
    }
}

fn encode_with_cleanup<F>(args: EncodeArgs, confirm_cleanup: F) -> anyhow::Result<EncodeSummary>
where
    F: FnOnce(&Path, usize) -> anyhow::Result<bool>,
{
    let stdout = io::stdout();
    let stdin = io::stdin();
    let wait_for_enter = args.terminal && stdin.is_terminal() && stdout.is_terminal();
    let mut stdout = stdout.lock();
    let mut stdin = stdin.lock();
    encode_with_cleanup_and_terminal_io(
        args,
        confirm_cleanup,
        &mut stdout,
        &mut stdin,
        wait_for_enter,
    )
}

#[cfg(test)]
fn encode_with_cleanup_and_terminal_writer<F, W>(
    args: EncodeArgs,
    confirm_cleanup: F,
    terminal_writer: &mut W,
) -> anyhow::Result<EncodeSummary>
where
    F: FnOnce(&Path, usize) -> anyhow::Result<bool>,
    W: Write,
{
    let mut terminal_input = io::empty();
    encode_with_cleanup_and_terminal_io(
        args,
        confirm_cleanup,
        terminal_writer,
        &mut terminal_input,
        false,
    )
}

fn encode_with_cleanup_and_terminal_io<F, W, R>(
    args: EncodeArgs,
    confirm_cleanup: F,
    terminal_writer: &mut W,
    terminal_input: &mut R,
    wait_for_enter: bool,
) -> anyhow::Result<EncodeSummary>
where
    F: FnOnce(&Path, usize) -> anyhow::Result<bool>,
    W: Write,
    R: BufRead,
{
    if args.qr_size == 0 {
        bail!("--qr-size must be greater than 0");
    }

    if args.qr_size <= HEADER_LEN {
        bail!(
            "--qr-size must be greater than {HEADER_LEN} byte(s) to leave room for QR payload data"
        );
    }

    if args.qr_size > MAX_FRAME_BYTES_PER_QR {
        bail!(
            "--qr-size must be at most {MAX_FRAME_BYTES_PER_QR} byte(s) with the current QR framing and error correction settings"
        );
    }

    if !args.terminal {
        prepare_output_dir(&args.output, confirm_cleanup)?;
    }

    let input = fs::read(&args.filename)
        .with_context(|| format!("failed to read input file {}", args.filename.display()))?;
    let original_len = input.len() as u64;
    let compression = args.compression.clone();
    let encoded = apply_compression(input, &compression)?;
    let encoded_len = encoded.len() as u64;
    let stream_hash = blake3::hash(&encoded);
    let chunk_size = args.qr_size - HEADER_LEN;
    let chunks = chunk_ranges(encoded.len(), chunk_size)?;
    let total_chunks: u32 = chunks
        .len()
        .try_into()
        .context("too many QR chunks to encode")?;
    let filename_width = chunks.len().to_string().len().max(6);

    if args.terminal {
        write_terminal_qr_frames(
            terminal_writer,
            terminal_input,
            wait_for_enter,
            &encoded,
            &chunks,
            original_len,
            encoded_len,
            stream_hash,
            compression.frame_compression(),
            args.qr_size,
            total_chunks,
        )?;
    } else {
        write_png_qr_frames(
            &args.output,
            &encoded,
            &chunks,
            original_len,
            encoded_len,
            stream_hash,
            compression.frame_compression(),
            args.qr_size,
            total_chunks,
            filename_width,
        )?;
    }

    Ok(EncodeSummary {
        original_len,
        encoded_len,
        chunk_count: chunks.len(),
        compression,
        output: args.output,
        terminal: args.terminal,
    })
}

#[allow(clippy::too_many_arguments)]
fn write_png_qr_frames(
    output: &Path,
    encoded: &[u8],
    chunks: &[Range<usize>],
    original_len: u64,
    encoded_len: u64,
    stream_hash: blake3::Hash,
    compression: FrameCompression,
    qr_size: usize,
    total_chunks: u32,
    filename_width: usize,
) -> anyhow::Result<()> {
    let progress = encode_progress_bar(chunks.len())?;
    let started_at = Instant::now();
    let payload_bytes_written = AtomicU64::new(0);

    let write_result: anyhow::Result<()> =
        chunks
            .par_iter()
            .enumerate()
            .try_for_each(|(index, range)| {
                let sequence: u32 = (index + 1)
                    .try_into()
                    .context("too many QR chunks to encode")?;
                let frame = build_frame(Frame {
                    sequence,
                    total_chunks,
                    original_len,
                    encoded_len,
                    stream_hash,
                    compression,
                    chunk: encoded[range.clone()].to_vec(),
                })?;
                let output_path = output.join(format!("{sequence:0filename_width$}.png"));
                write_qr_png(&frame, &output_path, qr_size, sequence, total_chunks)?;
                let bytes_written = payload_bytes_written
                    .fetch_add(range.len() as u64, Ordering::Relaxed)
                    + range.len() as u64;
                progress.set_message(human_bytes_per_second(
                    bytes_written as f64 / started_at.elapsed().as_secs_f64().max(f64::EPSILON),
                ));
                progress.inc(1);
                Ok(())
            });
    progress.finish_and_clear();
    write_result
}

#[allow(clippy::too_many_arguments)]
fn write_terminal_qr_frames<W>(
    writer: &mut W,
    input: &mut impl BufRead,
    wait_for_enter: bool,
    encoded: &[u8],
    chunks: &[Range<usize>],
    original_len: u64,
    encoded_len: u64,
    stream_hash: blake3::Hash,
    compression: FrameCompression,
    qr_size: usize,
    total_chunks: u32,
) -> anyhow::Result<()>
where
    W: Write,
{
    for (index, range) in chunks.iter().enumerate() {
        let sequence: u32 = (index + 1)
            .try_into()
            .context("too many QR chunks to encode")?;
        let frame = build_frame(Frame {
            sequence,
            total_chunks,
            original_len,
            encoded_len,
            stream_hash,
            compression,
            chunk: encoded[range.clone()].to_vec(),
        })?;
        if wait_for_enter {
            write!(writer, "{ANSI_CLEAR_SCREEN}").context("failed to clear terminal")?;
        } else if index > 0 {
            writeln!(writer).context("failed to write terminal QR separator")?;
        }
        writeln!(writer, "Frame {sequence}/{total_chunks}")
            .context("failed to write terminal QR frame label")?;
        write_qr_terminal(&frame, writer, qr_size, sequence, total_chunks)?;
        if wait_for_enter && sequence < total_chunks {
            prompt_next_terminal_frame(input, writer, sequence + 1, total_chunks)?;
        }
    }
    writer.flush().context("failed to flush terminal QR output")
}

fn prompt_next_terminal_frame<W>(
    input: &mut impl BufRead,
    writer: &mut W,
    next_sequence: u32,
    total_chunks: u32,
) -> anyhow::Result<()>
where
    W: Write,
{
    write!(
        writer,
        "Press Enter for frame {next_sequence}/{total_chunks}..."
    )
    .context("failed to write terminal QR prompt")?;
    writer
        .flush()
        .context("failed to flush terminal QR prompt")?;

    let mut response = String::new();
    input
        .read_line(&mut response)
        .context("failed to read terminal QR prompt response")?;
    writeln!(writer).context("failed to finish terminal QR prompt")
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

fn write_qr_terminal<W>(
    frame: &[u8],
    writer: &mut W,
    qr_size: usize,
    sequence: u32,
    total_chunks: u32,
) -> anyhow::Result<()>
where
    W: Write,
{
    let code = byte_mode_qr_code(frame).with_context(|| {
        format!(
            "failed to fit chunk {sequence}/{total_chunks} into a QR code; reduce --qr-size from {qr_size} byte(s)"
        )
    })?;
    write_terminal_qr_code(&code, writer).context("failed to write terminal QR code")
}

fn write_terminal_qr_code<W>(code: &QrCode, writer: &mut W) -> io::Result<()>
where
    W: Write,
{
    let module_width = code.width();
    let output_width = module_width + (TERMINAL_QUIET_ZONE_MODULES * 2);
    let output_height = output_width;

    for top_y in (0..output_height).step_by(2) {
        for x in 0..output_width {
            let top = terminal_module_color(code, x, top_y);
            let bottom = if top_y + 1 < output_height {
                terminal_module_color(code, x, top_y + 1)
            } else {
                Color::Light
            };
            writer.write_all(terminal_cell_style(top, bottom).as_bytes())?;
            write!(writer, "\u{2580}")?;
        }
        writeln!(writer, "{ANSI_RESET}")?;
    }

    Ok(())
}

fn terminal_module_color(code: &QrCode, x: usize, y: usize) -> Color {
    let module_width = code.width();
    if x < TERMINAL_QUIET_ZONE_MODULES
        || y < TERMINAL_QUIET_ZONE_MODULES
        || x >= TERMINAL_QUIET_ZONE_MODULES + module_width
        || y >= TERMINAL_QUIET_ZONE_MODULES + module_width
    {
        return Color::Light;
    }

    code[(
        x - TERMINAL_QUIET_ZONE_MODULES,
        y - TERMINAL_QUIET_ZONE_MODULES,
    )]
}

fn terminal_cell_style(top: Color, bottom: Color) -> &'static str {
    match (top, bottom) {
        (Color::Dark, Color::Dark) => ANSI_BLACK_ON_BLACK,
        (Color::Dark, Color::Light) => ANSI_BLACK_ON_WHITE,
        (Color::Light, Color::Dark) => ANSI_WHITE_ON_BLACK,
        (Color::Light, Color::Light) => ANSI_WHITE_ON_WHITE,
    }
}

fn byte_mode_qr_code(frame: &[u8]) -> anyhow::Result<QrCode> {
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

fn prepare_output_dir<F>(output: &Path, confirm_cleanup: F) -> anyhow::Result<()>
where
    F: FnOnce(&Path, usize) -> anyhow::Result<bool>,
{
    fs::create_dir_all(output)
        .with_context(|| format!("failed to create output directory {}", output.display()))?;

    let png_files = existing_png_files(output)?;
    if png_files.is_empty() {
        return Ok(());
    }

    if !confirm_cleanup(output, png_files.len())? {
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

fn confirm_cleanup_yes(_output: &Path, _count: usize) -> anyhow::Result<bool> {
    Ok(true)
}

fn confirm_cleanup_interactive(output: &Path, count: usize) -> anyhow::Result<bool> {
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
    R: BufRead,
    W: Write,
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

    use clap::Parser;

    use super::*;

    #[derive(Debug, Parser)]
    struct EncodeCli {
        #[command(flatten)]
        args: EncodeArgs,
    }

    #[test]
    fn parses_compression_modes() {
        assert_eq!("none".parse::<Compression>().unwrap(), Compression::None);
        assert_eq!(
            "zstd".parse::<Compression>().unwrap(),
            Compression::Zstd(DEFAULT_ZSTD_LEVEL)
        );
        assert_eq!(
            "zstd:3".parse::<Compression>().unwrap(),
            Compression::Zstd(3)
        );
    }

    #[test]
    fn rejects_invalid_compression_modes() {
        assert!("gzip".parse::<Compression>().is_err());
        assert!("zstd:".parse::<Compression>().is_err());
        assert!("zstd:abc".parse::<Compression>().is_err());
        assert!("zstd:0".parse::<Compression>().is_err());
        assert!("zstd:23".parse::<Compression>().is_err());
    }

    #[test]
    fn encode_cli_defaults_to_no_compression() {
        let args = EncodeCli::try_parse_from(["encode", "input.bin", "--qr-size", "128"])
            .unwrap()
            .args;
        assert_eq!(args.compression, Compression::None);
        assert!(!args.terminal);
        assert!(!args.yes);
    }

    #[test]
    fn encode_cli_accepts_terminal_output() {
        let args =
            EncodeCli::try_parse_from(["encode", "input.bin", "--qr-size", "128", "--terminal"])
                .unwrap()
                .args;
        assert!(args.terminal);
    }

    #[test]
    fn encode_cli_yes_selects_auto_cleanup_confirmation() {
        let args = EncodeCli::try_parse_from(["encode", "input.bin", "--qr-size", "128", "--yes"])
            .unwrap();
        let args = args.args;

        assert!(args.yes);
        assert!(cleanup_confirmation_for(&args)(Path::new("output"), 1).unwrap());
    }

    #[test]
    fn builds_expected_frame_header() {
        let encoded = b"abcdef";
        let stream_hash = blake3::hash(encoded);
        let frame = build_frame(Frame {
            sequence: 2,
            total_chunks: 3,
            original_len: 10,
            encoded_len: encoded.len() as u64,
            stream_hash,
            compression: Compression::Zstd(3).frame_compression(),
            chunk: b"cd".to_vec(),
        })
        .unwrap();

        assert_eq!(&frame[0..8], crate::frame::MAGIC);
        assert_eq!(frame[8], crate::frame::VERSION);
        assert_eq!(frame[9], 1);
        assert_eq!(u16::from_be_bytes(frame[10..12].try_into().unwrap()), 72);
        assert_eq!(u32::from_be_bytes(frame[12..16].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(frame[16..20].try_into().unwrap()), 3);
        assert_eq!(u64::from_be_bytes(frame[20..28].try_into().unwrap()), 10);
        assert_eq!(
            u64::from_be_bytes(frame[28..36].try_into().unwrap()),
            encoded.len() as u64
        );
        assert_eq!(&frame[36..68], stream_hash.as_bytes());
        assert_eq!(u32::from_be_bytes(frame[68..72].try_into().unwrap()), 2);
        assert_eq!(&frame[72..], b"cd");
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

    #[test]
    fn rejects_too_large_qr_size_before_cleanup() {
        let tempdir = tempfile::tempdir().unwrap();
        let input_path = tempdir.path().join("input.bin");
        let output = tempdir.path().join("out");
        fs::create_dir(&output).unwrap();
        fs::write(&input_path, b"hello").unwrap();
        fs::write(output.join("stale.png"), b"old").unwrap();

        let error = encode_with_cleanup(
            EncodeArgs {
                filename: input_path,
                qr_size: MAX_FRAME_BYTES_PER_QR + 1,
                compression: Compression::None,
                output: output.clone(),
                terminal: false,
                yes: false,
            },
            |_, _| panic!("cleanup should not be requested after qr-size validation fails"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("must be at most 2331"));
        assert!(output.join("stale.png").exists());
    }

    #[test]
    fn rejects_qr_size_without_payload_room_before_cleanup() {
        let tempdir = tempfile::tempdir().unwrap();
        let input_path = tempdir.path().join("input.bin");
        let output = tempdir.path().join("out");
        fs::create_dir(&output).unwrap();
        fs::write(&input_path, b"hello").unwrap();
        fs::write(output.join("stale.png"), b"old").unwrap();

        let error = encode_with_cleanup(
            EncodeArgs {
                filename: input_path,
                qr_size: HEADER_LEN,
                compression: Compression::None,
                output: output.clone(),
                terminal: false,
                yes: false,
            },
            |_, _| panic!("cleanup should not be requested after qr-size validation fails"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("must be greater than 72"));
        assert!(output.join("stale.png").exists());
    }

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

        let error = prepare_output_dir(output, |_, count| {
            assert_eq!(count, 1);
            Ok(false)
        })
        .unwrap_err();
        assert!(error.to_string().contains("refusing to delete"));
        assert!(stale_png.exists());

        prepare_output_dir(output, |_, count| {
            assert_eq!(count, 1);
            Ok(true)
        })
        .unwrap();

        assert!(!stale_png.exists());
        assert!(keep_txt.exists());
        assert!(nested_png.exists());
    }

    #[test]
    fn encode_writes_numbered_pngs_and_preserves_non_pngs() {
        let tempdir = tempfile::tempdir().unwrap();
        let input_path = tempdir.path().join("input.bin");
        let output = tempdir.path().join("out");
        fs::create_dir(&output).unwrap();
        fs::write(&input_path, b"hello world").unwrap();
        fs::write(output.join("stale.png"), b"old").unwrap();
        fs::write(output.join("keep.txt"), b"keep").unwrap();

        let summary = encode_with_cleanup(
            EncodeArgs {
                filename: input_path,
                qr_size: HEADER_LEN + 5,
                compression: Compression::None,
                output: output.clone(),
                terminal: false,
                yes: false,
            },
            |_, count| {
                assert_eq!(count, 1);
                Ok(true)
            },
        )
        .unwrap();

        assert_eq!(summary.original_len, 11);
        assert_eq!(summary.encoded_len, 11);
        assert_eq!(summary.chunk_count, 3);
        assert!(!output.join("stale.png").exists());
        assert!(output.join("keep.txt").exists());
        assert!(output.join("000001.png").exists());
        assert!(output.join("000002.png").exists());
        assert!(output.join("000003.png").exists());
    }

    #[test]
    fn terminal_qr_renderer_uses_ansi_half_blocks_with_quiet_zone() {
        let code = byte_mode_qr_code(b"hello").unwrap();
        let mut output = Vec::new();

        write_terminal_qr_code(&code, &mut output).unwrap();

        let output = String::from_utf8(output).unwrap();
        let expected_lines = (code.width() + TERMINAL_QUIET_ZONE_MODULES * 2).div_ceil(2);
        assert_eq!(output.lines().count(), expected_lines);
        assert!(output.contains("\u{2580}"));
        assert!(output.contains(ANSI_WHITE_ON_WHITE));
        assert!(output.contains(ANSI_RESET));
    }

    #[test]
    fn terminal_encode_skips_png_cleanup_and_writes_stdout() {
        let tempdir = tempfile::tempdir().unwrap();
        let input_path = tempdir.path().join("input.bin");
        let output = tempdir.path().join("out");
        fs::create_dir(&output).unwrap();
        fs::write(&input_path, b"hello world").unwrap();
        fs::write(output.join("stale.png"), b"old").unwrap();

        let mut terminal_output = Vec::new();
        let summary = encode_with_cleanup_and_terminal_writer(
            EncodeArgs {
                filename: input_path,
                qr_size: HEADER_LEN + 5,
                compression: Compression::None,
                output: output.clone(),
                terminal: true,
                yes: false,
            },
            |_, _| panic!("terminal output should not clean the PNG output directory"),
            &mut terminal_output,
        )
        .unwrap();

        assert!(summary.terminal);
        assert_eq!(summary.chunk_count, 3);
        assert!(output.join("stale.png").exists());
        assert!(!output.join("000001.png").exists());
        assert!(!terminal_output.is_empty());
        let terminal_output = String::from_utf8(terminal_output).unwrap();
        assert!(terminal_output.contains("Frame 1/3"));
        assert!(terminal_output.contains("Frame 2/3"));
        assert!(terminal_output.contains("Frame 3/3"));
        assert!(!terminal_output.contains("Press Enter"));
    }

    #[test]
    fn interactive_terminal_encode_prompts_between_frames() {
        let tempdir = tempfile::tempdir().unwrap();
        let input_path = tempdir.path().join("input.bin");
        let output = tempdir.path().join("out");
        fs::write(&input_path, b"hello world").unwrap();

        let mut terminal_output = Vec::new();
        let mut terminal_input = Cursor::new(b"\n\n");
        let summary = encode_with_cleanup_and_terminal_io(
            EncodeArgs {
                filename: input_path,
                qr_size: HEADER_LEN + 5,
                compression: Compression::None,
                output,
                terminal: true,
                yes: false,
            },
            |_, _| panic!("terminal output should not clean the PNG output directory"),
            &mut terminal_output,
            &mut terminal_input,
            true,
        )
        .unwrap();

        assert!(summary.terminal);
        assert_eq!(summary.chunk_count, 3);
        let terminal_output = String::from_utf8(terminal_output).unwrap();
        assert_eq!(terminal_output.matches("Press Enter for frame").count(), 2);
        assert!(terminal_output.contains("Press Enter for frame 2/3"));
        assert!(terminal_output.contains("Press Enter for frame 3/3"));
        assert!(!terminal_output.contains("Press Enter for frame 4/3"));
        assert_eq!(terminal_output.matches(ANSI_CLEAR_SCREEN).count(), 3);
    }
}
