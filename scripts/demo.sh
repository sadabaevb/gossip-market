#!/usr/bin/env bash
# The devnet demo on one screen: the local verifier on top, three agents side by side below.
# Alice and Bob start at once and cannot trade. Carol starts when you press Enter in her pane,
# and the ring of three settles. Needs tmux. See docs/DEMO.md for what to say and type.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release
bin=target/release/gossipmarket
session=gossipmarket

# Keeps a pane open after its node quits, so that what it printed last can be read and copied.
node() {
  echo "sleep 1; $bin node --config demo/$1.toml; echo; read -r -p '$1 has stopped. Enter closes this pane. '"
}

tmux kill-session -t "$session" 2>/dev/null || true
tmux new-session -d -s "$session" -x "$(tput cols)" -y "$(tput lines)" "$bin devnet --genesis demo/devnet.toml"
tmux split-window -v -t "$session" -l 85% "$(node alice)"
tmux split-window -h -t "$session" -l 66% "$(node bob)"
tmux split-window -h -t "$session" -l 50% "read -r -p 'Press Enter to start carol. '; $(node carol)"
tmux attach -t "$session"
