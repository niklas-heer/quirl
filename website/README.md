# Quirl website

The Quirl landing page and complete documentation site are built with Next.js
and [Fumadocs](https://www.fumadocs.dev/). Public copy describes the supported
0.1.0 release while keeping untagged source work, historical evidence, and
future product direction clearly separated. The documentation mirror is
designed for frequent updates.

## Develop locally

```sh
npm ci
npm run dev
```

Open [http://localhost:3000](http://localhost:3000). The documentation starts at
`/docs`, and local search is served by the Fumadocs search route.

## Content model

- `app/(home)/page.tsx` owns the product landing page.
- `content/docs/` contains curated entry pages and generated mirrors.
- `scripts/sync-docs.mjs` maps canonical repository documents into the
  Fumadocs information architecture.
- `public/reference/` contains the checked-in LuaLS and protocol projections.
- `lib/source.ts` loads the content tree, search text, and page metadata.

Repository Markdown, `docs/quirl.lua`, and `docs/protocol-freeze-v1.json` remain
authoritative. Do not edit generated MDX pages directly. Refresh them with:

```sh
npm run sync:docs
npm run sync:reference
```

Both `npm run dev` and `npm run build` run this sync automatically. The sync
adds frontmatter, rewrites internal Markdown links to website routes, marks
the release benchmark from its canonical structured evidence status, and
substitutes plain-text highlighting for Quirl code until a Shiki grammar is
available. `sync:reference` is intentionally manual because it compiles Quirl,
then regenerates the CLI catalog and Lua API pages from the installed Rust
definitions.

`npm run check:generated` is non-mutating: it renders both mirror classes in
memory and fails if any tracked output would change. Use it in reviews and
release gates; use the two sync commands only when intentionally updating
canonical-source projections.

## Validate

```sh
npm ci
npm run check
```

`npm run check` verifies generated-mirror freshness, semantic release-evidence
attribution, lint, route type checking, and a production build without rewriting
tracked files. It uses the exact dependency graph in `package-lock.json`; do not
substitute `npm install` for release validation.

Set `NEXT_PUBLIC_SITE_URL` to the canonical production origin in deployment.
Without it, local builds deliberately use `http://localhost:3000` as their
metadata origin.

## Updating release status

For every public release:

1. Refresh canonical project documentation and run `npm run sync:docs`.
2. Update release, installation, support, and historical-evidence wording only
   after the immutable release exists.
3. Confirm `NEXT_PUBLIC_SITE_URL` in the deployment environment.
4. Review the landing-page terminal example against the released binary.
5. Validate search, social cards, mobile navigation, and representative long
   architecture/reference pages.
