// Dependency provisioning for the PDF/image ML pipeline.
//
// The declarative backends (Markdown, HTML, DOCX, XLSX, …) are pure Rust and
// need nothing. The PDF/image path needs native assets that are NOT bundled in
// the addon (they're large and licensed separately from fleischwolf's own MIT
// code), mirroring how Python docling downloads its models on first use:
//
//   - libpdfium            (PDF text extraction + page rasterization) — required for PDF
//   - RT-DETR layout model (models/layout_heron.onnx)                 — required for PDF & image
//   - PP-OCR rec + dict    (models/ocr_rec.onnx, ppocr_keys_v1.txt)   — used for pages with no text layer
//   - TableFormer          (models/tableformer/{encoder,decoder,bbox}.onnx) — optional; geometric fallback otherwise
//
// All four download automatically via `installDependencies()`, no extra
// configuration needed. pdfium and the OCR model come from their own public
// upstream releases. The layout model and TableFormer are PyTorch→ONNX exports
// of docling-project's own models (`docling-project/docling-layout-heron`,
// Apache-2.0; `docling-project/docling-models`, CDLA-Permissive-2.0 /
// Apache-2.0) — fleischwolf doesn't train or modify the weights, just converts
// their format, and redistributes the export as a GitHub Release asset on this
// repo (see `MODELS_NOTICE.md` for the full attribution) at `DEFAULT_MODELS_URL`
// below. Override with `installDependencies({ modelsUrl })` /
// `FLEISCHWOLF_MODELS_URL` to use your own export/host instead, or set
// `DOCLING_LAYOUT_ONNX` etc. directly to a local file to skip downloading
// entirely.
//
// Everything is installed under a single home directory (default
// `~/.cache/fleischwolf`, overridable via `FLEISCHWOLF_HOME` or the `dir`
// option), and the corresponding `DOCLING_*` / `PDFIUM_DYNAMIC_LIB_PATH`
// environment variables are set in-process so the native pipeline finds them.

'use strict'

const fs = require('fs')
const os = require('os')
const path = require('path')
const http = require('http')
const https = require('https')
const { execFileSync } = require('child_process')

// Formats whose conversion requires the ML models + native libs above.
const ML_FORMATS = new Set(['pdf', 'image', 'mets_gbs'])

const PDFIUM_RELEASE =
  'https://github.com/bblanchon/pdfium-binaries/releases/latest/download'
const OCR_REC_URL =
  'https://huggingface.co/SWHL/RapidOCR/resolve/main/PP-OCRv3/ch_PP-OCRv3_rec_infer.onnx'
const OCR_DICT_URL =
  'https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/main/ppocr/utils/ppocr_keys_v1.txt'
// A fixed (non-"latest") tag: this repo also cuts a code release on every
// master push (see .github/workflows/ci.yml), so "latest" would almost always
// point at one of *those* instead of the model assets. Bump to models-v2 (and
// this constant) only when the export changes — see
// .github/workflows/publish-models.yml.
const DEFAULT_MODELS_URL =
  'https://github.com/artiz/fleischwolf/releases/download/models-v1'

// pdfium-binaries platform tag + shared-library filename, by (platform, arch).
function pdfiumPlatform() {
  const arch = process.arch === 'arm64' ? 'arm64' : process.arch === 'x64' ? 'x64' : process.arch
  switch (process.platform) {
    case 'linux':
      return { tag: `linux-${arch}`, lib: 'libpdfium.so' }
    case 'darwin':
      return { tag: `mac-${arch}`, lib: 'libpdfium.dylib' }
    case 'win32':
      return { tag: `win-${arch}`, lib: 'pdfium.dll' }
    default:
      throw new Error(`unsupported platform for pdfium: ${process.platform}/${process.arch}`)
  }
}

/** Resolve the install home directory (absolute). */
function homeDir(dir) {
  if (dir) return path.resolve(dir)
  if (process.env.FLEISCHWOLF_HOME) return path.resolve(process.env.FLEISCHWOLF_HOME)
  return path.join(os.homedir(), '.cache', 'fleischwolf')
}

/**
 * The resolved on-disk location of each dependency: an existing `DOCLING_*` /
 * `PDFIUM_DYNAMIC_LIB_PATH` environment variable wins (so a local Python export
 * is honored), else the path under the install home directory.
 */
