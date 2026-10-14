#!/usr/bin/env bash

# Stop at the first failure.
set -euo pipefail

cargo build --release

systemctl --user stop cursor-clip.service || true

sudo cp ./target/release/cursor-clip ./target/release/cursor-clip-toggle /usr/local/bin
mkdir -p ~/.config/systemd/user/
cp cursor-clip.service ~/.config/systemd/user/

systemctl --user daemon-reload
systemctl --user enable cursor-clip.service
systemctl --user start cursor-clip.service
