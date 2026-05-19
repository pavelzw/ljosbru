use std::{
    fmt,
    io::{self, BufRead, IsTerminal, Write},
    path::PathBuf,
    str::FromStr,
    thread,
    time::Duration,
};

use anyhow::Context;
use clap::Args;

use self::{
    emit::{EmissionPlan, FrameEmitter},
    png::PngSink,
    terminal::{TerminalScreenSink, TerminalStreamSink},
    transfer::{EncodedFrame, Transfer},
};
use crate::frame::FrameCompression;

mod emit;
mod png;
mod terminal;
mod transfer;

const DEFAULT_ZSTD_LEVEL: u32 = 3;
const MIN_ZSTD_LEVEL: u32 = 1;
const MAX_ZSTD_LEVEL: u32 = 22;

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
        conflicts_with = "terminal",
        help = "Directory for generated PNGs"
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

enum WaitMode<'a> {
    None,
    Input {
        reader: &'a mut dyn BufRead,
        prompt: &'a mut dyn Write,
    },
    Delay(Duration),
}

enum WaitModeSelection {
    None,
    Input,
    Delay(Duration),
}

impl WaitModeSelection {
    fn from_args(args: &EncodeArgs) -> Self {
        Self::from_options(
            args.terminal && io::stdin().is_terminal() && io::stdout().is_terminal(),
            None,
        )
    }

    fn from_options(input_enabled: bool, delay: Option<Duration>) -> Self {
        if let Some(duration) = delay {
            Self::Delay(duration)
        } else if input_enabled {
            Self::Input
        } else {
            Self::None
        }
    }
}

impl<'a> WaitMode<'a> {
    fn input(reader: &'a mut dyn BufRead, prompt: &'a mut dyn Write) -> Self {
        Self::Input { reader, prompt }
    }

    fn wait(&mut self) -> anyhow::Result<()> {
        match self {
            Self::None => Ok(()),
            Self::Input { reader, prompt } => wait_for_enter(&mut **reader, &mut **prompt),
            Self::Delay(duration) => {
                thread::sleep(*duration);
                Ok(())
            }
        }
    }
}

fn wait_for_enter<R, W>(input: &mut R, writer: &mut W) -> anyhow::Result<()>
where
    R: BufRead + ?Sized,
    W: Write + ?Sized,
{
    write!(writer, "Press Enter to continue...").context("failed to write wait prompt")?;
    writer.flush().context("failed to flush wait prompt")?;

    let mut response = String::new();
    input
        .read_line(&mut response)
        .context("failed to read wait prompt response")?;
    writeln!(writer).context("failed to finish wait prompt")
}

trait QrSink {
    fn prepare(&mut self) -> anyhow::Result<()>;
    fn emit_batch(&mut self, frames: Vec<EncodedFrame>) -> anyhow::Result<()>;
    fn finish(&mut self) -> anyhow::Result<()>;
}