function resolvePaths(dir) {
  const home = homeDir(dir)
  const models = path.join(home, 'models')
  const { lib } = pdfiumPlatform()

  const pdfiumLibDir = process.env.PDFIUM_DYNAMIC_LIB_PATH || path.join(home, 'pdfium', 'lib')
  return {
    home,
    models,
    pdfiumLibDir,
    pdfiumLib: path.join(pdfiumLibDir, lib),
    layout: process.env.DOCLING_LAYOUT_ONNX || path.join(models, 'layout_heron.onnx'),
    ocrRec: process.env.DOCLING_OCR_REC_ONNX || path.join(models, 'ocr_rec.onnx'),
    ocrDict: process.env.DOCLING_OCR_DICT || path.join(models, 'ppocr_keys_v1.txt'),
    tfEncoder:
      process.env.DOCLING_TABLEFORMER_ENCODER || path.join(models, 'tableformer', 'encoder.onnx'),
    tfDecoder:
      process.env.DOCLING_TABLEFORMER_DECODER || path.join(models, 'tableformer', 'decoder.onnx'),
    tfBbox: process.env.DOCLING_TABLEFORMER_BBOX || path.join(models, 'tableformer', 'bbox.onnx'),
  }
}

/**
 * Report which dependencies are present on disk, without downloading anything.
 * `ready` is true when the minimum for PDF (pdfium + layout) is present.
 */
function checkDependencies(options = {}) {
  const p = resolvePaths(options.dir)
  const has = (f) => fs.existsSync(f)
  const status = {
    home: p.home,
    pdfium: has(p.pdfiumLib),
    layout: has(p.layout),
    ocr: has(p.ocrRec) && has(p.ocrDict),
    tableformer: has(p.tfEncoder) && has(p.tfDecoder) && has(p.tfBbox),
  }
  status.ready = status.pdfium && status.layout
  status.missing = [
    !status.pdfium && 'pdfium',
    !status.layout && 'layout_heron.onnx',
  ].filter(Boolean)
  return status
}

/** Point the current process at installed assets (so the native pipeline finds them). */
function exportEnv(p) {
  if (fs.existsSync(p.pdfiumLib)) process.env.PDFIUM_DYNAMIC_LIB_PATH = p.pdfiumLibDir
  if (fs.existsSync(p.layout)) process.env.DOCLING_LAYOUT_ONNX = p.layout
  if (fs.existsSync(p.ocrRec)) process.env.DOCLING_OCR_REC_ONNX = p.ocrRec
  if (fs.existsSync(p.ocrDict)) process.env.DOCLING_OCR_DICT = p.ocrDict
  if (fs.existsSync(p.tfEncoder)) process.env.DOCLING_TABLEFORMER_ENCODER = p.tfEncoder
  if (fs.existsSync(p.tfDecoder)) process.env.DOCLING_TABLEFORMER_DECODER = p.tfDecoder
  if (fs.existsSync(p.tfBbox)) process.env.DOCLING_TABLEFORMER_BBOX = p.tfBbox
}

/**
 * A numbered, copy-pasteable walkthrough for getting the layout model (and,
 * optionally, TableFormer) in place, shown when the default hosted download
 * didn't succeed (or wasn't reachable) so both error sites below give the same
 * concrete next steps.
 */
function layoutSetupGuide() {
  return [
    'pdfium and the OCR model download automatically. The RT-DETR layout model',
    '(required) and TableFormer (optional — tables fall back to geometric',
    'reconstruction without it) normally do too, from a PyTorch→ONNX export',
    `fleischwolf hosts at ${DEFAULT_MODELS_URL} (docling-project's own models,`,
    'format-converted only — see MODELS_NOTICE.md for full attribution). That',
    "fetch didn't succeed here. Options:",
    '',
    '  1. Check connectivity to github.com and retry:',
    '       await installDependencies({ force: true })',
    '',
    '  2. Export the models yourself (needs Python + torch + transformers + onnx):',
    '       git clone https://github.com/artiz/fleischwolf && cd fleischwolf',
    '       pip install torch transformers onnx',
    '       python scripts/export_layout.py models/layout_heron.onnx',
    '       # optional — also needs docling_ibm_models + onnxscript + onnxruntime:',
    '       python scripts/export_tableformer.py models/tableformer',
    '     then point fleischwolf at the exported files, either directly:',
    '       export DOCLING_LAYOUT_ONNX=/path/to/layout_heron.onnx',
    '       export DOCLING_TABLEFORMER_ENCODER=/path/to/tableformer/encoder.onnx  # optional',
    '       export DOCLING_TABLEFORMER_DECODER=/path/to/tableformer/decoder.onnx  # optional',
    '       export DOCLING_TABLEFORMER_BBOX=/path/to/tableformer/bbox.onnx        # optional',
    '     or by copying them into installDependencies()’s install dir',
    '     (default ~/.cache/fleischwolf/models, or $FLEISCHWOLF_HOME/models) so',
    "     they're picked up as already installed on the next call.",
    '',
    '  3. Point at a different host (your own export, an internal mirror, …):',
    "       await installDependencies({ modelsUrl: 'https://your-host/models' })",
    '     (serving layout_heron.onnx and, optionally, tableformer-*.onnx), or set',
    '     FLEISCHWOLF_MODELS_URL to the same value.',
    '',
    'Declarative formats (md, html, docx, xlsx, …) need none of this — only PDF,',
    'image and METS conversion do.',
  ].join('\n')
}

