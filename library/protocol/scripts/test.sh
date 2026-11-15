#!/bin/bash

set -euo pipefail

echo "> Checking compatibility and component-ID tooling"
node --test scripts/tooling.test.cjs

echo "> Checking bitwise generator goldens"
node protoc-gen-bitwise/test/generator.test.js

bash "$(dirname "$0")/generate.sh"

echo "> Checking room recovery wire compatibility"
node --test scripts/room-recovery.test.cjs
