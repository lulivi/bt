# SPDX-FileCopyrightText: 2026 Lorena Jiménez, Luis Liñán
# SPDX-License-Identifier: MIT

.PHONY: run check build-release deploy logs

DEPLOY_HOST ?= bt
DEPLOY_TARGET ?= aarch64-unknown-linux-musl
RELEASE_BIN := target/$(DEPLOY_TARGET)/release/bt

# Files that need no SPDX header. The [-] in the check keeps the Makefile from matching itself.
NO_HEADER := ':!*.md' ':!*.lock' ':!*.png' ':!LICENSE' ':!.gitignore'

run:
	cargo run

check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test
	shellcheck tests/*.sh
	actionlint
	@git rev-parse --is-inside-work-tree >/dev/null
	@missing=$$(for tag in 'SPDX[-]FileCopyrightText:' 'SPDX[-]License-Identifier:'; do \
		git grep -L "$$tag" -- $(NO_HEADER); done | sort -u); \
	[ -z "$$missing" ] || { echo "Missing SPDX header:"; echo "$$missing"; exit 1; }

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
