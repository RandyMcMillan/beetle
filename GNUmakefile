cargo-install-all:
	for t in */;  do echo $$t; cargo install --bins -vv --path $$t; done || \
	for t in */;  do echo $$t; cargo install --bins -vv --path $$t --features log || true; done
cargo-build-all:
	for t in */Cargo.toml;  do echo $$t; cargo build -vv --manifest-path $$t; done
cargo-test-all:
	for t in */Cargo.toml;  do echo $$t; cargo test -vv --manifest-path $$t; done
