// Keeps every manifest published under a tag fetchable by its digest (docs/decisions/0006-manifest-retention.md):
//
//   registry/v2/<namespace>/<model>/manifests/sha256-<hex>   a tag's manifest, byte for byte, named by its sha256
//
// The site serves it at /v2/<namespace>/<model>/manifests/sha256:<hex> (public/_redirects), which is
// what `ollaya pull <name>@sha256:<hex>` fetches. These files are never deleted. package.py writes
// one with every tag; build.mjs refuses a tag without one, and a deploy without every blob a kept
// manifest references.
//
//   node scripts/archive-manifests.mjs                     keep every current tag's manifest
//   node scripts/archive-manifests.mjs --history           also keep the older ones in git history
//                                                          whose blobs on SITE_ORIGIN still exist
//   node scripts/archive-manifests.mjs --history --dry-run only say what it would do
//
// With --history, each older manifest's derived blobs (those under SITE_ORIGIN/blobs/) are taken
// from ../registry/blobs or downloaded from the live site and checked against their sha256 (saved
// into ../registry/blobs, as fetch-blobs.mjs does). A manifest with any blob missing, or with
// blobs on another host, is left out: it can't be served whole. Weights are not checked: they
// stay on their authors' Hugging Face repositories, pinned to a commit.

import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdir, readdir, readFile, rename, stat, writeFile } from 'node:fs/promises'
import { join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const registry = resolve(process.env.OLLAYA_REGISTRY_DIR || fileURLToPath(new URL('../../registry', import.meta.url)))
const origin = new URL(process.env.SITE_ORIGIN?.trim() || 'https://ollaya.dev').origin
const history = process.argv.includes('--history')
const dryRun = process.argv.includes('--dry-run')
const DIGEST_FILE = /^sha256-[0-9a-f]{64}$/

const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex')
const exists = (p) => stat(p).then(() => true, () => false)

async function dirs(p) {
  return (await readdir(p, { withFileTypes: true }).catch(() => [])).filter((e) => e.isDirectory()).map((e) => e.name).sort()
}

// The manifests directories: [namespace, model, path].
async function repos() {
  const out = []
  for (const ns of await dirs(join(registry, 'v2'))) {
    for (const model of await dirs(join(registry, 'v2', ns))) out.push([ns, model, join(registry, 'v2', ns, model, 'manifests')])
  }
  return out
}

let kept = 0
const seen = new Set() // directories and digests already kept, or about to be (--dry-run)
async function keep(dir, bytes, what) {
  const path = join(dir, `sha256-${sha256(bytes)}`)
  if (seen.has(path)) return false
  seen.add(path)
  const current = await readFile(path).catch(() => null)
  if (current) {
    if (!current.equals(bytes)) throw new Error(`${path} is not the manifest its name says`)
    return false
  }
  console.log(`${dryRun ? 'would keep' : 'keep'} ${what}`)
  if (!dryRun) {
    await mkdir(dir, { recursive: true })
    await writeFile(`${path}.tmp`, bytes)
    await rename(`${path}.tmp`, path)
  }
  kept++
  return true
}

// 1. Every current tag.
for (const [ns, model, dir] of await repos()) {
  for (const e of await readdir(dir, { withFileTypes: true })) {
    if (!e.isFile() || DIGEST_FILE.test(e.name) || e.name.endsWith('.tmp')) continue
    const bytes = await readFile(join(dir, e.name))
    await keep(dir, bytes, `${ns}/${model}:${e.name} as sha256:${sha256(bytes)}`)
  }
}
const current = kept

// 2. Older versions from git history, when their blobs are still there.
const skipped = []
if (history) {
  const git = (...args) => {
    const r = spawnSync('git', ['-C', registry, ...args], { encoding: 'buffer', maxBuffer: 1 << 30 })
    if (r.status !== 0) throw new Error(`git ${args.join(' ')}: ${r.stderr.toString()}`)
    return r.stdout
  }
  const versions = new Map() // git object id -> { ns, model, tags: Set, first: commit }
  const commits = git('log', '--format=%H', '--', 'v2').toString().split('\n').filter(Boolean)
  for (const commit of commits) {
    for (const line of git('ls-tree', '-r', commit, '--', 'v2').toString().split('\n')) {
      const m = /^\d+ blob ([0-9a-f]+)\t(?:.*\/)?v2\/([^/]+)\/([^/]+)\/manifests\/([^/]+)$/.exec(line)
      if (!m || DIGEST_FILE.test(m[4])) continue
      const [, oid, ns, model, tag] = m
      const v = versions.get(oid) ?? { ns, model, tags: new Set() }
      v.tags.add(tag)
      versions.set(oid, v)
    }
  }
  const blobs = join(registry, 'blobs')
  const available = new Map() // blob file name -> true / reason
  async function blobAvailable(name, url, digest) {
    if (available.has(name)) return available.get(name)
    let result = true
    const path = join(blobs, name)
    const local = await readFile(path).catch(() => null)
    if (local) {
      if (`sha256:${sha256(local)}` !== digest) result = `registry/blobs/${name} does not match ${digest}`
    } else {
      const res = await fetch(url).catch((e) => ({ ok: false, status: e.message }))
      if (!res.ok) result = `${url}: HTTP ${res.status}`
      else {
        const body = Buffer.from(await res.arrayBuffer())
        const got = `sha256:${sha256(body)}`
        if (got !== digest) result = `${url} is ${got}, not ${digest}`
        else if (!dryRun) {
          await mkdir(blobs, { recursive: true })
          await writeFile(`${path}.tmp`, body)
          await rename(`${path}.tmp`, path)
        }
      }
    }
    available.set(name, result)
    return result
  }
  for (const [oid, { ns, model, tags }] of versions) {
    const bytes = git('cat-file', 'blob', oid)
    const dir = join(registry, 'v2', ns, model, 'manifests')
    const what = `${ns}/${model}:${[...tags].sort().join(',')} sha256:${sha256(bytes)}`
    const path = join(dir, `sha256-${sha256(bytes)}`)
    if (seen.has(path) || (await exists(path))) continue
    let manifest
    try {
      manifest = JSON.parse(bytes.toString('utf8'))
    } catch (e) {
      skipped.push(`${what}: not JSON (${e.message})`)
      continue
    }
    let reason
    layers: for (const layer of [manifest.config, ...(manifest.layers ?? [])]) {
      for (const url of layer.urls ?? []) {
        if (!url.includes('/blobs/sha256-')) continue
        const ok = url.startsWith(`${origin}/blobs/`)
          ? await blobAvailable(url.slice(`${origin}/blobs/`.length), url, layer.digest)
          : `a blob on another host, ${url}`
        if (ok !== true) {
          reason = ok
          break layers
        }
      }
    }
    if (reason) skipped.push(`${what}: ${reason}`)
    else await keep(dir, bytes, `${what} (from git history)`)
  }
}

console.log(
  `manifests by digest: ${kept} ${dryRun ? 'to add' : 'added'} (${current} current, ${kept - current} older)` +
    (skipped.length ? `; ${skipped.length} older left out:\n  ${skipped.join('\n  ')}` : ''),
)
