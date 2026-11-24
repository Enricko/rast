#!/bin/bash
# nvenc-bench.sh — compare hardware NVENC (h264_nvenc) with software x264.
#
# The xpra runtime cannot use NVENC on this host (xpra 6.5.5's nvenc module is
# built against NVENC SDK 13.1, the 595.99 driver exposes API 13.0), so this
# benchmark measures the silicon directly with ffmpeg: the same 1080p60 source
# is encoded by h264_nvenc and by libx264 while CPU user time and
# `nvidia-smi` utilization are sampled. The xpra/x264 numbers come from the
# real xpra session (`xpra info` + /proc/<pid> CPU).
#
# Usage:
#   scripts/gpu/nvenc-bench.sh                 # 1080p60, 6 s per encoder
#   scripts/gpu/nvenc-bench.sh 1920x1080 60 6  # WxH, fps, seconds
#   scripts/gpu/nvenc-bench.sh --with-xpra     # also measure a disposable xpra session
#
# Requirements: ffmpeg (h264_nvenc + libx264), nvidia-smi. No package changes.
set -u

SIZE="${1:-1920x1080}"; FPS="${2:-60}"; DURATION="${3:-6}"

SRC="testsrc2=size=${SIZE}:rate=${FPS}"
OUTDIR="${TMPDIR:-/tmp}/rast-nvenc-bench"
mkdir -p "$OUTDIR"

sample_gpu() { # sample nvidia-smi for $1 seconds, append to $2
    local secs=$1 out=$2 pid
    : >"$out"
    nvidia-smi --query-gpu=utilization.gpu,utilization.encoder,power.draw,memory.used \
        --format=csv,noheader,nounits -l 1 >"$out" 2>/dev/null &
    pid=$!
    sleep "$secs"
    kill "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
}

bench_encoder() { # $1 = label, $2 = ffmpeg encoder args
    local label=$1; shift
    local gpu_csv="$OUTDIR/gpu-$label.csv"
    local out="$OUTDIR/out-$label.mp4"
    echo "--- $label"
    /usr/bin/time -f "wall=%es cpu_user=%Us cpu_sys=%Ss maxrss=%MkB" \
        ffmpeg -hide_banner -loglevel error -y \
        -f lavfi -i "$SRC" -t "$DURATION" "$@" -an "$out" 2>"$OUTDIR/time-$label.txt" &
    local ff=$!
    sample_gpu $((DURATION + 2)) "$gpu_csv"
    wait "$ff" || { echo "  ffmpeg failed"; cat "$OUTDIR/time-$label.txt"; return 1; }
    echo "  $(cat "$OUTDIR/time-$label.txt")"
    echo "  size: $(stat -c %s "$out" 2>/dev/null || echo 0) bytes"
    if [ -s "$gpu_csv" ]; then
        echo "  nvidia-smi (gpu%, enc%, W, MiB) mean:"
        awk -F', ' '{g+=$1; e+=$2; p+=$3; m+=$4; n++}
            END {if (n) printf "    gpu=%.0f%% enc=%.0f%% power=%.1fW mem=%.0fMiB samples=%d\n", g/n, e/n, p/n, m/n, n}' "$gpu_csv"
    fi
    echo "  raw: $gpu_csv $OUTDIR/time-$label.txt"
}

echo "source: $SRC, ${DURATION}s per encoder, output in $OUTDIR"
echo
bench_encoder nvenc -c:v h264_nvenc -preset p1 -b:v 8M
echo
bench_encoder x264 -c:v libx264 -preset ultrafast -b:v 8M

if [ "${4:-}" = "--with-xpra" ] || [ "${1:-}" = "--with-xpra" ]; then
    echo
    echo "--- xpra software encoder (x264) on a disposable session"
    echo "    start one manually, e.g.:"
    echo "      sudo -u rast xpra start :105 --bind-tcp=127.0.0.1:18999 --html=on --auth=none \\"
    echo "        --xvfb=Xvfb --start=xterm --video-encoders=x264"
    echo "    then: sudo -u rast xpra info :105 | grep -iE 'encoding|encoder'"
fi

echo
echo "note: on this host xpra's nvenc cannot be measured - the module is built"
echo "against NVENC SDK 13.1 while driver 595.99 exposes API 13.0 (see nvenc-probe.py)."
