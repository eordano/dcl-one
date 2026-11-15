
ifneq ($(CI), true)
LOCAL_ARG = --local --verbose --diagnostics
endif

PROTOBUF_VERSION = 3.20.1
ifeq ($(shell uname),Darwin)
PROTOBUF_ZIP = protoc-$(PROTOBUF_VERSION)-osx-x86_64.zip
else
ifeq ($(shell arch),aarch64)
PROTOBUF_ZIP = protoc-$(PROTOBUF_VERSION)-linux-aarch_64.zip
else
PROTOBUF_ZIP = protoc-$(PROTOBUF_VERSION)-linux-x86_64.zip
endif
endif

SED_OPTION = -i
ifeq ($(shell uname),Darwin)
SED_OPTION=-i ""
endif

# Set SDK_PROTOC to generate with an installed protoc: a name on PATH (the Nix shell's `protoc`,
# or brew's on a Mac without Rosetta) or a path; it is resolved to an absolute path and exported
# for the TS generators. Not PROTOC, which prost reads for the Rust crates. CI leaves it unset
# and install-protobuf downloads 3.20.1, so generated code in a PR comes from that version.
PROTOC_DOWNLOADED = node_modules/.bin/protobuf/bin/protoc
ifdef SDK_PROTOC
SDK_PROTOC_RESOLVED := $(shell command -v "$(SDK_PROTOC)")
ifeq ($(SDK_PROTOC_RESOLVED),)
$(error SDK_PROTOC=$(SDK_PROTOC) is not an executable on PATH or a path to one)
endif
override SDK_PROTOC := $(abspath $(SDK_PROTOC_RESOLVED))
export SDK_PROTOC
SDK_PROTOC_BIN = $(SDK_PROTOC)
else
SDK_PROTOC_BIN = $(PROTOC_DOWNLOADED)
endif
SCENE_PROTO_FILES := $(wildcard node_modules/@dcl/protocol/proto/decentraland/kernel/apis/*.proto)
PBS_TS = $(SCENE_PROTO_FILES:node_modules/@dcl/protocol/proto/decentraland/kernel/apis/%.proto=scripts/rpc-api-generation/src/proto/%.gen.ts)
TSC = node_modules/.bin/tsc
ESLINT = node_modules/.bin/eslint
SYNC_PACK = node_modules/.bin/syncpack
JEST = node_modules/.bin/jest
# library/hashing is a pnpm package; a machine with pnpm on its PATH can pass PNPM=pnpm
PNPM ?= corepack pnpm@11.27.0
export PNPM

# this DEVELOPER_MODE is important to not send developer's events to the same segment
# stream as the production ones. Look for it's usage on the analytics component
export DEVELOPER_MODE=true

install:
	cd ../protocol && $(PNPM) install --frozen-lockfile --ignore-scripts && $(PNPM) exec bash scripts/generate.sh
	$(PNPM) install --frozen-lockfile --ignore-scripts
	$(MAKE) sync-deps
	$(PNPM) --filter @dcl/hashing run build
	node ../../tooling/check-local-hashing.mjs packages/@dcl/sdk-commands
ifndef SDK_PROTOC
	test -x "$(PROTOC_DOWNLOADED)" || $(MAKE) install-protobuf
endif

lint:
	npx tsx scripts/lint-packages.ts

sync-deps:
	$(SYNC_PACK) format --config .syncpackrc.json --source "packages/*/package.json" --source "package.json"
	$(SYNC_PACK) fix-mismatches --config .syncpackrc.json --source "packages/*/package.json" --source "package.json"

lint-packages:
	$(SYNC_PACK) list-mismatches --config .syncpackrc.json  --source "packages/*/package.json" --source "package.json"
	$(SYNC_PACK) format --config .syncpackrc.json  --source "packages/*/package.json" --source "package.json"

lint-fix: sync-deps
	npx tsx scripts/lint-packages.ts --fix

test:
	node_modules/.bin/jest --detectOpenHandles --forceExit --colors test/

test-cli:
	@rm -rf tmp
	@mkdir -p tmp/scene
	cd tmp/scene; $(PWD)/packages/@dcl/sdk-commands/dist/index.js init

test-file:
	node_modules/.bin/jest --detectOpenHandles --colors --runTestsByPath $(FILE)

init-test-scene:
	git clone https://github.com/decentraland/sdk7-scene-template test-scene
	cd test-scene && npm i ./../packages/@dcl/sdk  && npm i ./../packages/@dcl/sdk-commands  && npm i ./../packages/@dcl/js-runtime

format:
	npx prettier --write "**/*.{js,ts,tsx,json}" --loglevel=error

install-protobuf:
	curl -OL https://github.com/protocolbuffers/protobuf/releases/download/v$(PROTOBUF_VERSION)/$(PROTOBUF_ZIP)
	unzip -o $(PROTOBUF_ZIP) -d node_modules/.bin/protobuf
	rm $(PROTOBUF_ZIP)
	chmod +x $(PROTOC_DOWNLOADED)

docs: | install build
	node_modules/.bin/jest --detectOpenHandles --colors --runInBand --runTestsByPath scripts/docs.spec.ts
# Cloudflare doesn't allow a directory called functions. 🪄🎩
	mv api-docs/functions api-docs/funcs
# replace the paths of /functions to /funcs
	find ./api-docs -type f -name '*.html' \
  	| xargs sed ${SED_OPTION} -E 's:(href="[^"]+)functions/:\1funcs/:g'

build:
	make clean
	node_modules/.bin/jest --detectOpenHandles --colors --runInBand --runTestsByPath scripts/build.spec.ts

prepare:
	node_modules/.bin/jest --detectOpenHandles --colors --runInBand --runTestsByPath scripts/prepare.spec.ts

scripts/rpc-api-generation/src/proto/%.gen.ts: node_modules/@dcl/protocol/proto/decentraland/kernel/apis/%.proto $(SDK_PROTOC_BIN)
	@$(SDK_PROTOC_BIN)  \
			--plugin=./node_modules/.bin/protoc-gen-dcl_ts_proto \
			--dcl_ts_proto_opt=esModuleInterop=true,returnObservable=false,outputServices=generic-definitions \
			--dcl_ts_proto_opt=fileSuffix=.gen \
			--dcl_ts_proto_opt=onlyTypes=true \
			--dcl_ts_proto_out="$(PWD)/scripts/rpc-api-generation/src/proto" \
			-I="$(PWD)/scripts/rpc-api-generation/src/proto" \
			-I="$(PWD)/node_modules/@dcl/protocol/proto/" \
			"$(PWD)/node_modules/@dcl/protocol/proto/decentraland/kernel/apis/$*.proto";

compile_apis: ${PBS_TS}

deep-clean-and-snapshot:
	git clean -fxd
	make install
	make lint-fix
	make build
	make update-snapshots

.PHONY: build test install docs deep-clean-and-snapshot update-snapshots lint-packages

deep-clean:
	rm -rf node_modules/ \
		packages/@dcl/ecs/node_modules/ \
		packages/@dcl/react-ecs/node_modules/ \
		packages/@dcl/sdk/node_modules/ \
		packages/@dcl/sdk-commands/node_modules \
	make clean

update-snapshots: export UPDATE_SNAPSHOTS=true
update-snapshots: build test

clean:
	@echo "> Cleaning all folders"
	@rm -rf coverage/
	@rm -rf packages/@dcl/sdk/*.js packages/@dcl/sdk/*.d.ts packages/@dcl/sdk/internal packages/@dcl/sdk/testing
	@rm -rf packages/@dcl/ecs/dist/ packages/@dcl/sdk/dist/
	@rm -rf packages/@dcl/sdk-commands/dist
	@rm -rf packages/@dcl/ecs/src/components/generated/ packages/@dcl/ecs/temp/
	@rm -rf packages/@dcl/js-runtime/apis.d.ts
	@rm -rf packages/@dcl/react-ecs/dist/ packages/@dcl/react-ecs/src/generated/ packages/@dcl/react-ecs/temp/
	@rm -rf packages/@dcl/sdk/package-lock.json packages/@dcl/sdk/types/env/ packages/@dcl/sdk/types/rpc-modules/
	@rm -rf scripts/rpc-api-generation/src/modules/ scripts/rpc-api-generation/src/proto/
	@rm -rf test/build-ecs/fixtures/dcl-test-lib-integration/bin/ test/build-ecs/fixtures/dcl-test-lib-integration/node_modules/
	@rm -rf test/build-ecs/fixtures/ecs7-scene/bin/ test/build-ecs/fixtures/ecs7-scene/node_modules/
	@rm -rf test/build-ecs/fixtures/simple-scene-with-bundled/bin/ test/build-ecs/fixtures/simple-scene-with-bundled/node_modules/
	@rm -rf test/build-ecs/fixtures/simple-scene-with-library/bin/ test/build-ecs/fixtures/simple-scene-with-library/node_modules/
	@rm -rf test/build-ecs/fixtures/simple-scene/bin/ test/build-ecs/fixtures/simple-scene/node_modules/
	@rm -rf test/ecs/snippets/dist/
