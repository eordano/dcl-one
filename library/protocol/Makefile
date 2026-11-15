.PHONY: buf-lint buf-build buf-breaking test all install list-components-ids check-component-id

BIN="./node_modules/.bin"
BUF_VERSION=1.8.0
OS = $(shell uname -s)
OS_ARCH = $(shell uname -m)
# Wire compatibility is checked against this repository's main branch: its proto tree is
# exported with git archive (no clone, no LFS smudge, works from a worktree).
BUF_AGAINST_REF ?= origin/main

node_modules/.bin/buf:
	curl -sSL https://github.com/bufbuild/buf/releases/download/v$(BUF_VERSION)/buf-$(OS)-$(OS_ARCH) -o node_modules/.bin/buf
	chmod +x node_modules/.bin/buf

buf-lint: node_modules/.bin/buf
	./node_modules/.bin/buf lint proto/

buf-build: node_modules/.bin/buf
	./node_modules/.bin/buf build proto/

export BUF_AGAINST_REF
buf-breaking: node_modules/.bin/buf
	@bash scripts/check-proto-compabitility.sh

test: buf-lint
	bash scripts/test.sh

all: buf-lint buf-build test

install:
	npm i
	rm -rf proto/google || true
	cp -r node_modules/protobufjs/google proto/google

list-components-ids:
	@bash scripts/list-components-ids.sh

# Preserve literal input through both Make expansion and shell argument parsing.
override ID := $(value ID)
export ID
check-component-id:
	@bash scripts/check-component-id.sh "$${ID}"
