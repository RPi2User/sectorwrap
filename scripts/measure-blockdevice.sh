#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: measure-blockdevice.sh [options] DEVICE

Run read benchmarks on a block device and append one CSV row per test.
Read tests are the default. Write tests require --write-tests.
The default profile increases block size, queue depth, and parallelism in stages.

Options:
  -o, --output FILE       CSV output (default: blockdevice-measurement.csv)
  -s, --size SIZE         fio test size (default: 1G)
  -b, --block-size SIZE   fio block size (default: 128K)
  -t, --runtime SECONDS   Runtime per test (default: 30)
  -j, --jobs COUNT        Override numjobs for every stage
      --stages LIST       Comma-separated stages (default: all five stages)
      --runs COUNT        Repeat every test COUNT times (default: 1)
      --write-tests       Also run sequential and random write tests
      --direct 0|1        Use direct I/O (default: 1)
  -h, --help              Show this help

Examples:
  sudo ./scripts/measure-blockdevice.sh -o /tmp/ublk.csv /dev/ublkb0
  sudo ./scripts/measure-blockdevice.sh --size 4G --runtime 60 \
      --write-tests /dev/ublkb0
  sudo ./scripts/measure-blockdevice.sh \
      --stages baseline,large-block,queue4,queue16,parallel4 /dev/ublkb0

Stage profiles:
  baseline      128K block, iodepth 1, numjobs 1
  large-block   1M block, iodepth 1, numjobs 1
  queue4        1M block, iodepth 4, numjobs 1
  queue16       1M block, iodepth 16, numjobs 1
  parallel4     1M block, iodepth 16, numjobs 4
EOF
}

output="blockdevice-measurement.csv"
size="1G"
block_size="128K"
runtime=30
jobs=""
stages="baseline,large-block,queue4,queue16,parallel4"
runs=1
write_tests=0
direct=1

while (($#)); do
    case "$1" in
        -o|--output) output=$2; shift 2 ;;
        -s|--size) size=$2; shift 2 ;;
        -b|--block-size) block_size=$2; shift 2 ;;
        -t|--runtime) runtime=$2; shift 2 ;;
        -j|--jobs) jobs=$2; shift 2 ;;
        --stages) stages=$2; shift 2 ;;
        --runs) runs=$2; shift 2 ;;
        --write-tests) write_tests=1; shift ;;
        --direct) direct=$2; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        --) shift; break ;;
        -*) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
        *) break ;;
    esac
done

if (($# != 1)); then
    echo "exactly one block device is required" >&2
    usage >&2
    exit 2
fi

device=$1
if [[ ! -b "$device" ]]; then
    echo "not a block device: $device" >&2
    exit 2
fi
command -v fio >/dev/null || {
    echo "fio is required; install it with your distribution package manager" >&2
    exit 2
}
command -v python3 >/dev/null || {
    echo "python3 is required to convert fio results to CSV" >&2
    exit 2
}

[[ "$direct" == 0 || "$direct" == 1 ]] || {
    echo "--direct must be 0 or 1" >&2
    exit 2
}
[[ "$runtime" =~ ^[1-9][0-9]*$ ]] || {
    echo "--runtime must be a positive integer" >&2
    exit 2
}
if [[ -n "$jobs" && ! "$jobs" =~ ^[1-9][0-9]*$ ]]; then
    echo "--jobs must be a positive integer" >&2
    exit 2
fi
[[ "$runs" =~ ^[1-9][0-9]*$ ]] || {
    echo "--runs must be a positive integer" >&2
    exit 2
}

output_dir=$(dirname -- "$output")
mkdir -p -- "$output_dir"
tmp_dir=$(mktemp -d)
trap 'rm -rf -- "$tmp_dir"' EXIT

python3 - "$output" <<'PY'
import csv
import os
import sys

path = sys.argv[1]
header = [
    "timestamp_utc", "device", "stage", "test", "rw", "block_size",
    "iodepth", "numjobs", "size", "runtime_s", "direct", "run", "bytes",
    "io_bytes_per_s", "iops", "runtime_ms", "latency_mean_ns",
    "latency_p50_ns", "latency_p95_ns", "latency_p99_ns",
]
new_file = not os.path.exists(path) or os.path.getsize(path) == 0
if new_file:
    with open(path, "w", newline="", encoding="utf-8") as file:
        csv.writer(file).writerow(header)
else:
    with open(path, newline="", encoding="utf-8") as file:
        existing = next(csv.reader(file), [])
    if existing != header:
        raise SystemExit(
            "CSV header is incompatible; choose a new output file"
        )
PY

stage_config() {
    case "$1" in
        baseline) echo "$block_size 1 1" ;;
        large-block) echo "1M 1 1" ;;
        queue4) echo "1M 4 1" ;;
        queue16) echo "1M 16 1" ;;
        parallel4) echo "1M 16 4" ;;
        *) echo "unknown benchmark stage: $1" >&2; exit 2 ;;
    esac
}

