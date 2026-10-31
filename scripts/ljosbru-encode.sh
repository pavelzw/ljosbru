#!/usr/bin/env bash
# Lightweight ljosbru encoder. Produces the same frames as ljosbru-encode, so
# the output can be read with ljosbru-decode or ljosbru-gui.
#
# Requires qrencode and b3sum, plus zstd for --compression zstd.

# ShellCheck 0.11 wrongly reports the helpers below as defined after use.
# shellcheck disable=SC2218
set -euo pipefail

readonly MAGIC="LJOSBRU1"
readonly VERSION=1
readonly HEADER_LEN=72
readonly MAX_FRAME_BYTES_PER_QR=2331
readonly QR_MODULE_PIXELS=8
readonly QR_QUIET_ZONE_MODULES=4
readonly ANSI_CLEAR_SCREEN=$'\033[2J\033[H'

usage() {
  cat <<EOF
Usage: $(basename "$0") <filename> --qr-size <bytes> [options]

Options:
  --qr-size <bytes>          Maximum QR data size in bytes, including ljosbru framing
  --compression <mode>       none (default), zstd, or zstd:<level>
  --output <directory>       Directory for generated PNGs [default: ./ljosbru-output/]
  --terminal                 Print QR codes to stdout instead of writing PNG files
  --yes                      Delete existing output PNG files without prompting
  --wait <mode>              auto (default), none, enter, or delay:<milliseconds>
  -h, --help                 Print help
EOF
}

die() {
  echo "error: $*" >&2
  exit 1
}

# Each argument is "<command>:<conda-forge package>". Fails with a pixi exec
# suggestion covering every missing command.
require() {
  local requirement missing=() specs="" invocation
  for requirement in "$@"; do
    if ! command -v "${requirement%%:*}" >/dev/null 2>&1; then
      missing+=("${requirement%%:*}")
      specs+=" -s ${requirement#*:}"
    fi
  done
  ((${#missing[@]} == 0)) && return
  invocation=$(printf "%q " "$0" ${original_args[@]+"${original_args[@]}"})
  die "${missing[*]} not found in PATH; consider running
  pixi exec$specs -- ${invocation% }"
}

# Write $2 as a big-endian unsigned integer of $1 bytes.
write_be() {
  local width=$1 value=$2 shift_bits escapes=""
  for ((shift_bits = (width - 1) * 8; shift_bits >= 0; shift_bits -= 8)); do
    escapes+=$(printf '\\%03o' $(((value >> shift_bits) & 0xff)))
  done
  printf '%b' "$escapes"
}

original_args=("$@")
filename=""
qr_size=""
compression="none"
output="./ljosbru-output/"
output_set=false
terminal=false
assume_yes=false
wait_mode="auto"

while (($# > 0)); do
  case "$1" in
    --qr-size) qr_size="${2:?--qr-size requires a value}"; shift 2 ;;
    --qr-size=*) qr_size="${1#*=}"; shift ;;
    --compression) compression="${2:?--compression requires a value}"; shift 2 ;;
    --compression=*) compression="${1#*=}"; shift ;;
    --output) output="${2:?--output requires a value}"; output_set=true; shift 2 ;;
    --output=*) output="${1#*=}"; output_set=true; shift ;;
    --terminal) terminal=true; shift ;;
    --yes) assume_yes=true; shift ;;
    --wait) wait_mode="${2:?--wait requires a value}"; shift 2 ;;
    --wait=*) wait_mode="${1#*=}"; shift ;;
    -h | --help) usage; exit 0 ;;
    -*) die "unknown option $1" ;;
    *)
      [[ -z "$filename" ]] || die "unexpected argument $1"
      filename="$1"
      shift
      ;;
  esac
done

[[ -n "$filename" ]] || { usage >&2; exit 2; }
[[ -n "$qr_size" ]] || die "--qr-size is required"
[[ -f "$filename" ]] || die "failed to read input file $filename"
if $terminal && $output_set; then
  die "--output cannot be used with --terminal"
fi

