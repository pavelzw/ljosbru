use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use clap::Args;
use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use image::{DynamicImage, RgbaImage};
use indicatif::{ProgressBar, ProgressStyle};
use log::debug;
use rxing::{
    BarcodeFormat, BinaryBitmap, DecodeHintValue, DecodeHints, Luma8LuminanceSource,
    MultiFormatReader, RXingResult, RXingResultMetadataType, RXingResultMetadataValue,
    common::HybridBinarizer,
    multi::{GenericMultipleBarcodeReader, MultipleBarcodeReader},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use xcap::Monitor;

use crate::frame::{Frame, FrameCompression, HEADER_LEN, MAGIC, build_frame, parse_frame};
use crate::progress::{human_bytes_per_second, human_eta};

const DEFAULT_RETRY_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_INITIAL_DELAY_MS: u64 = 1_000;
const CACHE_EXTENSION: &str = "ljosbru-frame";

#[derive(Debug, Args)]
pub struct DecodeArgs {
    #[arg(long, value_name = "milliseconds")]
    delay_between: u64,

    #[arg(long, value_name = "key", value_parser = parse_forward_key)]
    forward_keypress: Key,

    #[arg(long, value_name = "directory")]
    cache_dir: PathBuf,

    #[arg(long, value_name = "file")]
    output: PathBuf,

    #[arg(long, value_name = "index")]
    monitor: Option<usize>,

    #[arg(
        long,
        value_name = "milliseconds",
        default_value_t = DEFAULT_INITIAL_DELAY_MS
    )]
    initial_delay: u64,

    #[arg(
        long,
        value_name = "milliseconds",
        default_value_t = DEFAULT_RETRY_TIMEOUT_MS
    )]
    retry_timeout: u64,

    #[arg(
        long,
        help = "Save screenshots to <cache-dir>/<timestamp>-screenshot.png"
    )]
    save_screenshots: bool,
}

#[derive(Debug, Args)]
pub struct PrintMissingArgs {
    #[arg(long, value_name = "directory")]
    cache_dir: PathBuf,
}

/// Parses a forward key name, such as `Space`, `Tab`, or `RightArrow`.
pub fn parse_forward_key(value: &str) -> anyhow::Result<Key> {
    let deserializer = serde::de::value::StrDeserializer::<serde::de::value::Error>::new(value);
    Key::deserialize(deserializer)
        .context("expected a key name, such as `Space`, `Tab`, or `RightArrow`")
}

pub fn print_missing(args: PrintMissingArgs) -> anyhow::Result<()> {
    let store = FrameStore::load(&args.cache_dir)?;
    for range in store.missing_ranges()? {
        println!("{range}");
    }
    Ok(())
}

/// Decode settings shared by the CLI and the GUI.
#[derive(Debug, Clone)]
pub struct DecodeConfig {
    pub delay_between: Duration,
    pub forward_keypress: Key,
    pub cache_dir: PathBuf,
    pub output: PathBuf,
    pub monitor: Option<usize>,
    pub initial_delay: Duration,
    pub retry_timeout: Duration,
    pub save_screenshots: bool,
}

