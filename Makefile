PREFIX ?= /usr
LIBDIR ?= $(PREFIX)/lib
DESTDIR ?=

.PHONY: build test install uninstall build-macos test-macos package-macos

build:
	cargo build --release --locked

test:
	cargo test --locked

install: build
	install -Dm755 target/release/libfcitx5_candidate_translator.so \
		$(DESTDIR)$(LIBDIR)/fcitx5/libfcitx5-candidate-translator.so
	install -Dm644 data/candidate-translator.conf \
		$(DESTDIR)$(PREFIX)/share/fcitx5/addon/candidate-translator.conf

uninstall:
	rm -f $(DESTDIR)$(LIBDIR)/fcitx5/libfcitx5-candidate-translator.so
	rm -f $(DESTDIR)$(PREFIX)/share/fcitx5/addon/candidate-translator.conf

build-macos:
	./scripts/macos.sh build

test-macos:
	./scripts/macos.sh test

package-macos:
	./scripts/macos.sh package
