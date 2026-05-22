.PHONY: check lint fmt test cover cover-gate cover-html clean

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

# === Cleanup ===

clean:
	cargo clean
