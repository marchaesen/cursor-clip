cargo build --release

systemctl --user stop cursor-clip.service

sudo cp ./target/release/cursor-clip /usr/local/bin
mkdir -p ~/.config/systemd/user/
cp cursor-clip.service ~/.config/systemd/user/

systemctl --user daemon-reload
systemctl --user enable cursor-clip.service
systemctl --user start cursor-clip.service

