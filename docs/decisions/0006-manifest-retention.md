# 0006: Every published manifest stays fetchable by digest

- Status: proposed, 2026-10-13 (issue #64)
- Applies to: the static registry on ollaya.dev (`registry/`, `site/`), `convert/ollaya_convert/package.py`,
  and model names in the client (`ollaya pull`, `/api/pull`).

## Context

A tag moves when a model is repackaged: new graphs, a new calibration, a new upstream commit.
Tools that pin a model to an exact version record the manifest's sha256 (the `digest` that
`/api/tags` and `/api/ps` report) and need to fetch that version again later, for example to
rebuild a lost volume. Until now that was impossible:

- **Only tags were served.** `GET /v2/<ns>/<model>/manifests/sha256:<hex>` got a 307 to
  `sha256%3A<hex>` from Cloudflare's asset server, then the HTML 404 page.
- **Old blobs were not kept on purpose.** The manifests are in git, but the derived blobs they
  reference on ollaya.dev (ONNX graphs, config, decision, calibration, arch) are build outputs,
  gitignored. A deploy publishes what is in the deployer's `registry/blobs`, and nothing checks
  that an old manifest's blobs are still there. Weights are fine either way: they stay on the
  author's Hugging Face repository, pinned to a commit.
- **The client had no digest syntax.** `laya:en@sha256:<hex>` was an invalid name.

Constraints: the site is static assets with no Worker script (`site/README.md`), and Workers static
assets allow 20,000 files per version on the free plan (100,000 on paid), 25 MiB per file, 100
`_headers` rules and 100 dynamic `_redirects` rules. A file name can't contain a colon on Windows,
where CI checks out the repository.

## Options

1. **Manifests from git history at build time.** The build walks `git log registry/v2` and emits
   every old version. Rejected: it needs the full history in the deploy checkout, and which
   versions it emits would depend on which blobs the deployer happens to have, so two builds of
   one commit could differ.
2. **R2 (or Hugging Face `ollaya-dev/*`) as an archive.** A bucket holds old manifests and blobs.
   Rejected for now: a second store to keep in sync, and serving it from `ollaya.dev/v2/` needs a
   Worker script or a separate host. Static assets are enough at today's size (below).
3. **Committed copies by digest (chosen).** Every manifest written under a tag is also written as
   `manifests/sha256-<hex>`, committed and never deleted. One `_redirects` rule serves it at
   `sha256:<hex>`. The build already refuses to deploy a manifest whose blobs are missing, and now
   that covers the kept ones too.

## Decision

**Registry layout.** Next to each tag, `registry/v2/<ns>/<model>/manifests/sha256-<hex>` holds a
manifest byte for byte, named by its sha256. `package.py` writes it with every tag (the same bytes,
written once), and refuses a tag that starts with `sha256-`. The files are append-only: retention
is exactly the set of `sha256-*` files in git, reviewable in a diff. `site/scripts/archive-manifests.mjs`
adds the copy for every current tag, and with `--history` for the older versions in git history
whose blobs can still be fetched (it downloads and verifies them into `registry/blobs`).

**Serving.** `site/public/_redirects` rewrites, with status 200:

```
/v2/:namespace/:model/manifests/sha256:* /v2/:namespace/:model/manifests/sha256-:splat 200
```

- **Same response as a tag.** The body, and the headers static assets give an extensionless file:
  no `Content-Type`, `Cache-Control: public, max-age=0, must-revalidate`, an `ETag`. There is no
  `Docker-Content-Digest` header (that would take one `_headers` rule per manifest); the client
  hashes the body instead.
- **Unknown digest.** The site's 404 page (`text/html`, status 404), as for an unknown tag.
- **Scope.** A digest is served under the model it was published for; under another model it is a 404.
- **Also served as a tag.** `sha256-<hex>` is a plain file, so `.../manifests/sha256-<hex>` serves
  it too. That is why the client treats the tag `sha256-<hex>` as that digest (below).
- **Percent-encoded colon.** `sha256%3A<hex>` is not rewritten (404). Clients send the colon as is.

Checked with `wrangler dev` (the same asset server): tag and digest URLs return the same bytes with
the same headers, an older `laya:en` comes back by its digest, and an unknown digest is the 404 page.

**Deploy invariant.** `npm run build` (`check.mjs`) fails when a tag has no copy by digest or a
`sha256-*` file does not hash to its name, and `build.mjs` already fails when any blob that any
manifest points to is missing from `registry/blobs` (unless `OLLAYA_ALLOW_MISSING_BLOBS=1`, which
only CI sets, and CI never deploys). `scripts/fetch-blobs.mjs` downloads every referenced blob from
the live site, so each deploy carries forward the blobs of every kept manifest from the one before.
Blobs are never removed by anything but a deploy without them, which the build refuses.

**Client.** Model names are `[host/][namespace/]model[:tag][@sha256:<hex>]`
(`crates/ollaya-registry/src/name.rs`):

- `name:tag@sha256:<hex>` pulls `/manifests/sha256:<hex>` and stores it under `tag`, whatever the
  registry's `tag` points to now. The tag is not checked against the registry; OCI clients ignore it
  for the same reason (a static registry has no index of which tags a digest was published under).
- `name@sha256:<hex>` stores it under the tag `sha256-<hex>`; the name lists as
  `name@sha256:<hex>`, and `name:sha256-<hex>` is the same name.
- **Verified before anything is stored.** The manifest's sha256 must equal the digest before its JSON
  is parsed or any blob is downloaded; otherwise `502 DIGEST_MISMATCH` and nothing changes. A 404 is
  `404 MODEL_NOT_FOUND` with its own message (`... it has no manifest with that digest`); the HTML
  page is never read as a manifest.
- **Reads.** Elsewhere (`/api/decide`, `/api/show`, `/api/delete`, `/api/copy` `source`, `/api/create`
  `from`) a name with a digest finds its tag only while the tag holds that manifest, else `404`. So
  `ollaya run laya:en@sha256:<hex>` pulls when `laya:en` is another version.
- **Writes.** `/api/create` `model` and `/api/copy` `destination` refuse a digest (`422`).
- **Routers.** A digest pins the router manifest, not its targets: they are pulled by tag, as before.

## Consequences

- **Retention from now on**, for every manifest published under a tag, as long as its derived blobs
  are carried forward. The backfill with `--history` keeps 17 older versions in git history whose
  blobs ollaya.dev still serves: earlier builds of the nine `laya` tags, `nli:modernbert-large` and
  `kev:0.8b`. 15 older ones are left
  out: their blob URLs are on `ollaya.cobanov.dev` (before the move to ollaya.dev), which the build
  doesn't accept for this origin.
- **Cost today** (2026-10-13): 65 tags are 46 distinct manifests; with the 17 older ones, 63 files
  of 170 KiB in git. Their blobs are 174 files, 204 MiB, of which 9 files and 20 MiB serve only the
  older manifests; the largest blob is 13.6 MiB. The built site is 522 files. Each later tag move adds
  one manifest file plus the new derived blobs, which a deploy publishes anyway; what retention adds
  is that the old blobs stay. At a few dozen moves a month that is hundreds of files a year against
  the 20,000-file limit; bytes are not limited.
- **The live site is the only full copy of old blobs** besides the deployer's `registry/blobs`.
  Losing both loses the old manifests' graphs, which are not byte-reproducible in general (ONNX
  exports). A copy of `registry/blobs` somewhere durable (an R2 bucket, a release asset) would remove
  that risk; it is not part of this decision.
- **Clients older than this change** keep working: tags are unchanged, and the `sha256-*` files are
  invisible to them (the site's catalog skips them).