run_test() {
    local stage=$1
    local rw=$2
    local stage_block_size=$3
    local iodepth=$4
    local numjobs=$5
    local run=$6
    local name="${stage}-${rw}-run${run}"
    local result="$tmp_dir/$name.json"

    echo "Running $name on $device (bs=$stage_block_size, iodepth=$iodepth, numjobs=$numjobs) ..."
    fio \
        --name="$name" \
        --filename="$device" \
        --rw="$rw" \
        --bs="$stage_block_size" \
        --size="$size" \
        --runtime="$runtime" \
        --time_based \
        --iodepth="$iodepth" \
        --numjobs="$numjobs" \
        --direct="$direct" \
        --group_reporting \
        --output-format=json \
        --output="$result" \
        $(if [[ "$rw" == read* ]]; then echo --readonly; fi)

    python3 - "$result" "$output" "$device" "$stage" "$name" "$rw" \
        "$stage_block_size" "$iodepth" "$numjobs" "$size" "$runtime" \
        "$direct" "$run" <<'PY'
import csv
import datetime
import json
import sys

(
    result, output, device, stage, name, rw, block_size, iodepth,
    numjobs, size, runtime, direct, run
) = sys.argv[1:]
with open(result, encoding="utf-8") as file:
    data = json.load(file)
key = "read" if rw in ("read", "randread") else "write"
jobs_data = [job[key] for job in data["jobs"] if key in job]
io_bytes = sum(item.get("io_bytes", 0) for item in jobs_data)
runtime_ms = max((item.get("runtime", 0) for item in jobs_data), default=0)
seconds = runtime_ms / 1000
bytes_per_second = io_bytes / seconds if seconds else 0
iops = sum(item.get("iops", 0) for item in jobs_data)
latencies = [item.get("clat_ns", {}) for item in jobs_data]
count = sum(item.get("count", 0) for item in latencies)
latency_mean = (
    sum(item.get("mean", 0) * item.get("count", 0) for item in latencies) / count
    if count else ""
)
percentiles = latencies[0].get("percentile", {}) if len(latencies) == 1 else {}

def percentile(value):
    return percentiles.get(value, "")

with open(output, "a", newline="", encoding="utf-8") as file:
    csv.writer(file).writerow([
        datetime.datetime.now(datetime.timezone.utc).isoformat(), device, stage,
        name, rw, block_size, iodepth, numjobs, size, runtime, direct,
        run, io_bytes, bytes_per_second, iops, runtime_ms,
        latency_mean, percentile("50.000000"),
        percentile("95.000000"), percentile("99.000000"),
    ])
PY
}

IFS=',' read -r -a stage_list <<< "$stages"
for stage in "${stage_list[@]}"; do
    read -r stage_block_size iodepth numjobs < <(stage_config "$stage")
    [[ -n "$jobs" ]] && numjobs=$jobs
    for run in $(seq "$runs"); do
        run_test "$stage" read "$stage_block_size" "$iodepth" "$numjobs" "$run"
        run_test "$stage" randread "$stage_block_size" "$iodepth" "$numjobs" "$run"
        if ((write_tests)); then
            echo "WARNING: write tests modify $device" >&2
            run_test "$stage" write "$stage_block_size" "$iodepth" "$numjobs" "$run"
            run_test "$stage" randwrite "$stage_block_size" "$iodepth" "$numjobs" "$run"
        fi
    done
done

echo "CSV written to $output"
