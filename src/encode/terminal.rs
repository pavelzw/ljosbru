use std::io::{self, Write};

use anyhow::Context;
use qrcode::{Color, QrCode};

use super::{QrSink, emit::byte_mode_qr_code, transfer::EncodedFrame};

const TERMINAL_QUIET_ZONE_MODULES: usize = 4;
const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BLACK_ON_BLACK: &str = "\x1b[30;40m";
const ANSI_BLACK_ON_WHITE: &str = "\x1b[30;107m";
const ANSI_WHITE_ON_BLACK: &str = "\x1b[97;40m";
const ANSI_WHITE_ON_WHITE: &str = "\x1b[97;107m";
pub(super) const ANSI_CLEAR_SCREEN: &str = "\x1b[2J\x1b[H";

pub(super) struct TerminalSink<W> {
    writer: W,
    qr_size: usize,
    clear_screen: bool,
    first_batch: bool,
}

impl<W> TerminalSink<W>
where
    W: Write,
{
    pub(super) fn new(writer: W, qr_size: usize, clear_screen: bool) -> Self {
        Self {
            writer,
            qr_size,
            clear_screen,
            first_batch: true,
        }
    }

    fn write_frames(&mut self, frames: Vec<EncodedFrame>) -> anyhow::Result<()> {
        for (index, frame) in frames.into_iter().enumerate() {
            if index > 0 {
                writeln!(self.writer).context("failed to write terminal QR separator")?;
            }
            self.write_frame(frame)?;
        }
        Ok(())
    }

    pub(super) fn finish(&mut self) -> anyhow::Result<()> {
        self.writer
            .flush()
            .context("failed to flush terminal QR output")
    }

    fn write_frame(&mut self, frame: EncodedFrame) -> anyhow::Result<()> {
        writeln!(
            self.writer,
            "Frame {}/{}",
            frame.sequence, frame.total_chunks
        )
        .context("failed to write terminal QR frame label")?;
        write_qr_terminal(
            &frame.bytes,
            &mut self.writer,
            self.qr_size,
            frame.sequence,
            frame.total_chunks,
        )?;
        Ok(())
    }
}

impl<W> QrSink for TerminalSink<W>
where
    W: Write,
{
    fn prepare(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn emit_batch(&mut self, frames: Vec<EncodedFrame>) -> anyhow::Result<()> {
        if self.clear_screen {
            clear_screen(&mut self.writer)?;
        } else if !self.first_batch {
            separate_frames(&mut self.writer)?;
        }
        self.first_batch = false;
        self.write_frames(frames)?;
        self.finish()
    }

    fn finish(&mut self) -> anyhow::Result<()> {
        TerminalSink::finish(self)
    }
}

fn clear_screen(writer: &mut dyn Write) -> anyhow::Result<()> {
    write!(writer, "{ANSI_CLEAR_SCREEN}").context("failed to clear terminal")
}

fn separate_frames(writer: &mut dyn Write) -> anyhow::Result<()> {
    writeln!(writer).context("failed to write terminal QR separator")
}

fn write_qr_terminal<W>(
    frame: &[u8],
    writer: &mut W,
    qr_size: usize,
    sequence: u32,
    total_chunks: u32,
) -> anyhow::Result<()>
where
    W: Write + ?Sized,
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
    W: Write + ?Sized,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::emit::byte_mode_qr_code;

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
}