fn get_sink(args: &EncodeArgs, transfer: &Transfer) -> Box<dyn QrSink> {
    if args.terminal {
        let stdout = io::stdout();
        if stdout.is_terminal() {
            Box::new(TerminalScreenSink::new(stdout, args.qr_size))
        } else {
            Box::new(TerminalStreamSink::new(stdout, args.qr_size))
        }
    } else {
        Box::new(PngSink::new(
            args.output.clone(),
            args.qr_size,
            transfer.filename_width(),
            args.yes,
        ))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct EncodeSummary {
    original_len: u64,
    encoded_len: u64,
    chunk_count: usize,
    emitted_count: usize,
    compression: Compression,
    output: PathBuf,
    terminal: bool,
}

pub fn encode(args: EncodeArgs) -> anyhow::Result<()> {
    let summary = run_encode(args)?;
    print_summary(&summary);
    Ok(())
}

fn print_summary(summary: &EncodeSummary) {
    let qr_count = if summary.emitted_count == summary.chunk_count {
        summary.chunk_count.to_string()
    } else {
        format!(
            "{} selected of {}",
            summary.emitted_count, summary.chunk_count
        )
    };
    let message = format!(
        "Encoded {} input byte(s) into {} payload byte(s) across {} QR code(s) with {} compression",
        summary.original_len, summary.encoded_len, qr_count, summary.compression,
    );
    if summary.terminal {
        eprintln!("{message} to terminal");
    } else {
        println!("{message} in {}", summary.output.display());
    }
}

fn run_encode(args: EncodeArgs) -> anyhow::Result<EncodeSummary> {
    Transfer::validate_qr_size(args.qr_size)?;

    let compression = args.compression.clone();
    let transfer = Transfer::prepare(&args.filename, args.qr_size, args.compression.clone())?;
    let emitted_count = create_qr_codes_for_args(&args, &transfer)?;

    Ok(EncodeSummary {
        original_len: transfer.original_len(),
        encoded_len: transfer.encoded_len(),
        chunk_count: transfer.chunk_count(),
        emitted_count,
        compression,
        output: args.output,
        terminal: args.terminal,
    })
}

fn create_qr_codes_for_args(args: &EncodeArgs, transfer: &Transfer) -> anyhow::Result<usize> {
    let mut sink = get_sink(args, transfer);

    match WaitModeSelection::from_args(args) {
        WaitModeSelection::None => {
            let mut wait_mode = WaitMode::None;
            create_qr_codes(transfer, sink.as_mut(), &mut wait_mode)
        }
        WaitModeSelection::Input => {
            let stdin = io::stdin();
            let mut stdin = stdin.lock();
            let mut stderr = io::stderr();
            let mut wait_mode = WaitMode::input(&mut stdin, &mut stderr);
            create_qr_codes(transfer, sink.as_mut(), &mut wait_mode)
        }
        WaitModeSelection::Delay(duration) => {
            let mut wait_mode = WaitMode::Delay(duration);
            create_qr_codes(transfer, sink.as_mut(), &mut wait_mode)
        }
    }
}

fn create_qr_codes(
    transfer: &Transfer,
    sink: &mut dyn QrSink,
    wait_mode: &mut WaitMode<'_>,
) -> anyhow::Result<usize> {
    sink.prepare()?;

    let plan = EmissionPlan::single_frames();
    let mut emitter = FrameEmitter::new(transfer, &plan);
    let mut emitted_count = 0;

    while let Some(batch) = emitter.next_batch()? {
        if emitted_count > 0 {
            wait_mode.wait()?;
        }
        emitted_count += batch.len();
        sink.emit_batch(batch)?;
    }

    sink.finish()?;
    Ok(emitted_count)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{self, BufRead, Cursor, Write},
    };

    use clap::Parser;

    use super::*;
    use crate::frame::{HEADER_LEN, MAX_FRAME_BYTES_PER_QR};

    #[derive(Debug, Parser)]
    struct EncodeCli {
        #[command(flatten)]
        args: EncodeArgs,
    }

    fn run_terminal_encode_with_writer<W>(
        args: EncodeArgs,
        terminal_writer: &mut W,
    ) -> anyhow::Result<EncodeSummary>
    where
        W: Write,
    {
        let mut terminal_input = io::empty();
        run_terminal_encode_for_test(args, terminal_writer, &mut terminal_input, false)
            .map(|(summary, _)| summary)
    }

    fn run_terminal_encode_for_test<W, R>(
        args: EncodeArgs,
        terminal_writer: &mut W,
        terminal_input: &mut R,
        wait_for_enter: bool,
    ) -> anyhow::Result<(EncodeSummary, Vec<u8>)>
    where
        W: Write,
        R: BufRead,
    {
        Transfer::validate_qr_size(args.qr_size)?;

        let compression = args.compression.clone();
        let transfer = Transfer::prepare(&args.filename, args.qr_size, args.compression.clone())?;
        let mut prompt_output = Vec::new();
        let mut sink: Box<dyn QrSink + '_> = if wait_for_enter {
            Box::new(TerminalScreenSink::new(
                terminal_writer as &mut dyn Write,
                args.qr_size,
            ))
        } else {
            Box::new(TerminalStreamSink::new(
                terminal_writer as &mut dyn Write,
                args.qr_size,
            ))
        };
        let emitted_count = {
            let mut wait_mode = if wait_for_enter {
                WaitMode::input(
                    terminal_input as &mut dyn BufRead,
                    &mut prompt_output as &mut dyn Write,
                )
            } else {
                WaitMode::None
            };
            create_qr_codes(&transfer, sink.as_mut(), &mut wait_mode)?
        };

        let summary = EncodeSummary {
            original_len: transfer.original_len(),
            encoded_len: transfer.encoded_len(),
            chunk_count: transfer.chunk_count(),
            emitted_count,
            compression,
            output: args.output,
            terminal: args.terminal,
        };

        Ok((summary, prompt_output))
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
    fn encode_cli_rejects_terminal_with_explicit_output() {
        let error = EncodeCli::try_parse_from([
            "encode",
            "input.bin",
            "--qr-size",
            "128",
            "--terminal",
            "--output",
            "out",
        ])
        .unwrap_err();

        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn encode_cli_yes_selects_auto_cleanup_mode() {
        let args = EncodeCli::try_parse_from(["encode", "input.bin", "--qr-size", "128", "--yes"])
            .unwrap();
        let args = args.args;

        assert!(args.yes);
    }

    #[test]
    fn rejects_too_large_qr_size_before_cleanup() {
        let tempdir = tempfile::tempdir().unwrap();
        let input_path = tempdir.path().join("input.bin");
        let output = tempdir.path().join("out");
        fs::create_dir(&output).unwrap();
        fs::write(&input_path, b"hello").unwrap();
        fs::write(output.join("stale.png"), b"old").unwrap();

        let error = run_encode(EncodeArgs {
            filename: input_path,
            qr_size: MAX_FRAME_BYTES_PER_QR + 1,
            compression: Compression::None,
            output: output.clone(),
            terminal: false,
            yes: false,
        })
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

        let error = run_encode(EncodeArgs {
            filename: input_path,
            qr_size: HEADER_LEN,
            compression: Compression::None,
            output: output.clone(),
            terminal: false,
            yes: false,
        })
        .unwrap_err();

        assert!(error.to_string().contains("must be greater than 72"));
        assert!(output.join("stale.png").exists());
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

        let summary = run_encode(EncodeArgs {
            filename: input_path,
            qr_size: HEADER_LEN + 5,
            compression: Compression::None,
            output: output.clone(),
            terminal: false,
            yes: true,
        })
        .unwrap();

        assert_eq!(summary.original_len, 11);
        assert_eq!(summary.encoded_len, 11);
        assert_eq!(summary.chunk_count, 3);
        assert_eq!(summary.emitted_count, 3);
        assert!(!output.join("stale.png").exists());
        assert!(output.join("keep.txt").exists());
        assert!(output.join("000001.png").exists());
        assert!(output.join("000002.png").exists());
        assert!(output.join("000003.png").exists());
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
        let summary = run_terminal_encode_with_writer(
            EncodeArgs {
                filename: input_path,
                qr_size: HEADER_LEN + 5,
                compression: Compression::None,
                output: output.clone(),
                terminal: true,
                yes: false,
            },
            &mut terminal_output,
        )
        .unwrap();

        assert!(summary.terminal);
        assert_eq!(summary.chunk_count, 3);
        assert_eq!(summary.emitted_count, 3);
        assert!(output.join("stale.png").exists());
        assert!(!output.join("000001.png").exists());
        assert!(!terminal_output.is_empty());
        let terminal_output = String::from_utf8(terminal_output).unwrap();
        assert!(terminal_output.contains("Frame 1/3"));
        assert!(terminal_output.contains("Frame 2/3"));
        assert!(terminal_output.contains("Frame 3/3"));
        assert!(terminal_output.contains("\n\nFrame 2/3"));
        assert!(terminal_output.contains("\n\nFrame 3/3"));
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
        let (summary, prompt_output) = run_terminal_encode_for_test(
            EncodeArgs {
                filename: input_path,
                qr_size: HEADER_LEN + 5,
                compression: Compression::None,
                output,
                terminal: true,
                yes: false,
            },
            &mut terminal_output,
            &mut terminal_input,
            true,
        )
        .unwrap();

        assert!(summary.terminal);
        assert_eq!(summary.chunk_count, 3);
        assert_eq!(summary.emitted_count, 3);
        let prompt_output = String::from_utf8(prompt_output).unwrap();
        assert_eq!(prompt_output.matches("Press Enter to continue").count(), 2);
        let terminal_output = String::from_utf8(terminal_output).unwrap();
        assert_eq!(
            terminal_output.matches(terminal::ANSI_CLEAR_SCREEN).count(),
            3
        );
    }

    #[test]
    fn delay_wait_mode_waits_without_frame_context() {
        let mut wait_mode = WaitMode::Delay(Duration::ZERO);

        wait_mode.wait().unwrap();
    }
}
