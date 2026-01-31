# aegis-mcp Makefile

PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
LIBDIR ?= $(PREFIX)/lib

.PHONY: all build release install uninstall clean help

all: build

build:
	cargo build --workspace

release:
	cargo build --workspace --release

install: release
	@echo "Installing aegis-mcp to $(BINDIR)"
	@mkdir -p $(BINDIR) $(LIBDIR)
	@# Remove conflicting cargo install if present
	@if [ -f "$(HOME)/.cargo/bin/aegis-mcp" ] && [ "$(BINDIR)" != "$(HOME)/.cargo/bin" ]; then \
		echo "Removing old cargo install at ~/.cargo/bin/aegis-mcp"; \
		rm -f "$(HOME)/.cargo/bin/aegis-mcp"; \
	fi
	@# Remove before copy to handle "text file busy" (binary in use)
	rm -f $(BINDIR)/aegis-mcp
	cp target/release/aegis-mcp $(BINDIR)/
	cp target/release/libaegis_hooks.so $(LIBDIR)/
	@echo ""
	@echo "Installed:"
	@echo "  $(BINDIR)/aegis-mcp"
	@echo "  $(LIBDIR)/libaegis_hooks.so"
	@echo ""
	@echo "NOTE: Run 'hash -r' or restart your shell to clear command cache"
	@echo ""
	@$(BINDIR)/aegis-mcp --version

uninstall:
	@echo "Removing aegis-mcp from $(BINDIR)"
	rm -f $(BINDIR)/aegis-mcp
	rm -f $(LIBDIR)/libaegis_hooks.so
	@echo "Uninstalled."

clean:
	cargo clean

help:
	@echo "aegis-mcp Makefile"
	@echo ""
	@echo "Targets:"
	@echo "  build     - Build debug binaries (default)"
	@echo "  release   - Build release binaries"
	@echo "  install   - Build release and install to PREFIX (default: ~/.local)"
	@echo "  uninstall - Remove installed files"
	@echo "  clean     - Remove build artifacts"
	@echo ""
	@echo "Variables:"
	@echo "  PREFIX    - Installation prefix (default: ~/.local)"
	@echo "  BINDIR    - Binary directory (default: PREFIX/bin)"
	@echo "  LIBDIR    - Library directory (default: PREFIX/lib)"
	@echo ""
	@echo "Examples:"
	@echo "  make install                    # Install to ~/.local"
	@echo "  make install PREFIX=/usr/local  # Install system-wide (needs sudo)"
