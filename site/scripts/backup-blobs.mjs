// Copies the derived registry blobs in ../registry/blobs to an R2 bucket, so the blobs of every
// manifest kept by digest (ADR 0006) have a copy besides the live site and the deployer's disk.
// Nothing serves the bucket; it is the copy to restore from. Run it after each deploy:
//
//   node scripts/backup-blobs.mjs <bucket>              # e.g. ollaya-registry-blobs
//   node scripts/backup-blobs.mjs <bucket> --dry-run    # list what it would upload
//
// Each blob is checked against its name before upload and stored as blobs/sha256-<hex>. Blobs are
// named by their sha256 and never change, so the bucket's INDEX object lists the ones it already
// holds and only the others are sent; --all sends every blob again. --local uses Wrangler's local
// storage instead of R2 (for testing). Needs `wrangler login` or CLOUDFLARE_API_TOKEN with R2
// access, and the bucket, created once with `npx wrangler r2 bucket create <bucket>`.
// To restore one: npx wrangler r2 object get <bucket>/blobs/sha256-<hex> --remote --file ../registry/blobs/sha256-<hex>

import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdtemp, readdir, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'

const args = process.argv.slice(2)
const flags = new Set(args.filter((a) => a.startsWith('--')))
const [bucket] = args.filter((a) => !a.startsWith('--'))
const unknown = [...flags].filter((f) => !['--dry-run', '--all', '--local'].includes(f))
if (!bucket || unknown.length) {
  console.error('usage: node scripts/backup-blobs.mjs <bucket> [--dry-run] [--all] [--local]')
  process.exit(2)
}
const where = flags.has('--local') ? '--local' : '--remote'
const registry = process.env.OLLAYA_REGISTRY_DIR || fileURLToPath(new URL('../../registry', import.meta.url))
const blobs = join(registry, 'blobs')
const wranglerBin = fileURLToPath(new URL('../node_modules/wrangler/bin/wrangler.js', import.meta.url))

function wrangler(...rest) {
  const r = spawnSync(process.execPath, [wranglerBin, 'r2', 'object', ...rest, where], {
    encoding: 'utf8',
    maxBuffer: 64 << 20,
  })
  return { ok: r.status === 0, out: r.stdout ?? '', err: `${r.stderr ?? ''}${r.error ?? ''}` }
}

const names = (await readdir(blobs)).filter((n) => /^sha256-[0-9a-f]{64}$/.test(n)).sort()
for (const name of names) {
  const got = createHash('sha256').update(await readFile(join(blobs, name))).digest('hex')
  if (`sha256-${got}` !== name) throw new Error(`${join(blobs, name)} hashes to sha256-${got}`)
}

let held = new Set()
if (!flags.has('--all')) {
  const index = wrangler('get', `${bucket}/INDEX`, '--pipe')
  if (index.ok) held = new Set(index.out.split('\n').filter((l) => /^sha256-[0-9a-f]{64}$/.test(l)))
  else if (!/does not exist|NoSuchKey|not found/i.test(index.err)) throw new Error(`reading ${bucket}/INDEX: ${index.err.trim()}`)
}
const missing = names.filter((n) => !held.has(n))
let bytes = 0
for (const name of missing) bytes += (await readFile(join(blobs, name))).length
const mib = (bytes / 2 ** 20).toFixed(1)
if (flags.has('--dry-run')) {
  for (const name of missing) console.log(`would upload blobs/${name}`)
  console.log(`backup: ${names.length} blobs in ${blobs}, ${held.size} already in ${bucket}, ${missing.length} to upload (${mib} MiB)`)
  process.exit(0)
}

let uploaded = 0
for (const name of missing) {
  const put = wrangler('put', `${bucket}/blobs/${name}`, '--file', join(blobs, name), '--content-type', 'application/octet-stream')
  if (!put.ok) throw new Error(`uploading ${name} (${uploaded} of ${missing.length} done): ${put.err.trim()}`)
  held.add(name)
  uploaded++
}
const dir = await mkdtemp(join(tmpdir(), 'ollaya-backup-'))
try {
  await writeFile(join(dir, 'INDEX'), [...held].sort().join('\n') + '\n')
  const put = wrangler('put', `${bucket}/INDEX`, '--file', join(dir, 'INDEX'), '--content-type', 'text/plain')
  if (!put.ok) throw new Error(`writing ${bucket}/INDEX: ${put.err.trim()}`)
} finally {
  await rm(dir, { recursive: true, force: true })
}
console.log(`backup: ${names.length} blobs in ${blobs}, ${uploaded} uploaded to ${bucket} (${mib} MiB), INDEX lists ${held.size}`)
