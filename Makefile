.PHONY: release

CARGO ?= cargo
BINARY := diet_soda
INSTALL_DIR := $(HOME)/.cargo/bin

release:
	$(CARGO) build --release --locked --bin $(BINARY)
	mkdir -p "$(INSTALL_DIR)"
	install -m 755 "target/release/$(BINARY)" "$(INSTALL_DIR)/$(BINARY)"
