use std::{
    fmt,
    io::{self, BufRead, IsTerminal, Write},
    path::PathBuf,
    str::FromStr,
    thread,
    time::{Duration, Instant},
};

use anyhow::Context;
use clap::Args;
use indicatif::{ProgressBar, ProgressStyle};

use self::{
    emit::{EmissionPlan, FrameEmitter},
    png::PngSink,
    terminal::TerminalSink,
    transfer::{EncodedFrame, Transfer},
};
use crate::frame::FrameCompression;
use crate::progress::human_bytes_per_second;

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

    #[arg(
        long,
        value_name = "auto|none|enter|delay:<milliseconds>",
        default_value = "auto"
    )]
    wait: WaitArg,
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum WaitArg {
    Auto,
    None,
    Enter,
    Delay(Duration),
}

impl FromStr for WaitArg {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => return Ok(Self::Auto),
            "none" => return Ok(Self::None),
            "enter" => return Ok(Self::Enter),
            _ => {}
        }

        let Some(milliseconds) = value.strip_prefix("delay:") else {
            return Err("expected `auto`, `none`, `enter`, or `delay:<milliseconds>`".to_owned());
        };

        let milliseconds = milliseconds
            .parse()
            .map_err(|_| "expected delay milliseconds to be an integer".to_owned())?;

        Ok(Self::Delay(Duration::from_millis(milliseconds)))
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

#[derive(Debug)]
enum WaitModeSelection {
    None,
    Input,
    Delay(Duration),
}

impl WaitModeSelection {
    fn from_args(args: &EncodeArgs) -> anyhow::Result<Self> {
        let stdin_is_terminal = io::stdin().is_terminal();
        let stdout_is_terminal = io::stdout().is_terminal();
        Self::from_wait_arg(
            &args.wait,
            args.terminal && stdin_is_terminal && stdout_is_terminal,
            stdin_is_terminal,
        )
    }

    fn from_wait_arg(
        wait: &WaitArg,
        auto_input_enabled: bool,
        input_available: bool,
    ) -> anyhow::Result<Self> {
        match wait {
            WaitArg::Auto if auto_input_enabled => Ok(Self::Input),
            WaitArg::Auto | WaitArg::None => Ok(Self::None),
            WaitArg::Enter if input_available => Ok(Self::Input),
            WaitArg::Enter => anyhow::bail!("--wait enter requires interactive stdin"),
            WaitArg::Delay(duration) => Ok(Self::Delay(*duration)),
        }
    }
}

impl<'a> WaitMode<'a> {
    fn input(reader: &'a mut dyn BufRead, prompt: &'a mut dyn Write) -> Self {
        Self::Input { reader, prompt }
    }