impl From<DecodeArgs> for DecodeConfig {
    fn from(args: DecodeArgs) -> Self {
        Self {
            delay_between: Duration::from_millis(args.delay_between),
            forward_keypress: args.forward_keypress,
            cache_dir: args.cache_dir,
            output: args.output,
            monitor: args.monitor,
            initial_delay: Duration::from_millis(args.initial_delay),
            retry_timeout: Duration::from_millis(args.retry_timeout),
            save_screenshots: args.save_screenshots,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DecodeProgress {
    pub cached_frames: usize,
    /// Unknown until the first frame has been decoded.
    pub total_frames: Option<u32>,
    pub bytes_per_second: f64,
    pub remaining_bytes: u64,
}

impl DecodeProgress {
    fn new(store: &FrameStore, bytes_per_second: f64) -> Self {
        let remaining_bytes = store
            .metadata
            .as_ref()
            .map(|metadata| {
                metadata
                    .encoded_len
                    .saturating_sub(store.cached_payload_bytes())
            })
            .unwrap_or_default();
        Self {
            cached_frames: store.frame_count(),
            total_frames: store
                .metadata
                .as_ref()
                .map(|metadata| metadata.total_chunks),
            bytes_per_second,
            remaining_bytes,
        }
    }

    pub fn rate(&self) -> String {
        human_bytes_per_second(self.bytes_per_second)
    }

    pub fn eta(&self) -> String {
        human_eta(self.remaining_bytes, self.bytes_per_second)
    }
}

#[derive(Debug, Clone)]
pub struct DecodeSummary {
    pub output: PathBuf,
    pub original_len: u64,
    pub total_chunks: u32,
    pub sha256: String,
}

#[derive(Debug)]
pub enum DecodeEvent<'a> {
    /// The cache directory was loaded; emitted once before anything else.
    CacheLoaded(DecodeProgress),
    /// Time left of the initial delay, emitted roughly every 100 ms.
    Countdown(Duration),
    MonitorSelected {
        index: usize,
        description: &'a str,
    },
    Screenshot(&'a RgbaImage),
    Scanned {
        attempt: u64,
        summary: String,
    },
    FrameCached {
        sequence: u32,
        progress: DecodeProgress,
    },
}

#[derive(Debug)]
pub enum DecodeOutcome {
    Completed(DecodeSummary),
    Cancelled,
}

const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub fn decode(args: DecodeArgs) -> anyhow::Result<()> {
    let config = DecodeConfig::from(args);
    let mut progress_bar = None;
    let outcome = run_decode(&config, &AtomicBool::new(false), |event| match event {
        DecodeEvent::CacheLoaded(progress) | DecodeEvent::FrameCached { progress, .. } => {
            update_decode_progress_bar(&mut progress_bar, &progress);
        }
        _ => {}
    });
    if let Some(progress_bar) = progress_bar.take() {
        progress_bar.finish_and_clear();
    }
    if let DecodeOutcome::Completed(summary) = outcome? {
        println!(
            "Decoded {} byte(s) from {} QR code(s) into {}",
            summary.original_len,
            summary.total_chunks,
            summary.output.display()
        );
        println!("SHA-256: {}", summary.sha256);
    }
    Ok(())
}

/// Runs the screenshot/keypress loop until every frame is cached or `cancel` is set.
pub fn run_decode(
    config: &DecodeConfig,
    cancel: &AtomicBool,
    mut on_event: impl FnMut(DecodeEvent<'_>),
) -> anyhow::Result<DecodeOutcome> {
    let mut store = FrameStore::load(&config.cache_dir)?;
    debug!(
        "Loaded {} cached QR frame(s){} from {}",
        store.frame_count(),
        store
            .metadata
            .as_ref()
            .map(|metadata| format!(" of {}", metadata.total_chunks))
            .unwrap_or_default(),
        config.cache_dir.display()
    );
    on_event(DecodeEvent::CacheLoaded(DecodeProgress::new(&store, 0.0)));

    if store.is_complete() {
        debug!(
            "Cache is complete; writing decoded output to {}",
            config.output.display()
        );
        return finish_decode(&store, &config.output);
    }

    debug!(
        "Waiting {} ms before first screenshot",
        config.initial_delay.as_millis()
    );
    let countdown_started_at = Instant::now();
    while let Some(remaining) = config
        .initial_delay
        .checked_sub(countdown_started_at.elapsed())
        .filter(|remaining| !remaining.is_zero())
    {
        if cancel.load(Ordering::Relaxed) {
            return Ok(DecodeOutcome::Cancelled);
        }
        on_event(DecodeEvent::Countdown(remaining));
        thread::sleep(remaining.min(CANCEL_POLL_INTERVAL));
    }
    let started_at = Instant::now();
    let initial_payload_bytes = store.cached_payload_bytes();

    let selected_monitor = select_monitor(config.monitor)?;
    debug!(
        "Using monitor {} ({})",
        selected_monitor.index, selected_monitor.description
    );
    on_event(DecodeEvent::MonitorSelected {
        index: selected_monitor.index,
        description: &selected_monitor.description,
    });
    let mut enigo =
        Enigo::new(&Settings::default()).context("failed to initialize keyboard input")?;
    let screenshot_options = ScreenshotOptions {
        cache_dir: &config.cache_dir,
        save_screenshots: config.save_screenshots,
    };
    let mut scanner = RxingScanner::new();
    let mut capture = CaptureContext {
        monitor: &selected_monitor.monitor,
        scanner: &mut scanner,
        screenshot_options,
        on_event: &mut on_event,
    };
    let mut previous_sequence = None;

    loop {
        let Some(frame) = wait_for_frame(
            &mut capture,
            cancel,
            config.retry_timeout,
            config.delay_between,
            previous_sequence,
            &store,
        )?
        else {
            return Ok(DecodeOutcome::Cancelled);
        };
        let sequence = frame.sequence;
        let total_chunks = frame.total_chunks;
        previous_sequence = Some(frame.sequence);
        let status = store.cache_frame(&config.cache_dir, frame)?;
        let decoded_bytes = store
            .cached_payload_bytes()
            .saturating_sub(initial_payload_bytes);
        let bytes_per_second =
            decoded_bytes as f64 / started_at.elapsed().as_secs_f64().max(f64::EPSILON);
        (capture.on_event)(DecodeEvent::FrameCached {
            sequence,
            progress: DecodeProgress::new(&store, bytes_per_second),
        });
        debug!(
            "{} QR frame {sequence}/{total_chunks}; cache now has {} frame(s)",
            status.log_label(),
            store.frame_count()
        );

        if store.is_complete() {
            debug!(
                "All QR frames are cached; writing decoded output to {}",
                config.output.display()
            );
            return finish_decode(&store, &config.output);
        }

        debug!(
            "Pressing {:?}, then waiting {} ms",
            config.forward_keypress,
            config.delay_between.as_millis()
        );
        enigo
            .key(config.forward_keypress, Direction::Click)
            .context("failed to press the forward key")?;
        if !sleep_unless_cancelled(config.delay_between, cancel) {
            return Ok(DecodeOutcome::Cancelled);
        }
    }
}

/// Returns `false` if `cancel` was set while sleeping.
fn sleep_unless_cancelled(duration: Duration, cancel: &AtomicBool) -> bool {
    let deadline = Instant::now() + duration;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return true;
        }
        thread::sleep(remaining.min(CANCEL_POLL_INTERVAL));
    }
}

fn finish_decode(store: &FrameStore, output: &Path) -> anyhow::Result<DecodeOutcome> {
    let sha256 = write_reassembled_output(store, output)?;
    let metadata = store
        .metadata
        .as_ref()
        .context("no QR frames have been decoded")?;
    Ok(DecodeOutcome::Completed(DecodeSummary {
        output: output.to_owned(),
        original_len: metadata.original_len,
        total_chunks: metadata.total_chunks,
        sha256,
    }))
}

fn update_decode_progress_bar(progress_bar: &mut Option<ProgressBar>, progress: &DecodeProgress) {
    let Some(total_frames) = progress.total_frames else {
        return;
    };
    let progress_bar = progress_bar.get_or_insert_with(|| {
        let progress_bar = ProgressBar::new(u64::from(total_frames));
        progress_bar.set_style(
            ProgressStyle::with_template(
                "Decoding QR codes [{bar:40.cyan/blue}] {pos}/{len} {msg} ({elapsed_precise})",
            )
            .expect("progress bar template is valid")
            .progress_chars("##-"),
        );
        progress_bar
    });
    progress_bar.set_position(progress.cached_frames as u64);
    progress_bar.set_message(format!("{} ETA {}", progress.rate(), progress.eta()));
}

/// Cached frame counts of a cache directory, for showing resume state.
#[derive(Debug, Clone, Default)]
pub struct CacheStatus {
    pub cached_frames: usize,
    pub total_frames: Option<u32>,
    /// Missing frame IDs grouped into ranges, as printed by `print-missing`.
    pub missing_ranges: Vec<String>,
}

pub fn cache_status(cache_dir: &Path) -> anyhow::Result<CacheStatus> {
    if !cache_dir.is_dir() {
        return Ok(CacheStatus::default());
    }
    let store = FrameStore::load(cache_dir)?;
    Ok(CacheStatus {
        cached_frames: store.frame_count(),
        total_frames: store
            .metadata
            .as_ref()
            .map(|metadata| metadata.total_chunks),
        missing_ranges: if store.metadata.is_some() {
            store.missing_ranges()?
        } else {
            Vec::new()
        },
    })
}

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub index: usize,
    pub description: String,
    pub is_primary: bool,
}

pub fn list_monitors() -> anyhow::Result<Vec<MonitorInfo>> {
    Ok(Monitor::all()
        .context("failed to enumerate monitors")?
        .iter()
        .enumerate()
        .map(|(index, monitor)| MonitorInfo {
            index,
            description: monitor_description(monitor),
            is_primary: monitor.is_primary().unwrap_or(false),
        })
        .collect())
}

struct SelectedMonitor {
    index: usize,
    description: String,
    monitor: Monitor,
}

fn select_monitor(index: Option<usize>) -> anyhow::Result<SelectedMonitor> {
    let monitors = Monitor::all().context("failed to enumerate monitors")?;
    if monitors.is_empty() {
        bail!("no monitors found");
    }

    if let Some(index) = index {
        let monitor_count = monitors.len();
        let monitor = monitors.into_iter().nth(index).with_context(|| {
            format!("monitor index {index} is out of range; found {monitor_count} monitor(s)")
        })?;
        let description = monitor_description(&monitor);
        return Ok(SelectedMonitor {
            index,
            description,
            monitor,
        });
    }

    let primary_index = monitors
        .iter()
        .position(|monitor| monitor.is_primary().unwrap_or(false))
        .unwrap_or(0);

    let monitor = monitors
        .into_iter()
        .nth(primary_index)
        .expect("monitor index was derived from enumerated monitors");
    let description = monitor_description(&monitor);
    Ok(SelectedMonitor {
        index: primary_index,
        description,
        monitor,
    })
}

fn monitor_description(monitor: &Monitor) -> String {
    let name = monitor
        .friendly_name()
        .unwrap_or_else(|_| "unknown name".to_owned());
    let width = monitor
        .width()
        .map(|width| width.to_string())
        .unwrap_or_else(|_| "?".to_owned());
    let height = monitor
        .height()
        .map(|height| height.to_string())
        .unwrap_or_else(|_| "?".to_owned());
    let primary = if monitor.is_primary().unwrap_or(false) {
        ", primary"
    } else {
        ""
    };
    format!("{name}, {width}x{height}{primary}")
}

/// Returns `None` if `cancel` was set while waiting.
fn wait_for_frame(
    capture: &mut CaptureContext<'_>,
    cancel: &AtomicBool,
    retry_timeout: Duration,
    retry_interval: Duration,
    previous_sequence: Option<u32>,
    store: &FrameStore,
) -> anyhow::Result<Option<Frame>> {
    let started_at = Instant::now();
    let retry_interval = if retry_interval.is_zero() {
        Duration::from_millis(100)
    } else {
        retry_interval
    };
    let mut attempts = 0_u64;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        attempts += 1;
        let frames = capture.capture_ljosbru_frames()?;
        let mut usable_frames = Vec::new();
        let mut ignored_previous_sequences = Vec::new();
        let mut observed_sequences = Vec::new();
        for frame in frames {
            observed_sequences.push(frame.sequence);
            if Some(frame.sequence) == previous_sequence {
                ignored_previous_sequences.push(frame.sequence);
                continue;
            }
            store.validate_metadata(&frame)?;
            usable_frames.push(frame);
        }
        let observation = ScreenshotObservation {
            observed_sequences,
            ignored_previous_sequences,
            usable_count: usable_frames.len(),
        };
        debug!("screenshot attempt {attempts}: {observation}");
        (capture.on_event)(DecodeEvent::Scanned {
            attempt: attempts,
            summary: observation.to_string(),
        });

        usable_frames.sort_by_key(|frame| frame.sequence);
        usable_frames.dedup();
        match usable_frames.len() {
            0 => {}
            1 => return Ok(Some(usable_frames.remove(0))),
            _ => bail!("found multiple ljosbru QR codes on the selected monitor"),
        }

        if started_at.elapsed() >= retry_timeout {
            if let Some(sequence) = previous_sequence {
                bail!(
                    "did not find a new ljosbru QR code within {} ms after sequence {sequence}; last screenshot: {}",
                    retry_timeout.as_millis(),
                    observation
                );
            }

            bail!(
                "did not find a ljosbru QR code within {} ms; last screenshot: {}",
                retry_timeout.as_millis(),
                observation
            );
        }

        if !sleep_unless_cancelled(retry_interval, cancel) {
            return Ok(None);
        }
    }
}

