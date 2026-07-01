#!/usr/bin/env bash
# Fetch the PDF/image ML pipeline's native dependencies for a Node.js/Bun app
# using the `fleischwolf` npm package. A thin, curl-pipeable convenience
# wrapper around `installDependencies()` (crates/fleischwolf-node/deps.js) —
# all the real download/path logic lives there; this just drives it from a
# directory that may not have `fleischwolf` installed yet.
#
# Run from your app's directory (where `fleischwolf` is/will be an npm dep):
#   curl -fsSL https://raw.githubusercontent.com/artiz/fleischwolf/master/scripts/setup_nodejs_dependencies.sh | bash
# or, from a checkout of this repo:
#   bash scripts/setup_nodejs_dependencies.sh
#
# Downloads (to ~/.cache/fleischwolf by default; override with $FLEISCHWOLF_HOME):
#   - libpdfium + the PP-OCRv3 recognition model — from their own public releases
#   - the RT-DETR layout model + TableFormer — PyTorch->ONNX exports of
#     docling-project's own models (Apache-2.0 / CDLA-Permissive-2.0), hosted
#     as GitHub Release assets on this repo (see MODELS_NOTICE.md)
#
# Idempotent: skips anything already downloaded. Pass --force to re-fetch
# everything. Set FLEISCHWOLF_MODELS_URL to use your own model export/host
# instead of fleischwolf's.
#
# This only *caches* the files — your app still needs to call
# `await installDependencies()` once at startup (before converting any
# PDF/image) so the native pipeline in *that* process picks them up; with
# everything already on disk, that call becomes an instant no-op.
#
# Requires: node (with npm) and network access.
set -euo pipefail

FORCE=false
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=true ;;
    *)
      echo "usage: setup_nodejs_dependencies.sh [--force]" >&2
      exit 2
      ;;
  esac
done

if ! command -v node >/dev/null 2>&1; then
  echo "error: node is required (https://nodejs.org)" >&2
  exit 1
fi

# Make sure `fleischwolf` is resolvable from the current directory — install
# it locally if it isn't (piping this script via curl means there's no
# checked-out repo/node_modules to begin with).
if ! node -e "require.resolve('fleischwolf')" >/dev/null 2>&1; then
  if [ -f package.json ]; then
    echo "→ installing the fleischwolf npm package"
    npm install fleischwolf
  else
    echo "error: no package.json in $(pwd) and 'fleischwolf' isn't resolvable." >&2
    echo "  run this from your Node app's directory (with a package.json), or:" >&2
    echo "    npm init -y && npm install fleischwolf" >&2
    exit 1
  fi
fi

node -e "
const { installDependencies } = require('fleischwolf');
installDependencies({ force: ${FORCE}, onProgress: (m) => console.error('  ' + m) })
  .then((status) => {
    console.error('');
    console.error('done — installed under ' + status.home);
    console.error(JSON.stringify(status, null, 2));
  })
  .catch((err) => {
    console.error(err.message);
    process.exitCode = 1;
  });
"
