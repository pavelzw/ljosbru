<div align="center">

[![License][license-badge]](LICENSE)
[![CI Status][ci-badge]][ci]
[![Conda Platform][conda-badge]][conda-url]
[![Conda Downloads][conda-downloads-badge]][conda-url]

[license-badge]: https://img.shields.io/github/license/pavelzw/ljosbru?style=flat-square
[ci-badge]: https://img.shields.io/github/actions/workflow/status/pavelzw/ljosbru/ci.yml?style=flat-square&branch=main
[ci]: https://github.com/pavelzw/ljosbru/actions/
[conda-badge]: https://img.shields.io/conda/vn/conda-forge/ljosbru?style=flat-square
[conda-downloads-badge]: https://img.shields.io/conda/dn/conda-forge/ljosbru?style=flat-square
[conda-url]: https://prefix.dev/channels/conda-forge/packages/ljosbru

</div>

# ljosbru

Transfer data through QR codes.

`ljosbru` turns an input file into a numbered sequence of QR PNGs and can later
decode that sequence from screenshots of a monitor. It is useful when the only
available path between two machines is a screen and an automated screenshot loop.

## Install

From source:

```sh
cargo build --release
./target/release/ljosbru --help
```

If installed from conda-forge:

```sh
pixi global install -c conda-forge ljosbru
ljosbru --help
```

## Encode

Create QR images from a file:

```sh
ljosbru encode examples/input.bin \
  --qr-size 1200 \
  --compression zstd:3 \
  --output ljosbru-output \
  --yes
```

This writes numbered PNG files such as `000001.png`, `000002.png`, and so on.
The QR payload contains ljosbru framing metadata: sequence number, total chunk
count, original size, compressed size, compression mode, and a BLAKE3 hash of
the encoded payload.

Useful encode options:

| Option | Description |
| --- | --- |
| `--qr-size <bytes>` | Maximum framed data bytes per QR code. Must be at most `2331`. Smaller values are easier to scan. |
| `--compression none` | Store the input bytes directly. This is the default. |
| `--compression zstd` | Compress with zstd level 3. |
| `--compression zstd:<level>` | Compress with a zstd level from 1 to 22. |
| `--output <directory>` | Directory for generated PNGs. Defaults to `./ljosbru-output/`. |
| `--yes` | Delete existing PNGs in the output directory without prompting. |

## Decode

Show the generated QR images on a monitor, for example in a slide deck or image
viewer where one keypress advances to the next image. Then run:

```sh
ljosbru decode \
  --delay-between 100 \
  --forward-keypress RightArrow \
  --cache-dir ljosbru-cache \
  --output output.bin \
  --initial-delay 3000
```

The decoder waits for `--initial-delay`, screenshots the selected monitor,
extracts a ljosbru QR frame, caches it, presses `--forward-keypress`, waits
`--delay-between`, and repeats until every frame has been cached. At the end it
reassembles the file, verifies the encoded BLAKE3 hash, writes `--output`, and
prints the SHA-256 of the decoded output file.

Useful decode options:

| Option | Description |
| --- | --- |
| `--cache-dir <directory>` | Stores decoded frame files as `000001.ljosbru-frame`, etc. Reuse this directory to resume an interrupted decode. |
| `--output <file>` | Reassembled output file. |
| `--forward-keypress <key>` | Key to press after each decoded frame, such as `RightArrow`, `DownArrow`, or `Space`. See Enigo's [`Key` definitions](https://github.com/enigo-rs/enigo/blob/main/src/keycodes.rs) for the full list. |
| `--delay-between <milliseconds>` | Wait time after pressing the forward key. |
| `--initial-delay <milliseconds>` | Startup delay before the first screenshot. Defaults to `1000`. |
| `--retry-timeout <milliseconds>` | How long to keep trying to find a new frame before failing. Defaults to `5000`. |
| `--monitor <index>` | Monitor index to capture. Defaults to the primary monitor when available. |
| `--save-screenshots` | Save screenshots to `<cache-dir>/<timestamp>-screenshot.png` for debugging. |

The decode progress bar shows cached frame count, payload bytes per second, ETA,
and elapsed time.

On macOS, decoding may require Screen Recording permission for screenshots and
Accessibility permission for automated keypresses.

## Resume And Inspect Missing Frames

The cache makes decode resumable. If decoding stops part-way through, rerun the
same `decode` command with the same `--cache-dir`; existing frames are reused.

To print missing frame IDs:

```sh
ljosbru print-missing --cache-dir ljosbru-cache
```

Output is zero-padded and grouped into ranges:

```text
000003-000007
000014
```

## Debugging

Normal runs keep logs quiet. Enable detailed decoder logs with:

```sh
RUST_LOG=debug ljosbru decode \
  --delay-between 100 \
  --forward-keypress RightArrow \
  --cache-dir ljosbru-cache \
  --output output.bin \
  --save-screenshots
```

If decoding cannot find QR codes, use `--save-screenshots` and inspect the saved
PNG files to confirm the selected monitor contains a clearly visible QR code.
Try a longer `--initial-delay`, a larger `--delay-between`, a different
`--monitor`, or a smaller `--qr-size` when generating the QR images.

## Development

The decoder links against `zbar`, so install the native library before building
or running tests.

```sh
cargo fmt
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```