/**
 * Throw a clear, actionable error if `format` needs the ML pipeline but its
 * dependencies aren't installed. Called before ML conversions.
 */
function assertMlReady(format, dir) {
  if (!ML_FORMATS.has(format)) return
  const status = checkDependencies({ dir })
  // Image needs layout (+OCR), but not pdfium; PDF/METS need both.
  const needPdfium = format !== 'image'
  const missing = [!status.layout && 'layout_heron.onnx', needPdfium && !status.pdfium && 'pdfium'].filter(
    Boolean,
  )
  if (missing.length === 0) return
  throw new Error(
    `Converting '${format}' requires the PDF/ML dependencies, which are not installed: ` +
      `${missing.join(', ')}.\n\n` +
      `First, call \`await installDependencies()\` — it fetches pdfium and the OCR model on ` +
      `its own. If it still can't get the layout model afterwards, see below.\n\n${layoutSetupGuide()}`,
  )
}

// --- downloading -----------------------------------------------------------

function download(url, dest, onProgress) {
  return new Promise((resolve, reject) => {
    const tmp = `${dest}.download`
    const client = url.startsWith('http://') ? http : https
    const req = client.get(url, { headers: { 'User-Agent': 'fleischwolf-node' } }, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        res.resume()
        return download(res.headers.location, dest, onProgress).then(resolve, reject)
      }
      if (res.statusCode !== 200) {
        res.resume()
        return reject(new Error(`GET ${url} → HTTP ${res.statusCode}`))
      }
      fs.mkdirSync(path.dirname(dest), { recursive: true })
      const out = fs.createWriteStream(tmp)
      res.pipe(out)
      out.on('finish', () => out.close(() => {
        fs.renameSync(tmp, dest)
        resolve(dest)
      }))
      out.on('error', reject)
    })
    req.on('error', reject)
  })
}

async function ensureFile(dest, url, force, onProgress, label) {
  if (!force && fs.existsSync(dest)) return false
  onProgress?.(`downloading ${label}`)
  await download(url, dest, onProgress)
  return true
}

/**
 * Fetch `<mainDest>.data` (ONNX's external-data sidecar) from `sidecarUrl` if
 * the host has one, ignoring a 404/any fetch error — most exports don't need
 * one (only a graph over ONNX's ~2GB protobuf limit does), so its absence is
 * expected, not a failure.
 */
async function ensureOptionalSidecar(mainDest, sidecarUrl, onProgress) {
  try {
    await ensureFile(`${mainDest}.data`, sidecarUrl, false, onProgress, `${path.basename(mainDest)}.data`)
  } catch {
    // No sidecar for this export — fine.
  }
}

async function installPdfium(p, force, onProgress) {
  if (!force && fs.existsSync(p.pdfiumLib)) return false
  if (process.env.PDFIUM_DYNAMIC_LIB_PATH) {
    // The user pointed us at a pdfium directory that doesn't contain the lib.
    throw new Error(
      `PDFIUM_DYNAMIC_LIB_PATH is set to '${p.pdfiumLibDir}' but no pdfium library was found there.`,
    )
  }
  const { tag } = pdfiumPlatform()
  const url = `${PDFIUM_RELEASE}/pdfium-${tag}.tgz`
  const home = p.home
  const pdfiumRoot = path.join(home, 'pdfium')
  fs.mkdirSync(pdfiumRoot, { recursive: true })
  const tgz = path.join(pdfiumRoot, 'pdfium.tgz')
  onProgress?.(`downloading pdfium (${tag})`)
  await download(url, tgz)
  onProgress?.('extracting pdfium')
  // pdfium-binaries ships a .tgz; use the system `tar` (present on Linux, macOS,
  // and Windows 10+). The archive lays out lib/<libpdfium> which matches pdfiumLibDir.
  execFileSync('tar', ['-xzf', tgz, '-C', pdfiumRoot])
  fs.rmSync(tgz, { force: true })
  if (!fs.existsSync(p.pdfiumLib)) {
    throw new Error(`pdfium extracted but ${p.pdfiumLib} is missing (unexpected archive layout)`)
  }
  return true
}

