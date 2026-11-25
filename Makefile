# SPDX-FileCopyrightText: 2026 Lorena Jiménez, Luis Liñán
# SPDX-License-Identifier: MIT

.PHONY: run check build-release deploy logs

DEPLOY_HOST ?= bt
DEPLOY_TARGET ?= aarch64-unknown-linux-musl
RELEASE_BIN := target/$(DEPLOY_TARGET)/release/bt

run:
	cargo run

check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test
	shellcheck tests/*.sh
	actionlint

build-release:
	cargo build --release --target $(DEPLOY_TARGET)

# Upload as bt.new, then rename: overwriting a running binary fails with "Text file busy".
deploy: build-release
	ssh $(DEPLOY_HOST) 'mkdir -p ~/.local/bin ~/.local/share/bt ~/.config/systemd/user'
	scp $(RELEASE_BIN) $(DEPLOY_HOST):.local/bin/bt.new
	scp deploy/bt.service $(DEPLOY_HOST):.config/systemd/user/bt.service
	ssh $(DEPLOY_HOST) 'mv ~/.local/bin/bt.new ~/.local/bin/bt'
	ssh $(DEPLOY_HOST) 'systemctl --user daemon-reload && systemctl --user enable bt && systemctl --user restart bt'

logs:
	ssh $(DEPLOY_HOST) 'journalctl --user -u bt -f'
