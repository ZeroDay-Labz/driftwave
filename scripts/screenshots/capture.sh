#!/usr/bin/env bash
# Regenerate docs/screenshots/*.png without touching your desktop or speakers:
# driftwave runs in hidden tmux sessions with audio going to a temporary silent
# PipeWire sink, and each screen is rendered to PNG by render.py.
#
#   scripts/screenshots/capture.sh
#
# Needs: tmux, pactl (PipeWire/PulseAudio), python3 + Pillow, and a built
# target/release/driftwave (it builds one if missing).
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
OUT="$ROOT/docs/screenshots"
ENV="$ROOT/target/screenshot-env"
BIN="$ROOT/target/release/driftwave"
RENDER="$ROOT/scripts/screenshots/render.py"
SOCK="driftwave-shots"
W=150 H=42

[ -x "$BIN" ] || cargo build --release --manifest-path "$ROOT/Cargo.toml"
mkdir -p "$OUT" "$ENV/home/.config" "$ENV/ans"

wait_s() { python3 -c "import time; time.sleep($1)"; }
tm() { tmux -L "$SOCK" "$@"; }

# --- silent audio: a null sink only this run uses -------------------------
DEFAULT_SINK=$(pactl get-default-sink)
MODULE=$(pactl load-module module-null-sink sink_name=driftwave_shots \
    sink_properties=device.description=driftwave-screenshots)
[ "$(pactl get-default-sink)" != "$DEFAULT_SINK" ] && pactl set-default-sink "$DEFAULT_SINK"
printf 'pcm.!default { type pipewire playback_node "driftwave_shots" }\nctl.!default { type pipewire }\n' \
    > "$ENV/home/.asoundrc"
# A stand-in "terminal" for the lyrics pop-out: another hidden tmux session.
cat > "$ENV/term" <<EOF
#!/usr/bin/env bash
[ "\$1" = "-e" ] && shift
tmux -L $SOCK new-session -d -s lyrics -x 66 -y $H "\$(printf '%q ' "\$@")"
EOF
chmod +x "$ENV/term"

cleanup() {
    tm kill-server 2>/dev/null || true
    pactl unload-module "$MODULE" 2>/dev/null || true
    rm -rf "$ENV/home"
}
trap cleanup EXIT

run() {  # run <search words...>
    tm kill-server 2>/dev/null || true
    rm -rf "$ENV/home/.config/driftwave"
    tm new-session -d -s app -x $W -y $H \
        "HOME='$ENV/home' DRIFTWAVE_NO_MPRIS=1 DRIFTWAVE_NO_NOTIFY=1 DRIFTWAVE_TERMINAL='$ENV/term' exec '$BIN' $*"
    wait_s 7
}
grab() { tm capture-pane -e -p -t "$1" > "$ENV/ans/$2.ans"; }
screen() { tm capture-pane -p -t "${1:-app}"; }
play_first_ok() {  # start the first result that isn't DRM/preview, wait until it plays
    local rows; rows=$(screen | grep -nE '^│. +[0-9]+ ' | grep -vE 'DRM|preview' | head -1 | cut -d: -f1)
    local first; first=$(screen | grep -nE '^│. +[0-9]+ ' | head -1 | cut -d: -f1)
    for _ in $(seq $((rows - first))); do tm send-keys -t app Down; done
    tm send-keys -t app Enter
    for _ in $(seq 1 100); do screen | grep -qE 'AAC 160k|MP3 128k' && break; wait_s 0.1; done
}
liveliest() {  # capture a few frames, keep the one with the most visualizer bars
    local best=0 n
    for i in 1 2 3 4 5 6; do
        tm capture-pane -e -p -t app > "$ENV/ans/frame.ans"
        n=$(grep -o '[▃▄▅▆▇█]' "$ENV/ans/frame.ans" | wc -l)
        if [ "$n" -gt "$best" ]; then best=$n; cp "$ENV/ans/frame.ans" "$ENV/ans/$1.ans"; fi
        wait_s 0.35
    done
}

echo "1/4 player"
run "'#synthwave'"
play_first_ok
wait_s 6
liveliest player
python3 "$RENDER" "$OUT/player.png" "$ENV/ans/player.ans" --title "driftwave"

echo "2/4 genre page"
run "'#nerdcore'"
play_first_ok
for _ in 1 2 3; do tm send-keys -t app Right; done  # past the quiet intro
wait_s 4
tm send-keys -t app Home; wait_s 0.3
liveliest genre
python3 "$RENDER" "$OUT/genre.png" "$ENV/ans/genre.ans" --title "driftwave — #nerdcore"

echo "3/4 synced lyrics"
# The official upload is DRM-only, so this also shows driftwave switching to
# the label's playable upload.
run aesop rock rings
tm send-keys -t app Enter
for _ in $(seq 1 150); do screen | grep -qE 'AAC 160k|MP3 128k' && break; wait_s 0.1; done
for _ in 1 2 3 4 5 6; do tm send-keys -t app Right; done
wait_s 3
tm send-keys -t app l; wait_s 2
liveliest lyrics
python3 "$RENDER" "$OUT/lyrics.png" "$ENV/ans/lyrics.ans" --title "driftwave"

echo "4/4 pop-out lyrics window"
tm send-keys -t app Escape; wait_s 0.3
tm send-keys -t app L; wait_s 3
for _ in 1 2 3; do tm send-keys -t app Right; done
wait_s 2.5
grab app popout-player
tm capture-pane -e -p -t lyrics > "$ENV/ans/popout-lyrics.ans"
python3 "$RENDER" "$OUT/popout.png" "$ENV/ans/popout-player.ans" --title "driftwave" \
    "$ENV/ans/popout-lyrics.ans" --title "driftwave lyrics"

echo "done: $(ls "$OUT")"