#[derive(Default)]
struct ScreenshotObservation {
    observed_sequences: Vec<u32>,
    ignored_previous_sequences: Vec<u32>,
    usable_count: usize,
}

impl std::fmt::Display for ScreenshotObservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.observed_sequences.is_empty() {
            return formatter.write_str("no ljosbru QR frames found");
        }

        write!(
            formatter,
            "observed sequence(s) {}, ignored previous sequence(s) {}, usable candidate(s) {}",
            format_sequences(&self.observed_sequences),
            format_sequences(&self.ignored_previous_sequences),
            self.usable_count
        )
    }
}

fn format_sequences(sequences: &[u32]) -> String {
    if sequences.is_empty() {
        return "none".to_owned();
    }

    sequences
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

struct CaptureContext<'a> {
    monitor: &'a Monitor,
    scanner: &'a mut RxingScanner,
    screenshot_options: ScreenshotOptions<'a>,
    on_event: &'a mut dyn FnMut(DecodeEvent<'_>),
}

impl CaptureContext<'_> {
    fn capture_ljosbru_frames(&mut self) -> anyhow::Result<Vec<Frame>> {
        let screenshot = self
            .monitor
            .capture_image()
            .context("failed to capture monitor screenshot")?;
        if self.screenshot_options.save_screenshots {
            let path = screenshot_path(self.screenshot_options.cache_dir)?;
            screenshot
                .save(&path)
                .with_context(|| format!("failed to save screenshot {}", path.display()))?;
            debug!("saved screenshot {}", path.display());
        }
        (self.on_event)(DecodeEvent::Screenshot(&screenshot));

        self.scanner.decode_ljosbru_frames(screenshot)
    }
}

