.PHONY: help build release clean run serve serve-port serve-dev stop stop-all scheme-test app-build app-scheme-register keys-add keys-list keys-remove keys-migrate test-web test-web-wrapped test-web-status health

BIN_DEBUG := ./target/debug/hostless
BIN_RELEASE := ./target/release/hostless
BIN_ROOT := ./hostless
PORT ?= 11434
WEB_PORT ?= 4173
APP_DEV_BUNDLE := /Users/don/dev/hostless/app/build/dev-macos-arm64/hostless-app-dev.app
LSREGISTER := /System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister

# Ad-hoc codesign so macOS Keychain grants persist across runs
help:
	@echo "Hostless Make targets"
	@echo "  make build                         Build debug binary + codesign + copy to ./hostless"
	@echo "  make release                       Build release binary + codesign + copy to ./hostless"
	@echo "  make serve                         Start proxy on default port (11434)"
	@echo "  make serve-port PORT=15055         Start proxy on custom port"
	@echo "  make serve-dev PORT=11434          Start proxy in --dev-mode"
	@echo "  make stop                          Stop daemon (if running)"
	@echo "  make stop-all                      Stop all hostless processes"
	@echo "  make app-scheme-register           Register @app bundle for hostless://"
	@echo "  make scheme-test                   Trigger hostless:// URL from terminal"
	@echo "  make app-build                     Build desktop app (@app)"
	@echo "  make keys-add PROVIDER=openai KEY=sk-... [BASE_URL=...]"
	@echo "  make keys-list                     List stored providers"
	@echo "  make keys-remove PROVIDER=openai   Remove provider key"
	@echo "  make keys-migrate                  Migrate legacy keys.vault to keys.env"
	@echo "  make test-web WEB_PORT=4173        Host test webapp"
	@echo "  make test-web-wrapped WEB_PORT=4173 PORT=11434  Host test webapp via hostless run (auto-start daemon)"
	@echo "  make test-web-status WEB_PORT=4173 PORT=11434   Show daemon/route/port status"
	@echo "  make health PORT=11434             Curl /health"

build:
	cargo build
	codesign -f -s - $(BIN_DEBUG)
	cp $(BIN_DEBUG) $(BIN_ROOT)

release:
	cargo build --release
	codesign -f -s - $(BIN_RELEASE)
	cp $(BIN_RELEASE) $(BIN_ROOT)

run: serve

serve: build
	$(BIN_DEBUG) serve --port $(PORT)

serve-port: serve

serve-dev: build
	$(BIN_DEBUG) serve --port $(PORT) --dev-mode

stop: build
	$(BIN_DEBUG) stop

stop-all:
	@pkill -TERM -x hostless || true
	@sleep 1
	@if pgrep -x hostless >/dev/null; then \
		echo "Force killing remaining hostless processes..."; \
		pkill -KILL -x hostless || true; \
	fi
	@pgrep -fl hostless || echo "No hostless processes running"

scheme-test:
	open "hostless://register?origin=http%3A%2F%2Flocalhost%3A$(WEB_PORT)&callback=http%3A%2F%2Flocalhost%3A$(WEB_PORT)&state=make-test"

app-build:
	cd app && bun run build

app-scheme-register:
	@if [ ! -d "$(APP_DEV_BUNDLE)" ]; then \
		echo "@app bundle not found at $(APP_DEV_BUNDLE). Run: make app-build"; \
		exit 1; \
	fi
	@if [ ! -x "$(LSREGISTER)" ]; then \
		echo "lsregister tool not found at $(LSREGISTER)"; \
		exit 1; \
	fi
	"$(LSREGISTER)" -f "$(APP_DEV_BUNDLE)"
	@echo "Registered @app as hostless:// handler candidate: $(APP_DEV_BUNDLE)"

keys-add: build
	@if [ -z "$(PROVIDER)" ] || [ -z "$(KEY)" ]; then \
		echo "Usage: make keys-add PROVIDER=openai KEY=sk-... [BASE_URL=https://...]"; \
		exit 1; \
	fi
	@if [ -n "$(BASE_URL)" ]; then \
		$(BIN_DEBUG) keys add "$(PROVIDER)" "$(KEY)" --base-url "$(BASE_URL)"; \
	else \
		$(BIN_DEBUG) keys add "$(PROVIDER)" "$(KEY)"; \
	fi

keys-list: build
	$(BIN_DEBUG) keys list

keys-remove: build
	@if [ -z "$(PROVIDER)" ]; then \
		echo "Usage: make keys-remove PROVIDER=openai"; \
		exit 1; \
	fi
	$(BIN_DEBUG) keys remove "$(PROVIDER)"

keys-migrate: build
	$(BIN_DEBUG) keys migrate

test-web:
	cd ../test-web && python3 -m http.server $(WEB_PORT)

test-web-wrapped: build
	@if ! curl -fsS "http://localhost:$(PORT)/health" >/dev/null 2>&1; then \
		echo "Hostless daemon not running on port $(PORT); starting proxy in background..."; \
		"$(CURDIR)/target/debug/hostless" serve --port $(PORT) >/tmp/hostless-$(PORT).log 2>&1 & \
		for i in 1 2 3 4 5 6 7 8 9 10; do \
			if curl -fsS "http://localhost:$(PORT)/health" >/dev/null 2>&1; then break; fi; \
			sleep 0.5; \
		done; \
		if ! curl -fsS "http://localhost:$(PORT)/health" >/dev/null 2>&1; then \
			echo "Failed to start hostless on port $(PORT). Recent log output:"; \
			tail -n 40 /tmp/hostless-$(PORT).log || true; \
			exit 1; \
		fi; \
	fi
	@PIDS=$$(lsof -tiTCP:$(WEB_PORT) -sTCP:LISTEN 2>/dev/null || true); \
	if [ -n "$$PIDS" ]; then \
		echo "Port $(WEB_PORT) already in use; stopping existing listener(s): $$PIDS"; \
		kill $$PIDS || true; \
		sleep 0.5; \
	fi
	@"$(CURDIR)/target/debug/hostless" route remove test-web --daemon-port $(PORT) >/dev/null 2>&1 || true
	cd ../test-web && "$(CURDIR)/target/debug/hostless" run test-web --port $(WEB_PORT) --daemon-port $(PORT) -- python3 -m http.server $(WEB_PORT)

test-web-status:
	@echo "=== Hostless health (port $(PORT)) ==="
	@curl -fsS "http://localhost:$(PORT)/health" || echo "not reachable"
	@echo ""
	@echo "=== Hostless routes ==="
	@"$(CURDIR)/target/debug/hostless" route list || echo "routes unavailable"
	@echo ""
	@echo "=== Listener on WEB_PORT $(WEB_PORT) ==="
	@lsof -nP -iTCP:$(WEB_PORT) -sTCP:LISTEN || echo "no listener"
	@echo ""
	@echo "=== hostless processes ==="
	@pgrep -fl hostless || echo "no hostless process"

health:
	curl "http://localhost:$(PORT)/health"

clean:
	cargo clean
	rm hostless