[[ "$qr_size" =~ ^[0-9]+$ ]] || die "--qr-size must be an integer"
qr_size=$((10#$qr_size))
((qr_size > HEADER_LEN)) ||
  die "--qr-size must be greater than $HEADER_LEN byte(s) to leave room for QR payload data"
((qr_size <= MAX_FRAME_BYTES_PER_QR)) ||
  die "--qr-size must be at most $MAX_FRAME_BYTES_PER_QR byte(s) with the current QR framing and error correction settings"

case "$compression" in
  none) compression_code=0 ;;
  zstd) compression="zstd:3"; compression_code=1 ;;
  zstd:*)
    level="${compression#zstd:}"
    [[ "$level" =~ ^[0-9]+$ ]] || die "expected zstd compression level to be an integer"
    level=$((10#$level))
    ((level >= 1 && level <= 22)) || die "expected zstd compression level to be between 1 and 22"
    compression_code=1
    ;;
  *) die "expected --compression to be \`none\`, \`zstd\`, or \`zstd:<level>\`" ;;
esac

case "$wait_mode" in
  auto)
    if $terminal && [[ -t 0 && -t 1 ]]; then wait_mode="enter"; else wait_mode="none"; fi
    ;;
  none) ;;
  enter) [[ -t 0 ]] || die "--wait enter requires interactive stdin" ;;
  delay:*)
    delay_ms="${wait_mode#delay:}"
    [[ "$delay_ms" =~ ^[0-9]+$ ]] || die "expected delay milliseconds to be an integer"
    delay_ms=$((10#$delay_ms))
    ;;
  *) die "expected --wait to be \`auto\`, \`none\`, \`enter\`, or \`delay:<milliseconds>\`" ;;
esac

requirements=(qrencode:libqrencode b3sum:b3sum)
((compression_code == 0)) || requirements+=(zstd:zstd)
require "${requirements[@]}"

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

encoded="$workdir/encoded"
if ((compression_code == 0)); then
  cp "$filename" "$encoded"
else
  level="${compression#zstd:}"
  ultra=()
  ((level <= 19)) || ultra=(--ultra)
  zstd -q ${ultra[@]+"${ultra[@]}"} "-$level" -c "$filename" >"$encoded"
fi

original_len=$(wc -c <"$filename" | tr -d ' ')
encoded_len=$(wc -c <"$encoded" | tr -d ' ')
chunk_size=$((qr_size - HEADER_LEN))
if ((encoded_len == 0)); then
  total_chunks=1
else
  total_chunks=$(((encoded_len + chunk_size - 1) / chunk_size))
fi

# Header fields shared by every frame, from the magic up to the stream hash.
common_header="$workdir/common-header"
{
  printf '%s' "$MAGIC"
  write_be 1 "$VERSION"
  write_be 1 "$compression_code"
  write_be 2 "$HEADER_LEN"
  printf '\0\0\0\0' # Sequence placeholder, replaced per frame below.
  write_be 4 "$total_chunks"
  write_be 8 "$original_len"
  write_be 8 "$encoded_len"
  b3sum --raw "$encoded"
} >"$common_header"
[[ $(wc -c <"$common_header" | tr -d ' ') -eq 68 ]] || die "failed to build frame header"

if ! $terminal; then
  mkdir -p "$output"
  shopt -s nullglob
  existing=("$output"/*.png)
  shopt -u nullglob
  if ((${#existing[@]} > 0)); then
    if ! $assume_yes; then
      [[ -t 0 ]] || die "$output contains ${#existing[@]} PNG file(s); pass --yes to delete them"
      read -r -p "Delete ${#existing[@]} existing PNG file(s) in $output? [y/N] " answer
      [[ "$answer" =~ ^[Yy]([Ee][Ss])?$ ]] || die "aborted"
    fi
    rm -f "${existing[@]}"
  fi
fi

filename_width=${#total_chunks}
((filename_width >= 6)) || filename_width=6
clear_screen=false
[[ -t 1 ]] && clear_screen=true

frame="$workdir/frame"
for ((sequence = 1; sequence <= total_chunks; sequence++)); do
  if ((sequence > 1)); then
    case "$wait_mode" in
      enter) read -r -p "Press Enter to continue..." _ </dev/tty >&2 ;;
      delay:*) sleep "$(printf '%d.%03d' $((delay_ms / 1000)) $((delay_ms % 1000)))" ;;
    esac
  fi

  chunk_len=$((encoded_len - (sequence - 1) * chunk_size))
  ((chunk_len <= chunk_size)) || chunk_len=$chunk_size
  {
    head -c 12 "$common_header"
    write_be 4 "$sequence"
    tail -c +17 "$common_header"
    write_be 4 "$chunk_len"
    dd if="$encoded" bs="$chunk_size" skip=$((sequence - 1)) count=1 2>/dev/null
  } >"$frame"

  if $terminal; then
    if $clear_screen; then
      printf '%s' "$ANSI_CLEAR_SCREEN"
    elif ((sequence > 1)); then
      echo
    fi
    echo "Frame $sequence/$total_chunks"
    qrencode -8 -l M -m "$QR_QUIET_ZONE_MODULES" -t ANSIUTF8 -o - <"$frame"
    echo "Encoding QR codes $sequence/$total_chunks" >&2
  else
    png="$output/$(printf "%0${filename_width}d" "$sequence").png"
    qrencode -8 -l M -m "$QR_QUIET_ZONE_MODULES" -s "$QR_MODULE_PIXELS" -t PNG -o "$png" <"$frame"
  fi
done

message="Encoded $original_len input byte(s) into $encoded_len payload byte(s) across $total_chunks QR code(s) with $compression compression"
if $terminal; then
  echo "$message to terminal" >&2
else
  echo "$message in $output"
fi
