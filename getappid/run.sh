if [ ! -f ./target/release/getappid ]; then
  cargo build --release
fi
./target/release/getappid
