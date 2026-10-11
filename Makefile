BINARY  := $(shell . ./script/lib && echo $$BINARY)
VERSION := $(shell . ./script/lib && echo $$VERSION)

# macOS keeps /usr/bin read-only even for root
ifeq ($(shell uname -s),Darwin)
BINDIR := /usr/local/bin
else
BINDIR := /usr/bin
endif

ifeq ($(DESTDIR),)
SUDO := $(shell [ -w $(BINDIR) ] || echo sudo)
endif

# Static musl builds so one binary runs across distros, and mise can hand it
# out without worrying about the host's glibc. cross carries the musl toolchain
# for the C dependencies (bundled SQLite, ring, and the onig regex engine that
# candle's tokenizers build pulls in).
TARGETS = \
	x86_64-unknown-linux-musl \
	aarch64-unknown-linux-musl

# cross has no macOS images, but zig carries enough of the macOS SDK to
# compile the C dependencies and link against libSystem, and signs the arm64
# binary ad hoc the way Apple Silicon insists on
MAC_TARGETS = \
	x86_64-apple-darwin \
	aarch64-apple-darwin

.PHONY: build install build-all release clean

build:
	cargo fetch --locked
	cargo build --release

install: build
	$(SUDO) install -d $(DESTDIR)$(BINDIR)
	$(SUDO) install -m755 target/release/$(BINARY) $(DESTDIR)$(BINDIR)/$(BINARY)

build-all:
	cargo fetch --locked
	@set -e; for target in $(TARGETS); do \
		echo "Building for $$target..."; \
		cross build --release --locked --target $$target; \
		arch=$$(echo $$target | sed 's/x86_64/amd64/;s/aarch64/arm64/;s/-.*//'); \
		mkdir -p dist; \
		cp target/$$target/release/$(BINARY) dist/$(BINARY)-linux-$$arch; \
	done
	@set -e; for target in $(MAC_TARGETS); do \
		echo "Building for $$target..."; \
		rustup target add $$target; \
		cargo zigbuild --release --locked --target $$target; \
		arch=$$(echo $$target | sed 's/x86_64/amd64/;s/aarch64/arm64/;s/-.*//'); \
		mkdir -p dist; \
		cp target/$$target/release/$(BINARY) dist/$(BINARY)-macos-$$arch; \
	done
	@echo "Artifacts in dist/"

release: build-all
	./script/release-github
	@echo
	@echo "Released v$(VERSION)"
	@echo

clean:
	cargo clean
	rm -rf dist
