# Developer tasks for the ledger; `make help` lists them.
CARGO ?= cargo
PROFILE ?= release
IMAGE ?= example/ledger
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml)
TARGET_DIR := target/$(PROFILE)
SOURCES := $(wildcard src/*.rs src/*/*.rs)

.PHONY: all build test lint docker clean help

all: build

build: $(TARGET_DIR)/ledger ## Build the CLI

$(TARGET_DIR)/ledger: $(SOURCES) Cargo.toml Cargo.lock
	$(CARGO) build --profile $(PROFILE) --locked

test: ## Run the tests
	$(CARGO) test --locked

lint: ## Check formatting and lints
	$(CARGO) fmt --check
	$(CARGO) clippy --all-targets -- -D warnings

docker: ## Build the container image
	docker build --build-arg VERSION=$(VERSION) -t $(IMAGE):$(VERSION) .

clean:
	$(CARGO) clean
	rm -rf web/dist

help:
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F ':.*## ' '{ printf "%-12s %s\n", $$1, $$2 }'
