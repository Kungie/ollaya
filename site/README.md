# Ollaya website

The website for Ollaya: home page, model library, docs, download page, `/install.sh` and the static
model registry.
It is a **fully static site**. Pages are written as Hono JSX components, pre-rendered to HTML at
build time, and styled with Tailwind CSS v4. A small dependency-free script
(`public/static/app.js`) adds search, the navbar typeahead, tabs and copy buttons. There is no
server code and no Worker script, so hosting costs nothing.

## Develop

Requires Node.js 22 or newer.

```sh
npm ci
npm run build      # → dist/
npm run dev        # build, then serve dist/ with wrangler dev on http://localhost:8787
```

`wrangler dev` does not rebuild on its own. Run `npm run build` again after editing anything.

| Script | What it does |
|---|---|
| `npm run build` | Generate content and the catalog, typecheck, pre-render everything into `dist/`, run checks |
| `npm run gen` | Markdown → `src/generated/content.ts`; `../registry` → `src/generated/registry.ts` |
| `npm run typecheck` | `gen`, then `tsc --noEmit` |
| `npm run dev` | `build` + `wrangler dev` (assets only, same routing as production) |
| `npm run deploy` | `build` + `wrangler deploy` |

The build fails if:
- a host name is hard-coded in `src/`, `docs/`, `content/` or `public/`;
- an internal link in `dist/` points to a missing file;
- a page still says "coming soon", "pre-release", "illustrative output" or "being built";
- `dist/install.sh` is not byte-for-byte `../scripts/install.sh`;
- a registry manifest's blob URL is not under `SITE_ORIGIN`, or a blob it points to is missing
  from `../registry/blobs` (`node scripts/fetch-blobs.mjs` downloads them);
- a tag's manifest has no copy by digest (`manifests/sha256-<hex>`; `node scripts/archive-manifests.mjs`
  writes it), or such a copy does not hash to its name.

## Layout

```
wrangler.jsonc          Assets-only Worker: ./dist, 404 page, custom domain route
scripts/gen-content.mjs Markdown (docs/, content/) → src/generated/content.ts (git-ignored)
scripts/gen-registry.mjs ../registry manifests + config blobs → src/generated/registry.ts (git-ignored)
scripts/build.mjs       dist/: public/, Tailwind, asset hashes, pages, install.sh, registry copy
scripts/check.mjs       Post-build checks on dist/
scripts/fetch-blobs.mjs Downloads the derived blobs the manifests point to into ../registry/blobs
scripts/archive-manifests.mjs Keeps each tag's manifest by digest in ../registry (ADR 0006)
src/build.tsx           Page list and URL → file mapping; renders every page, 404, sitemap, …
src/pages/              Page components: home, search, library, download, docs, 404, text files
src/components/         Layout, header, footer, logo, icons, code blocks, tabs, badges
src/data/catalog.ts     Builds the catalog from the registry, plus optional hand-written overlays
src/data/examples.ts    Usage snippets (CLI / cURL / Python / JavaScript)
src/styles/app.css      Tailwind input: design tokens (light + dark) and prose styles
content/library/*.md    Model readmes (optional)
docs/*.md               Docs pages with front matter (title, nav, description, order)
public/                 Copied into dist/ as is: favicon, _headers, _redirects, static/app.js, images
```

`dist/` after a build:

```
index.html  search.html  download.html  docs.html  docs/<page>.html  404.html
library/laya.html  library/laya/tags.html  library/laya:<tag>.html  library/laya/tags/<tag>.html
library.html (meta refresh to /search)  search.json  install.sh  robots.txt  sitemap.xml
favicon.svg  static/{app.css, app.js, logo.svg, og.png}  _headers  _redirects
v2/<namespace>/<model>/manifests/<tag>  blobs/sha256-<hex>     (copied from ../registry)
v2/<namespace>/<model>/manifests/sha256-<hex>   every manifest ever published under a tag, by digest
```

URLs have no `.html` suffix: `/docs/api` is served from `docs/api.html`.

### Search

`/search` lists every model in the HTML, so it works without JavaScript. `app.js` filters and
sorts the list in place and keeps `?q=&c=&o=` in the URL. The navbar typeahead loads
`/search.json`, which is generated from `src/data/catalog.ts`.

### The catalog

The registry is the single source of truth. `scripts/gen-registry.mjs` reads every manifest under
`../registry/v2/<namespace>/<model>/manifests/` and the config blob each one references
(`model_format`, `family`, `parameter_size`, `context_length`, `languages`, `description`, `source`,
`license`, `release_date`), plus the small router, params, decision and calibration blobs. Models in
the `library` namespace become pages: sizes, context lengths, languages, layer digests, routes and
precision variants all come from there.

