BINARY  := $(shell . ./script/lib && echo $$BINARY)
VERSION := $(shell . ./script/lib && echo $$VERSION)

ifeq ($(DESTDIR),)
SUDO := $(shell [ -w /usr/bin ] || echo sudo)
endif

# Static musl builds so one binary runs across distros, and mise can hand it
# out without worrying about the host's glibc. cross carries the musl toolchain
# for the C dependencies (bundled SQLite, ring).
TARGETS = \
	x86_64-unknown-linux-musl \
	aarch64-unknown-linux-musl

.PHONY: build install build-all release clean

build:
	cargo fetch --locked
	cargo build --release

install: build
	$(SUDO) install -Dm755 target/release/$(BINARY) $(DESTDIR)/usr/bin/$(BINARY)

build-all:
	cargo fetch --locked
	@set -e; for target in $(TARGETS); do \
		echo "Building for $$target..."; \
		cross build --release --locked --target $$target; \
		arch=$$(echo $$target | sed 's/x86_64/amd64/;s/aarch64/arm64/;s/-.*//'); \
		mkdir -p dist; \
		cp target/$$target/release/$(BINARY) dist/$(BINARY)-linux-$$arch; \
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
