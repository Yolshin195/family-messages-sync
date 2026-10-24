# family-messages-sync: сборка, установка и автозапуск (macOS)
#
#   make deploy      всё сразу: установка, настройка, доступ, автозапуск
#   make help        список команд

NAME      := family-messages-sync
LABEL     := local.$(NAME)

BIN_DIR   ?= $(HOME)/.local/bin
AGENT_DIR ?= $(HOME)/Library/LaunchAgents
LOG_DIR   ?= $(HOME)/Library/Logs/$(NAME)
INTERVAL  ?= 300

TARGET_DIR := $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),target)
BUILT      := $(TARGET_DIR)/release/$(NAME)
BIN        := $(BIN_DIR)/$(NAME)
PLIST      := $(AGENT_DIR)/$(LABEL).plist
CONFIG     := $(HOME)/.config/$(NAME)/config.json
GUI        := gui/$(shell id -u)

# LaunchAgent: запуск при входе в систему и каждые $(INTERVAL) секунд.
# Во сне launchd не работает, после пробуждения сообщения догоняются.
# Ротации у launchd нет: out.log и err.log больше 1 МБ приложение само переносит в .1 (FAMILY_SYNC_LOG_DIR, src/logs.rs).
define PLIST_XML
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$(LABEL)</string>
  <key>ProgramArguments</key>
  <array>
    <string>$(BIN)</string>
    <string>run</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>StartInterval</key>
  <integer>$(INTERVAL)</integer>
  <key>ProcessType</key>
  <string>Background</string>
  <key>StandardOutPath</key>
  <string>$(LOG_DIR)/out.log</string>
  <key>StandardErrorPath</key>
  <string>$(LOG_DIR)/err.log</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>FAMILY_SYNC_LOG_DIR</key>
    <string>$(LOG_DIR)</string>
  </dict>
</dict>
</plist>
endef
export PLIST_XML

SHELL := /bin/bash
.DEFAULT_GOAL := help
.PHONY: help build test lint fmt install setup init dry fda plist agent deploy run-now status logs stop uninstall

help: ## Показать список команд
	@grep -E '^[a-z-]+:.*##' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  make %-9s %s\n", $$1, $$2}'

build: ## Собрать release-бинарник
	cargo build --release

test: ## Запустить тесты
	cargo test

lint: ## Проверки CI: форматирование и clippy (предупреждения — ошибки)
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

fmt: ## Отформатировать код (rustfmt.toml)
	cargo fmt

install: build ## Собрать и скопировать бинарник в ~/.local/bin
	mkdir -p $(BIN_DIR)
	install -m 755 $(BUILT) $(BIN)
	@echo "Установлено: $(BIN)"
	@echo "Если после обновления пропал доступ к Messages, выдайте Full Disk Access заново: make fda"

setup: ## Интерактивная настройка: config.json и API-токен в Keychain
	$(BIN) setup

init: ## Проверить токен и счета, создать категории автоматических операций
	$(BIN) init

dry: ## Показать, что будет создано, ничего не отправляя
	$(BIN) run --dry-run

fda: ## Открыть Full Disk Access и показать бинарник в Finder
	open -R $(BIN)
	open "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles"
	@echo "Добавьте $(BIN) через «+» (или перетащите из Finder) и включите переключатель."

plist: ## Сгенерировать LaunchAgent plist
	mkdir -p $(AGENT_DIR) $(LOG_DIR)
	@printf '%s\n' "$$PLIST_XML" > $(PLIST)
	@echo "Создан $(PLIST)"

agent: plist ## Включить автозапуск (launchd)
	-launchctl bootout $(GUI)/$(LABEL) 2>/dev/null
	@sleep 1
	launchctl bootstrap $(GUI) $(PLIST)
	@echo "Автозапуск включён: при входе в систему и каждые $(INTERVAL) с."

deploy: install ## Установка, настройка, доступ и автозапуск одной командой
	@test -f $(CONFIG) || $(BIN) setup
	@$(MAKE) --no-print-directory fda
	@read -r -p "Выдайте доступ и нажмите Enter для включения автозапуска... " _
	@$(MAKE) --no-print-directory agent

run-now: ## Запустить обработку прямо сейчас через launchd
	launchctl kickstart -k $(GUI)/$(LABEL)

status: ## Состояние автозапуска и импорта
	@launchctl print $(GUI)/$(LABEL) 2>/dev/null | grep -E '^[[:space:]]*(state|runs|last exit code) =' || echo "Агент не загружен"
	@$(BIN) status

logs: ## Смотреть логи (Ctrl+C для выхода)
	@mkdir -p $(LOG_DIR) && touch $(LOG_DIR)/out.log $(LOG_DIR)/err.log
	tail -n 50 -F $(LOG_DIR)/out.log $(LOG_DIR)/err.log

stop: ## Выключить автозапуск
	-launchctl bootout $(GUI)/$(LABEL)

uninstall: stop ## Удалить автозапуск и бинарник (конфиг и Keychain остаются)
	rm -f $(PLIST) $(BIN)
	@echo "Удалено. Конфиг ($(dir $(CONFIG))) и API-токен в Keychain (сервис $(NAME)) остались, см. README."