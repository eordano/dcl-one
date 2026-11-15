.PHONY: check figma-key-guard owners bevy-web bevy-headless bevy-avatar-render catalyst-build catalyst-test sites-install sites-build

NODE_CRATES = --workspace --exclude dcl-scene-cli --exclude dcl-scene-authoring --exclude dcl-scene-build --exclude dcl-preview-tunnel --exclude dcl-preview-tunnel-protocol --features catalyst/test-support

check:
	bash services/catalyst/scripts/check-module-dag.sh
	python3 tooling/ui_standards.py
	python3 tooling/ui_standards.py --token-floor

# Fails when the private Figma file key (FIGMA_FILE_KEY) or a Figma file URL is in the working tree or
# in origin/main..HEAD. Extra flags through ARGS, e.g. ARGS="--path library/ui/figma/review/review-dist".
figma-key-guard:
	node library/ui/figma/key-guard.mjs $(ARGS)

owners:
	python3 tooling/owners.py

bevy-web:
	$(MAKE) -C apps/bevy-explorer web

bevy-headless:
	$(MAKE) -C apps/bevy-explorer headless

bevy-avatar-render:
	$(MAKE) -C apps/bevy-explorer avatar-render

catalyst-build:
	cd services/catalyst && cargo build --locked --bin catalyst

catalyst-test:
	tooling/catalyst-test.sh $(NODE_CRATES)

sites-install:
	pnpm --dir apps/ui3 install --frozen-lockfile
	pnpm --dir apps/sites install --frozen-lockfile

sites-build:
	pnpm --dir apps/sites build