    fn writes_prompt(&self) -> bool {
        matches!(self, Self::Input { .. })
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProgressDisplay {
    Bar,
    Line,
    None,
}

struct EncodeProgress {
    progress: Option<ProgressBar>,
    started_at: Instant,
    display: ProgressDisplay,
    total_chunks: usize,
    emitted_chunks: usize,
    payload_bytes: u64,
}

impl EncodeProgress {
    fn new(total_chunks: usize, display: ProgressDisplay) -> anyhow::Result<Self> {
        let progress = if display == ProgressDisplay::Bar {
            let progress = ProgressBar::new(total_chunks as u64);
            progress.set_style(
                ProgressStyle::with_template(
                    "Encoding QR codes [{bar:40.cyan/blue}] {pos}/{len} {msg} ({elapsed_precise})",
                )
                .context("failed to configure encode progress bar")?
                .progress_chars("##-"),
            );
            progress.set_message(human_bytes_per_second(0.0));
            Some(progress)
        } else {
            None
        };

        Ok(Self {
            progress,
            started_at: Instant::now(),
            display,
            total_chunks,
            emitted_chunks: 0,
            payload_bytes: 0,
        })
    }

    fn suspend<R>(&self, action: impl FnOnce() -> R) -> R {
        if let Some(progress) = &self.progress {
            progress.suspend(action)
        } else {
            action()
        }
    }

    fn inc(&mut self, frames: usize, payload_bytes: usize) {
        self.emitted_chunks += frames;
        self.payload_bytes += payload_bytes as u64;
        let bytes_per_second =
            self.payload_bytes as f64 / self.started_at.elapsed().as_secs_f64().max(f64::EPSILON);
        let speed = human_bytes_per_second(bytes_per_second);

        if let Some(progress) = &self.progress {
            progress.inc(frames as u64);
            progress.set_message(speed);
        } else if self.display == ProgressDisplay::Line {
            eprintln!(
                "{}",
                encode_progress_line(self.emitted_chunks, self.total_chunks, &speed)
            );
        }
    }

    fn finish(self) {
        if let Some(progress) = self.progress {
            progress.finish_and_clear();
        }
    }
}

fn encode_progress_line(emitted_chunks: usize, total_chunks: usize, speed: &str) -> String {
    const BAR_WIDTH: usize = 40;

    let total_chunks = total_chunks.max(1);
    let filled = (emitted_chunks * BAR_WIDTH / total_chunks).min(BAR_WIDTH);
    format!(
        "Encoding QR codes [{}{}] {}/{} {}",
        "#".repeat(filled),
        "-".repeat(BAR_WIDTH - filled),
        emitted_chunks,
        total_chunks,
        speed
    )
}

fn get_sink(args: &EncodeArgs, transfer: &Transfer) -> Box<dyn QrSink> {
    if args.terminal {
        let stdout = io::stdout();
        let clear_screen = stdout.is_terminal();
        Box::new(TerminalSink::new(stdout, args.qr_size, clear_screen))
    } else {
        Box::new(PngSink::new(
            args.output.clone(),
            args.qr_size,
            transfer.filename_width(),
            transfer.chunk_count(),
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
    let progress_display = if args.terminal {
        ProgressDisplay::Line
    } else {
        ProgressDisplay::None
    };

    match WaitModeSelection::from_args(args)? {
        WaitModeSelection::None => {
            let mut wait_mode = WaitMode::None;
            create_qr_codes(transfer, sink.as_mut(), &mut wait_mode, progress_display)
        }
        WaitModeSelection::Input => {
            let stdin = io::stdin();
            let mut stdin = stdin.lock();
            let mut stderr = io::stderr();
            let mut wait_mode = WaitMode::input(&mut stdin, &mut stderr);
            create_qr_codes(transfer, sink.as_mut(), &mut wait_mode, progress_display)
        }
        WaitModeSelection::Delay(duration) => {
            let mut wait_mode = WaitMode::Delay(duration);
            create_qr_codes(transfer, sink.as_mut(), &mut wait_mode, progress_display)
        }
    }
}

fn create_qr_codes(
    transfer: &Transfer,
    sink: &mut dyn QrSink,
    wait_mode: &mut WaitMode<'_>,
    progress_display: ProgressDisplay,
) -> anyhow::Result<usize> {
    sink.prepare()?;

    let plan = EmissionPlan::single_frames();
    let mut emitter = FrameEmitter::new(transfer, &plan);
    let mut progress = EncodeProgress::new(transfer.chunk_count(), progress_display)?;
    let mut emitted_count = 0;

    while let Some(batch) = emitter.next_batch()? {
        if emitted_count > 0 {
            if wait_mode.writes_prompt() {
                progress.suspend(|| wait_mode.wait())?;
            } else {
                wait_mode.wait()?;
            }
        }
        let batch_len = batch.len();
        let payload_len = batch.iter().map(|frame| frame.payload_len).sum();
        sink.emit_batch(batch)?;
        emitted_count += batch_len;
        progress.inc(batch_len, payload_len);
    }

    progress.finish();
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
        let mut sink: Box<dyn QrSink + '_> = Box::new(TerminalSink::new(
            terminal_writer as &mut dyn Write,
            args.qr_size,
            wait_for_enter,
        ));
        let emitted_count = {
            let mut wait_mode = if wait_for_enter {
                WaitMode::input(
                    terminal_input as &mut dyn BufRead,
                    &mut prompt_output as &mut dyn Write,
                )
            } else {
                WaitMode::None
            };
            create_qr_codes(
                &transfer,
                sink.as_mut(),
                &mut wait_mode,
                ProgressDisplay::Line,
            )?
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
        assert_eq!(args.wait, WaitArg::Auto);
        assert!(!args.terminal);
        assert!(!args.yes);
    }

    #[test]
    fn encode_cli_parses_wait_modes() {
        let args = EncodeCli::try_parse_from([
            "encode",
            "input.bin",
            "--qr-size",
            "128",
            "--wait",
            "none",
        ])
        .unwrap()
        .args;
        assert_eq!(args.wait, WaitArg::None);

        let args = EncodeCli::try_parse_from([
            "encode",
            "input.bin",
            "--qr-size",
            "128",
            "--wait",
            "enter",
        ])
        .unwrap()
        .args;
        assert_eq!(args.wait, WaitArg::Enter);

        let args = EncodeCli::try_parse_from([
            "encode",
            "input.bin",
            "--qr-size",
            "128",
            "--wait",
            "delay:500",
        ])
        .unwrap()
        .args;
        assert_eq!(args.wait, WaitArg::Delay(Duration::from_millis(500)));
    }

    #[test]
    fn encode_cli_rejects_invalid_wait_modes() {
        for wait in ["", "delay", "delay:", "delay:abc", "key:Space"] {
            let error = EncodeCli::try_parse_from([
                "encode",
                "input.bin",
                "--qr-size",
                "128",
                "--wait",
                wait,
            ])
            .unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
        }
    }

    #[test]
    fn wait_selection_maps_cli_wait_modes() {
        assert!(matches!(
            WaitModeSelection::from_wait_arg(&WaitArg::Auto, true, true).unwrap(),
            WaitModeSelection::Input
        ));
        assert!(matches!(
            WaitModeSelection::from_wait_arg(&WaitArg::Auto, false, true).unwrap(),
            WaitModeSelection::None
        ));
        assert!(matches!(
            WaitModeSelection::from_wait_arg(&WaitArg::None, true, true).unwrap(),
            WaitModeSelection::None
        ));
        assert!(matches!(
            WaitModeSelection::from_wait_arg(&WaitArg::Enter, false, true).unwrap(),
            WaitModeSelection::Input
        ));
        assert!(
            WaitModeSelection::from_wait_arg(&WaitArg::Enter, false, false)
                .unwrap_err()
                .to_string()
                .contains("interactive stdin")
        );
        assert!(matches!(
            WaitModeSelection::from_wait_arg(
                &WaitArg::Delay(Duration::from_millis(7)),
                false,
                false
            )
            .unwrap(),
            WaitModeSelection::Delay(duration) if duration == Duration::from_millis(7)
        ));
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
            wait: WaitArg::Auto,
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
            wait: WaitArg::Auto,
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
            wait: WaitArg::Auto,
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
                wait: WaitArg::Auto,
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
                wait: WaitArg::Auto,
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

    #[test]
    fn terminal_progress_line_shows_position_and_speed() {
        let line = encode_progress_line(2, 4, "1 KiB/s");

        assert_eq!(
            line,
            "Encoding QR codes [####################--------------------] 2/4 1 KiB/s"
        );
    }
}