#[derive(Clone, Copy)]
struct ScreenshotOptions<'a> {
    cache_dir: &'a Path,
    save_screenshots: bool,
}

fn screenshot_path(cache_dir: &Path) -> anyhow::Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_millis();
    Ok(cache_dir.join(screenshot_filename(timestamp)))
}

fn screenshot_filename(timestamp: u128) -> String {
    format!("{timestamp}-screenshot.png")
}

struct RxingScanner {
    reader: GenericMultipleBarcodeReader<MultiFormatReader>,
    hints: DecodeHints,
}

impl RxingScanner {
    fn new() -> Self {
        let hints = DecodeHints::default()
            .with(DecodeHintValue::PossibleFormats(
                [BarcodeFormat::QR_CODE].into(),
            ))
            .with(DecodeHintValue::TryHarder(true));
        Self {
            reader: GenericMultipleBarcodeReader::new(MultiFormatReader::default()),
            hints,
        }
    }

    fn decode_ljosbru_frames(&mut self, screenshot: RgbaImage) -> anyhow::Result<Vec<Frame>> {
        let width = screenshot.width();
        let height = screenshot.height();
        let image = DynamicImage::ImageRgba8(screenshot).to_luma8();
        let source = Luma8LuminanceSource::new(image.into_raw(), width, height)
            .context("failed to create luminance source from screenshot")?;
        let mut bitmap = BinaryBitmap::new(HybridBinarizer::new(source));
        let results = match self
            .reader
            .decode_multiple_with_hints(&mut bitmap, &self.hints)
        {
            Ok(results) => results,
            Err(error) => {
                debug!("rxing found no ljosbru QR frames: {error}");
                return Ok(Vec::new());
            }
        };

        let frames = results
            .into_iter()
            .filter_map(|result| {
                if result.getBarcodeFormat() == &BarcodeFormat::QR_CODE {
                    Some(parse_rxing_result(&result))
                } else {
                    debug!(
                        "rxing decoded non-QR barcode format {}",
                        result.getBarcodeFormat()
                    );
                    None
                }
            })
            .flatten()
            .collect::<Vec<_>>();
        if frames.is_empty() {
            debug!("rxing found no ljosbru QR frames");
        } else {
            debug!(
                "rxing decoded ljosbru sequence(s) {}",
                format_sequences(
                    &frames
                        .iter()
                        .map(|frame| frame.sequence)
                        .collect::<Vec<_>>()
                )
            );
        }
        Ok(frames)
    }
}