To add a model family: run `convert/ollaya_convert/package.py`, then `npm run build`. Optionally
add a readme in `content/library/<model>.md`, and an overlay in `src/data/catalog.ts` for the
title, description, publisher, capability badges, search keywords, popularity rank and per-tag
summaries. Without them the model still gets a page from its config. Never add pull counts or
other numbers we do not have. `OLLAYA_REGISTRY_DIR` points the build at another registry.

## Hosting

The site is built for **Cloudflare Workers static assets**, with no `main` script. That means
`wrangler.jsonc` has only an `assets` block (`not_found_handling: "404-page"`,
`html_handling: "auto-trailing-slash"`), so no request runs any code.

`dist/` also works **unchanged on any static host**, for example GitHub Pages, Netlify or
`npx serve dist`. Two files are Cloudflare extras that other hosts ignore:

- `_headers`: security headers, `Content-Type` for `install.sh`, long cache for `/static/*`
  (files there are referenced with a `?v=<content hash>` query).
- `_redirects`:
  - a 302 from `/library` to `/search`. Other hosts serve `library.html`, which forwards with a
    meta refresh.
  - a 200 rewrite from `/library/<model>:<tag>` to `library/<model>/tags/<tag>.html`. Cloudflare's
    asset server would otherwise percent-encode the colon (307 to `/library/laya%3Aen`). Other
    hosts serve `library/laya:en.html` directly.
  - a 200 rewrite from `/v2/<namespace>/<model>/manifests/sha256:<hex>` to the file
    `sha256-<hex>`: the manifests `ollaya pull <name>@sha256:<hex>` fetches
    ([ADR 0006](../docs/decisions/0006-manifest-retention.md)). Other hosts need the same rewrite
    for pulls by digest; pulls by tag work without it.

`python3 -m http.server` works for spot checks, but it has no clean URLs (open `/search.html`).

## Domain

The public origin is set **at build time** with the `SITE_ORIGIN` environment variable
(default `https://ollaya.dev`). It is used for canonical and OpenGraph URLs, the install
command, `robots.txt` and `sitemap.xml`; docs use the `{{SITE_ORIGIN}}` placeholder. No host name
is hard-coded anywhere else.

The same Worker also answers on `www.ollaya.dev` and on `ollaya.cobanov.dev`, the site's first
host. Keep `ollaya.cobanov.dev`: Ollaya 0.3.1 and older use it as their default registry, and it
serves the same manifests, whose blob URLs point at `ollaya.dev`. Pages there carry a canonical
link to `ollaya.dev`.

## Static registry (`/v2/` and `/blobs/`)

The site also hosts Ollaya's model registry. `convert/ollaya_convert/package.py` writes
`../registry/v2/<ns>/<model>/manifests/<tag>` and the derived blobs (configs, ONNX graphs, decision
and calibration files) to `../registry/blobs/`; the build copies both into `dist/`. Weights and
tokenizers are referenced by URL inside the manifests: they download from the model authors'
Hugging Face repositories at a pinned commit, and are never re-hosted.

Manifests carry absolute blob URLs, so the build checks that they point at `SITE_ORIGIN`; package
the registry for the origin you deploy to. Pages never write under `/v2/` or `/blobs/`, and
`robots.txt` keeps crawlers out of both.

**Old versions stay.** `package.py` also writes each tag's manifest as
`manifests/sha256-<hex>`, named by its sha256, and those files are never deleted. They are what
`ollaya pull <name>@sha256:<hex>` fetches after the tag has moved, so every deploy has to carry
their blobs forward: run `node scripts/fetch-blobs.mjs` before deploying from a checkout that may
lack some (it downloads them from the live site), and never deploy with
`OLLAYA_ALLOW_MISSING_BLOBS=1`. See [ADR 0006](../docs/decisions/0006-manifest-retention.md).

## Installer

`/install.sh` is `../scripts/install.sh`, copied unchanged at build time (the check compares the
bytes). It detects the OS, CPU and NVIDIA GPU, downloads the release from
`github.com/ollaya-dev/ollaya` releases, verifies the sha256 and sets up the systemd service on
Linux. Edit the script there, never in `site/`.

## Deploy

```sh
npx wrangler login     # or: export CLOUDFLARE_API_TOKEN=... ("Edit Cloudflare Workers" token template)
node scripts/fetch-blobs.mjs   # every blob any kept manifest needs, from the live site
npm run deploy         # optionally: SITE_ORIGIN=https://… npm run deploy (registry packaged for it)
```

This is an assets-only Worker, which is free: static asset requests are not billed and there is no
script to invoke. Each custom domain requires its zone (`ollaya.dev`, `cobanov.dev`) in the same
Cloudflare account, with no existing DNS record for that host; Wrangler creates the DNS record and
certificate on the first deploy. The `*.workers.dev` URL also stays available (`workers_dev: true`).
