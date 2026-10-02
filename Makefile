.PHONY: setup release

CARGO ?= cargo
BINARY := diet_soda
INSTALL_DIR := $(HOME)/.cargo/bin
LOCAL_BIN := $(HOME)/.local/bin
CONFIG_FILE := $(HOME)/.config/diet_soda/config.json

release:
	$(CARGO) build --release --locked --bin $(BINARY)
	mkdir -p "$(INSTALL_DIR)"
	install -m 755 "target/release/$(BINARY)" "$(INSTALL_DIR)/$(BINARY)"

setup: release
	mkdir -p "$(LOCAL_BIN)"
	ln -sfn "$(INSTALL_DIR)/$(BINARY)" "$(LOCAL_BIN)/$(BINARY)"
	@if [ ! -f "$(CONFIG_FILE)" ]; then \
		"$(INSTALL_DIR)/$(BINARY)" --init; \
	else \
		echo "config already initialized, skipping init"; \
	fi
