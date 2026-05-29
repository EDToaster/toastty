.PHONY: check lint fmt test cover cover-gate cover-html clean install

# === Local development ===

check:
	cargo check --workspace --all-targets

lint:
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

test:
	cargo test --workspace

# === Coverage ===
#
# 95% gate on logic crates: parser, term, protocols, config, graphics,
# and the pure-function submodules of render.
# Excluded (covered by integration + snapshot tests): toastty (bin),
# toastty-pty, toastty-io, toastty-window, and render's device/pipeline code.
#
# Requires: cargo install cargo-llvm-cov

COVER_IGNORE := '/(crates/toastty/|toastty-pty/|toastty-io/|toastty-window/|toastty-render/src/(device|pipelines)/)'

cover-html:
	cargo llvm-cov --workspace --html --ignore-filename-regex $(COVER_IGNORE)

cover:
	cargo llvm-cov --workspace --summary-only --ignore-filename-regex $(COVER_IGNORE)

cover-gate:
	cargo llvm-cov --workspace --fail-under-lines 95 --ignore-filename-regex $(COVER_IGNORE)

# === Install ===
#
# Build a release binary and register it so it's launchable from the
# platform's app launcher (Spotlight on macOS, krunner / app menu on Linux).
# Pass env overrides straight through, e.g.: make install APP_NAME=Toastty

UNAME_S := $(shell uname -s)

install:
ifeq ($(UNAME_S),Darwin)
	./scripts/install_mac_app.sh
else ifeq ($(UNAME_S),Linux)
	./scripts/install_linux_app.sh
else
	@echo "unsupported platform: $(UNAME_S)" >&2; exit 1
endif

# === Cleanup ===

clean:
	cargo clean