/**
 * Download and install everything the PDF/image pipeline needs, then point the
 * process at it. Idempotent: skips assets already present (pass `{ force: true }`
 * to re-download). Returns a status report.
 *
 * @param {object} [options]
 * @param {string} [options.dir]         install home (default ~/.cache/fleischwolf or $FLEISCHWOLF_HOME)
 * @param {string} [options.modelsUrl]   base URL serving layout_heron.onnx + tableformer-*.onnx
 *                                       (default: fleischwolf's own hosted export, DEFAULT_MODELS_URL)
 * @param {boolean} [options.ocr=true]   also fetch the OCR model + dictionary
 * @param {boolean} [options.tableformer=true] also fetch TableFormer from modelsUrl
 * @param {boolean} [options.force=false] re-download assets that already exist
 * @param {(msg: string) => void} [options.onProgress]
 */
async function installDependencies(options = {}) {
  const p = resolvePaths(options.dir)
  const onProgress = options.onProgress
  const installed = []
  const missing = []
  fs.mkdirSync(p.models, { recursive: true })

  // 1. pdfium (required for PDF).
  if (await installPdfium(p, options.force, onProgress)) installed.push('pdfium')

  // 2. OCR recognition model + dictionary (for pages without a text layer).
  if (options.ocr !== false) {
    if (await ensureFile(p.ocrRec, OCR_REC_URL, options.force, onProgress, 'OCR model'))
      installed.push('ocr_rec.onnx')
    if (await ensureFile(p.ocrDict, OCR_DICT_URL, options.force, onProgress, 'OCR dictionary'))
      installed.push('ppocr_keys_v1.txt')
  }

  // 3. Layout (required) + TableFormer (optional) — from the configured base URL,
  // defaulting to fleischwolf's own hosted export (DEFAULT_MODELS_URL) so this
  // works with zero configuration; pass `{ modelsUrl }` / set
  // `FLEISCHWOLF_MODELS_URL` to use your own export/host instead.
  const base = (options.modelsUrl || process.env.FLEISCHWOLF_MODELS_URL || DEFAULT_MODELS_URL).replace(
    /\/$/,
    '',
  )
  if (!fs.existsSync(p.layout)) {
    try {
      if (
        await ensureFile(p.layout, `${base}/layout_heron.onnx`, options.force, onProgress, 'layout model')
      ) {
        installed.push('layout_heron.onnx')
        // Large exports carry their weights in a sidecar `<file>.onnx.data`
        // (ONNX's external-data format, used above the ~2GB protobuf limit).
        // Optional — most exports don't need one — so a missing sidecar is not
        // an error.
        await ensureOptionalSidecar(p.layout, `${base}/layout_heron.onnx.data`, onProgress)
      }
    } catch (e) {
      // Surfaced below via status.missing + layoutSetupGuide(), with more
      // actionable detail than the raw fetch error.
      onProgress?.(`could not fetch layout model from ${base}: ${e.message}`)
    }
  }
  if (options.tableformer !== false) {
    for (const [file, dest] of [
      ['tableformer/encoder.onnx', p.tfEncoder],
      ['tableformer/decoder.onnx', p.tfDecoder],
      ['tableformer/bbox.onnx', p.tfBbox],
    ]) {
      // GitHub Release assets can't contain "/", so the hosted copy is flat
      // (tableformer-encoder.onnx, …); a custom `modelsUrl` host is free to
      // mirror either layout, since ensureOptionalSidecar/ensureFile below try
      // the flat name.
      const flat = file.replace(/\//g, '-')
      try {
        if (await ensureFile(dest, `${base}/${flat}`, options.force, onProgress, file)) {
          installed.push(file)
          await ensureOptionalSidecar(dest, `${base}/${flat}.data`, onProgress)
        }
      } catch (e) {
        // TableFormer is optional (geometric fallback); note but don't fail.
        onProgress?.(`skipped ${file}: ${e.message}`)
      }
    }
  }

  exportEnv(p)
  const status = checkDependencies(options)

  if (!status.ready) {
    throw new Error(
      `installDependencies: PDF conversion is not ready. Missing: ${status.missing.join(', ')}.\n\n` +
        `layout_heron.onnx could not be fetched from ${base} — check the URL is reachable and\n` +
        `serves layout_heron.onnx (and, optionally, tableformer-*.onnx) at that path.\n\n${layoutSetupGuide()}`,
    )
  }

  return { ...status, installed, missing }
}

module.exports = {
  ML_FORMATS,
  installDependencies,
  checkDependencies,
  assertMlReady,
  resolvePaths,
  exportEnv,
}