fn parse_rxing_result(result: &RXingResult) -> Vec<Frame> {
    let mut frames = parse_rxing_byte_segments(result);
    if frames.is_empty() {
        frames = parse_frames_from_bytes(result.getRawBytes());
    }
    if frames.is_empty() {
        frames = parse_frames_from_text(result.getText());
    }
    frames
}

fn parse_rxing_byte_segments(result: &RXingResult) -> Vec<Frame> {
    let Some(RXingResultMetadataValue::ByteSegments(segments)) = result
        .getRXingResultMetadata()
        .get(&RXingResultMetadataType::BYTE_SEGMENTS)
    else {
        return Vec::new();
    };

    parse_frames_from_bytes(&segments.concat())
}

fn parse_frames_from_text(text: &str) -> Vec<Frame> {
    parse_text_as_bytes(text)
        .map(|decoded| parse_frames_from_bytes(&decoded))
        .unwrap_or_default()
}

fn parse_text_as_bytes(text: &str) -> Option<Vec<u8>> {
    let mut decoded = Vec::with_capacity(text.len());
    for character in text.chars() {
        let value = u32::from(character);
        let byte = u8::try_from(value).ok()?;
        decoded.push(byte);
    }
    Some(decoded)
}

fn parse_frames_from_bytes(output: &[u8]) -> Vec<Frame> {
    let mut frames = Vec::new();
    for index in 0..output.len() {
        if !output[index..].starts_with(MAGIC) || output.len() < index + HEADER_LEN {
            continue;
        }

        let chunk_len = u32::from_be_bytes([
            output[index + 68],
            output[index + 69],
            output[index + 70],
            output[index + 71],
        ]) as usize;
        let frame_len = HEADER_LEN + chunk_len;
        let Some(end) = index.checked_add(frame_len) else {
            continue;
        };
        if end > output.len() {
            continue;
        }

        if let Ok(frame) = parse_frame(&output[index..end]) {
            frames.push(frame);
        }
    }
    frames
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamMetadata {
    total_chunks: u32,
    original_len: u64,
    encoded_len: u64,
    stream_hash: blake3::Hash,
    compression: FrameCompression,
}

impl StreamMetadata {
    fn from_frame(frame: &Frame) -> Self {
        Self {
            total_chunks: frame.total_chunks,
            original_len: frame.original_len,
            encoded_len: frame.encoded_len,
            stream_hash: frame.stream_hash,
            compression: frame.compression,
        }
    }

    fn validate_frame(&self, frame: &Frame) -> anyhow::Result<()> {
        let frame_metadata = Self::from_frame(frame);
        if &frame_metadata != self {
            bail!("QR frame metadata does not match the current transfer");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InsertStatus {
    Inserted,
    Existing,
}

impl InsertStatus {
    fn log_label(&self) -> &'static str {
        match self {
            Self::Inserted => "Cached",
            Self::Existing => "Reused cached",
        }
    }
}

#[derive(Debug, Default)]
struct FrameStore {
    metadata: Option<StreamMetadata>,
    frames: BTreeMap<u32, Frame>,
}

impl FrameStore {
    fn load(cache_dir: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(cache_dir)
            .with_context(|| format!("failed to create cache directory {}", cache_dir.display()))?;

        let mut store = Self::default();
        for path in cache_frame_files(cache_dir)? {
            let bytes = fs::read(&path)
                .with_context(|| format!("failed to read cache frame {}", path.display()))?;
            let frame = parse_frame(&bytes)
                .with_context(|| format!("failed to parse cache frame {}", path.display()))?;
            store.insert(frame)?;
        }
        Ok(store)
    }

    fn cache_frame(&mut self, cache_dir: &Path, frame: Frame) -> anyhow::Result<InsertStatus> {
        let status = self.insert(frame.clone())?;
        let bytes = build_frame(frame.clone())?;
        let path = cache_dir.join(cache_frame_filename(frame.sequence, frame.total_chunks));
        fs::write(&path, bytes)
            .with_context(|| format!("failed to write cache frame {}", path.display()))?;
        Ok(status)
    }

    fn insert(&mut self, frame: Frame) -> anyhow::Result<InsertStatus> {
        self.validate_metadata(&frame)?;
        self.metadata
            .get_or_insert_with(|| StreamMetadata::from_frame(&frame));

        if let Some(existing) = self.frames.get(&frame.sequence) {
            if existing != &frame {
                bail!("conflicting QR frame for sequence {}", frame.sequence);
            }
            return Ok(InsertStatus::Existing);
        }

        self.frames.insert(frame.sequence, frame);
        Ok(InsertStatus::Inserted)
    }

    fn validate_metadata(&self, frame: &Frame) -> anyhow::Result<()> {
        if let Some(metadata) = &self.metadata {
            metadata.validate_frame(frame)?;
        }
        Ok(())
    }

    fn is_complete(&self) -> bool {
        self.metadata.as_ref().is_some_and(|metadata| {
            usize::try_from(metadata.total_chunks)
                .is_ok_and(|total_chunks| self.frames.len() == total_chunks)
        })
    }

    fn frame_count(&self) -> usize {
        self.frames.len()
    }

    fn cached_payload_bytes(&self) -> u64 {
        self.frames
            .values()
            .map(|frame| frame.chunk.len() as u64)
            .sum()
    }

    fn missing_sequences(&self) -> anyhow::Result<Vec<u32>> {
        let metadata = self
            .metadata
            .as_ref()
            .context("no QR frames have been decoded")?;
        Ok((1..=metadata.total_chunks)
            .filter(|sequence| !self.frames.contains_key(sequence))
            .collect())
    }

    fn missing_ranges(&self) -> anyhow::Result<Vec<String>> {
        let width = self.sequence_width()?;
        Ok(missing_sequence_ranges(&self.missing_sequences()?)
            .into_iter()
            .map(|range| format_sequence_range(range, width))
            .collect())
    }

    fn sequence_width(&self) -> anyhow::Result<usize> {
        let metadata = self
            .metadata
            .as_ref()
            .context("no QR frames have been decoded")?;
        Ok(sequence_width(metadata.total_chunks))
    }

    fn encoded_payload(&self) -> anyhow::Result<Vec<u8>> {
        let metadata = self
            .metadata
            .as_ref()
            .context("no QR frames have been decoded")?;
        let total_chunks: usize = metadata
            .total_chunks
            .try_into()
            .context("chunk count does not fit on this platform")?;

        let mut encoded = Vec::new();
        for sequence in 1..=total_chunks {
            let frame = self
                .frames
                .get(&(sequence as u32))
                .with_context(|| format!("missing QR frame sequence {sequence}"))?;
            encoded.extend_from_slice(&frame.chunk);
        }

        if encoded.len() as u64 != metadata.encoded_len {
            bail!(
                "encoded length mismatch: expected {} byte(s), got {}",
                metadata.encoded_len,
                encoded.len()
            );
        }

        let hash = blake3::hash(&encoded);
        if hash != metadata.stream_hash {
            bail!("encoded payload hash mismatch");
        }

        Ok(encoded)
    }
}

fn cache_frame_files(cache_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(cache_dir)
        .with_context(|| format!("failed to read cache directory {}", cache_dir.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", cache_dir.display()))?;
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
            .is_some_and(|extension| extension == CACHE_EXTENSION)
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn cache_frame_filename(sequence: u32, total_chunks: u32) -> String {
    let width = sequence_width(total_chunks);
    format!("{sequence:0width$}.{CACHE_EXTENSION}")
}

fn sequence_width(total_chunks: u32) -> usize {
    total_chunks.to_string().len().max(6)
}

fn missing_sequence_ranges(sequences: &[u32]) -> Vec<(u32, u32)> {
    let mut ranges = Vec::new();
    let mut iter = sequences.iter().copied();
    let Some(mut start) = iter.next() else {
        return ranges;
    };
    let mut end = start;

    for sequence in iter {
        if sequence == end + 1 {
            end = sequence;
            continue;
        }

        ranges.push((start, end));
        start = sequence;
        end = sequence;
    }

    ranges.push((start, end));
    ranges
}

fn format_sequence_range((start, end): (u32, u32), width: usize) -> String {
    if start == end {
        format!("{start:0width$}")
    } else {
        format!("{start:0width$}-{end:0width$}")
    }
}

fn write_reassembled_output(store: &FrameStore, output: &Path) -> anyhow::Result<String> {
    let metadata = store
        .metadata
        .as_ref()
        .context("no QR frames have been decoded")?;
    let encoded = store.encoded_payload()?;
    let decoded = match metadata.compression {
        FrameCompression::None => encoded,
        FrameCompression::Zstd => zstd::stream::decode_all(encoded.as_slice())
            .context("failed to decompress zstd payload")?,
    };

    if decoded.len() as u64 != metadata.original_len {
        bail!(
            "decoded length mismatch: expected {} byte(s), got {}",
            metadata.original_len,
            decoded.len()
        );
    }

    let sha256 = sha256_hex(&decoded);
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create output directory {}", parent.display()))?;
    }
    fs::write(output, decoded)
        .with_context(|| format!("failed to write decoded output {}", output.display()))?;
    Ok(sha256)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        write!(&mut hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hex
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use image::Luma;
    use qrcode::{EcLevel, QrCode, Version, bits::Bits};

    use super::*;

    #[derive(Debug, Parser)]
    struct DecodeCli {
        #[command(flatten)]
        args: DecodeArgs,
    }

    fn test_frame(
        sequence: u32,
        total_chunks: u32,
        original_len: u64,
        encoded: &[u8],
        chunk: &[u8],
        compression: FrameCompression,
    ) -> Frame {
        Frame {
            sequence,
            total_chunks,
            original_len,
            encoded_len: encoded.len() as u64,
            stream_hash: blake3::hash(encoded),
            compression,
            chunk: chunk.to_vec(),
        }
    }

    fn test_qr_image(bytes: &[u8]) -> RgbaImage {
        for version in 1..=40 {
            let mut bits = Bits::new(Version::Normal(version));
            if bits
                .push_byte_data(bytes)
                .and_then(|_| bits.push_terminator(EcLevel::M))
                .is_ok()
            {
                return DynamicImage::ImageLuma8(
                    QrCode::with_bits(bits, EcLevel::M)
                        .unwrap()
                        .render::<Luma<u8>>()
                        .quiet_zone(true)
                        .module_dimensions(8, 8)
                        .build(),
                )
                .to_rgba8();
            }
        }

        panic!("test QR payload does not fit");
    }

    #[test]
    fn decode_cli_parses_monitor_and_retry_defaults() {
        let args = DecodeCli::try_parse_from([
            "decode",
            "--delay-between",
            "250",
            "--forward-keypress",
            "RightArrow",
            "--cache-dir",
            "cache",
            "--output",
            "out.bin",
        ])
        .unwrap()
        .args;

        assert_eq!(args.monitor, None);
        assert_eq!(args.initial_delay, DEFAULT_INITIAL_DELAY_MS);
        assert_eq!(args.retry_timeout, DEFAULT_RETRY_TIMEOUT_MS);
        assert!(!args.save_screenshots);

        let args = DecodeCli::try_parse_from([
            "decode",
            "--delay-between",
            "250",
            "--forward-keypress",
            "RightArrow",
            "--cache-dir",
            "cache",
            "--output",
            "out.bin",
            "--monitor",
            "2",
            "--initial-delay",
            "2000",
            "--retry-timeout",
            "1000",
            "--save-screenshots",
        ])
        .unwrap()
        .args;

        assert_eq!(args.monitor, Some(2));
        assert_eq!(args.initial_delay, 2_000);
        assert_eq!(args.retry_timeout, 1_000);
        assert!(args.save_screenshots);
    }

    fn test_decode_config(cache_dir: &Path) -> DecodeConfig {
        DecodeConfig {
            delay_between: Duration::ZERO,
            forward_keypress: Key::RightArrow,
            cache_dir: cache_dir.to_owned(),
            output: cache_dir.join("out.bin"),
            monitor: None,
            initial_delay: Duration::from_secs(60),
            retry_timeout: Duration::from_secs(60),
            save_screenshots: false,
        }
    }

    #[test]
    fn run_decode_stops_during_initial_delay_when_cancelled() {
        let cache = tempfile::tempdir().unwrap();
        let config = test_decode_config(cache.path());
        let mut events = Vec::new();

        let outcome = run_decode(&config, &AtomicBool::new(true), |event| {
            events.push(format!("{event:?}"));
        })
        .unwrap();

        assert!(matches!(outcome, DecodeOutcome::Cancelled));
        assert_eq!(events.len(), 1);
        assert!(events[0].starts_with("CacheLoaded"));
    }

    #[test]
    fn run_decode_writes_output_from_complete_cache_without_capturing() {
        let cache = tempfile::tempdir().unwrap();
        let data = b"hello ljosbru";
        FrameStore::default()
            .cache_frame(
                cache.path(),
                test_frame(1, 1, data.len() as u64, data, data, FrameCompression::None),
            )
            .unwrap();
        let config = test_decode_config(cache.path());

        let outcome = run_decode(&config, &AtomicBool::new(false), |_| {}).unwrap();

        let DecodeOutcome::Completed(summary) = outcome else {
            panic!("expected a completed decode");
        };
        assert_eq!(summary.total_chunks, 1);
        assert_eq!(summary.sha256, sha256_hex(data));
        assert_eq!(fs::read(&config.output).unwrap(), data);
    }

    #[test]
    fn cache_status_reports_missing_ranges() {
        let cache = tempfile::tempdir().unwrap();
        assert_eq!(
            cache_status(&cache.path().join("missing"))
                .unwrap()
                .total_frames,
            None
        );

        let encoded = b"abcdef";
        let mut store = FrameStore::default();
        for (sequence, chunk) in [(1, b"ab"), (3, b"ef")] {
            store
                .cache_frame(
                    cache.path(),
                    test_frame(sequence, 3, 6, encoded, chunk, FrameCompression::None),
                )
                .unwrap();
        }

        let status = cache_status(cache.path()).unwrap();
        assert_eq!(status.cached_frames, 2);
        assert_eq!(status.total_frames, Some(3));
        assert_eq!(status.missing_ranges, vec!["000002"]);
    }

    #[test]
    fn parses_frame_bytes_with_trailing_newline() {
        let encoded = b"hello world";
        let frame = test_frame(1, 1, 11, encoded, encoded, FrameCompression::None);
        let mut output = build_frame(frame.clone()).unwrap();
        output.push(b'\n');

        assert_eq!(parse_frames_from_bytes(&output), vec![frame]);
    }

    #[test]
    fn parses_rxing_byte_segments() {
        let encoded = b"hello world";
        let frame = test_frame(1, 1, 11, encoded, encoded, FrameCompression::None);
        let output = build_frame(frame.clone()).unwrap();
        let mut result = RXingResult::new("", Vec::new(), Vec::new(), BarcodeFormat::QR_CODE);
        result.putMetadata(
            RXingResultMetadataType::BYTE_SEGMENTS,
            RXingResultMetadataValue::ByteSegments(vec![output]),
        );

        assert_eq!(parse_rxing_result(&result), vec![frame]);
    }

    #[test]
    fn rxing_scanner_decodes_ljosbru_qr_frame() {
        let encoded = &[0xeb, 0x4d, 0xa0, 0x62];
        let frame = test_frame(1, 1, 4, encoded, encoded, FrameCompression::None);
        let qr_image = test_qr_image(&build_frame(frame.clone()).unwrap());
        let mut scanner = RxingScanner::new();

        assert_eq!(
            scanner.decode_ljosbru_frames(qr_image).unwrap(),
            vec![frame]
        );
    }

    #[test]
    fn parses_rxing_utf8_transcoded_text() {
        let encoded = &[0xeb, 0x4d, 0xa0, 0x62];
        let frame = test_frame(1, 1, 4, encoded, encoded, FrameCompression::None);
        let bytes = build_frame(frame.clone()).unwrap();
        let text = bytes
            .iter()
            .map(|byte| char::from(*byte))
            .collect::<String>();

        assert_eq!(parse_frames_from_text(&text), vec![frame]);
    }

    #[test]
    fn store_detects_duplicate_conflicts_and_metadata_mismatch() {
        let encoded = b"abcdef";
        let frame = test_frame(1, 2, 6, encoded, b"abc", FrameCompression::None);
        let mut store = FrameStore::default();

        assert_eq!(store.insert(frame.clone()).unwrap(), InsertStatus::Inserted);
        assert_eq!(store.insert(frame.clone()).unwrap(), InsertStatus::Existing);

        let mut conflicting = frame.clone();
        conflicting.chunk = b"xyz".to_vec();
        assert!(
            store
                .insert(conflicting)
                .unwrap_err()
                .to_string()
                .contains("conflicting")
        );

        let mismatch = test_frame(2, 2, 99, encoded, b"def", FrameCompression::None);
        assert!(
            store
                .insert(mismatch)
                .unwrap_err()
                .to_string()
                .contains("metadata")
        );
    }

    #[test]
    fn reassembly_requires_all_chunks_and_hash_match() {
        let encoded = b"abcdef";
        let mut store = FrameStore::default();
        store
            .insert(test_frame(1, 2, 6, encoded, b"abc", FrameCompression::None))
            .unwrap();
        assert_eq!(store.missing_sequences().unwrap(), vec![2]);
        assert_eq!(store.sequence_width().unwrap(), 6);
        assert_eq!(store.cached_payload_bytes(), 3);

        assert!(
            store
                .encoded_payload()
                .unwrap_err()
                .to_string()
                .contains("missing QR frame sequence 2")
        );

        store
            .insert(test_frame(2, 2, 6, encoded, b"def", FrameCompression::None))
            .unwrap();
        assert!(store.missing_sequences().unwrap().is_empty());
        assert_eq!(store.cached_payload_bytes(), 6);
        assert_eq!(store.encoded_payload().unwrap(), encoded);

        let mut corrupt = FrameStore::default();
        let mut frame = test_frame(1, 1, 6, encoded, b"abcdeg", FrameCompression::None);
        frame.encoded_len = 6;
        corrupt.insert(frame).unwrap();
        assert!(
            corrupt
                .encoded_payload()
                .unwrap_err()
                .to_string()
                .contains("hash mismatch")
        );
    }

    #[test]
    fn cache_roundtrip_reconstructs_compressed_output() {
        let tempdir = tempfile::tempdir().unwrap();
        let cache_dir = tempdir.path().join("cache");
        let output = tempdir.path().join("out.bin");
        let original = b"hello hello hello hello hello".to_vec();
        let encoded = zstd::stream::encode_all(original.as_slice(), 3).unwrap();
        let split = encoded.len() / 2;
        let frame_1 = test_frame(
            1,
            2,
            original.len() as u64,
            &encoded,
            &encoded[..split],
            FrameCompression::Zstd,
        );
        let frame_2 = test_frame(
            2,
            2,
            original.len() as u64,
            &encoded,
            &encoded[split..],
            FrameCompression::Zstd,
        );

        let mut store = FrameStore::load(&cache_dir).unwrap();
        store.cache_frame(&cache_dir, frame_1).unwrap();
        store.cache_frame(&cache_dir, frame_2).unwrap();

        let store = FrameStore::load(&cache_dir).unwrap();
        let sha256 = write_reassembled_output(&store, &output).unwrap();

        assert_eq!(fs::read(output).unwrap(), original);
        assert_eq!(
            sha256,
            "a181bf87a4251839d4b7133eeba71bd1a9c085cbfe14d404d7327a944e37917e"
        );
        assert!(cache_dir.join("000001.ljosbru-frame").exists());
        assert!(cache_dir.join("000002.ljosbru-frame").exists());
    }

    #[test]
    fn non_cache_files_are_ignored() {
        let tempdir = tempfile::tempdir().unwrap();
        fs::write(tempdir.path().join("note.txt"), b"ignore").unwrap();

        let store = FrameStore::load(tempdir.path()).unwrap();

        assert!(store.frames.is_empty());
    }

    #[test]
    fn print_missing_requires_metadata() {
        let store = FrameStore::default();

        assert!(
            store
                .missing_sequences()
                .unwrap_err()
                .to_string()
                .contains("no QR frames")
        );
    }

    #[test]
    fn sequence_width_has_six_digit_minimum() {
        assert_eq!(sequence_width(5), 6);
        assert_eq!(sequence_width(1_000_000), 7);
    }

    #[test]
    fn missing_sequence_ranges_group_consecutive_sequences() {
        assert_eq!(
            missing_sequence_ranges(&[3, 4, 5, 7, 9, 10]),
            vec![(3, 5), (7, 7), (9, 10)]
        );
        assert!(missing_sequence_ranges(&[]).is_empty());
    }

    #[test]
    fn format_sequence_range_uses_zero_padding() {
        assert_eq!(format_sequence_range((3, 7), 6), "000003-000007");
        assert_eq!(format_sequence_range((9, 9), 6), "000009");
    }
}
