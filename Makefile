# pyroclear — сборка/установка через make.
#
# Cargo может отсутствовать в PATH (например, rustup-прокси удалён). Тогда
# автоматически подхватывается тулчейн из ~/.rustup/toolchains/stable-*/bin.
# Переопределить вручную:  make CARGO=/путь/к/cargo
# Поставить в другой каталог: make PREFIX=/usr/local install

CARGO ?= $(shell \
  command -v cargo 2>/dev/null || \
  ls $(HOME)/.rustup/toolchains/stable-*/bin/cargo 2>/dev/null | head -1)

# Каталог выбранного cargo добавляется в PATH, чтобы cargo нашёл rustc
# даже при отсутствии rustup-прокси.
CARGO_DIR := $(patsubst %/,%,$(dir $(CARGO)))

PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
BIN    := target/release/pyroclear

.PHONY: all build test install run clean help

all: build

build: ## release-сборка
	@test -n "$(CARGO)" || { echo "✗ cargo не найден ни в PATH, ни в ~/.rustup/toolchains"; exit 1; }
	PATH="$(CARGO_DIR):$$PATH" "$(CARGO)" build --release

test: ## unit-тесты
	@test -n "$(CARGO)" || { echo "✗ cargo не найден"; exit 1; }
	PATH="$(CARGO_DIR):$$PATH" "$(CARGO)" test

install: build ## собрать и положить в $(BINDIR) (по умолч. ~/.local/bin)
	@mkdir -p "$(BINDIR)"
	cp -f "$(BIN)" "$(BINDIR)/pyroclear"
	@echo "→ установлен в $(BINDIR)/pyroclear"

run: build ## собрать и запустить release-бинарь (без установки)
	./$(BIN)

clean: ## cargo clean
	@test -n "$(CARGO)" || exit 0
	PATH="$(CARGO_DIR):$$PATH" "$(CARGO)" clean

help: ## показать список целей
	@awk 'BEGIN{FS=":.*##"} /^[a-zA-Z_-]+:.*##/ {printf "  \033[36m%-10s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
