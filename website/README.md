# Quirl website

The Quirl landing page and complete documentation site are built with Next.js
and [Fumadocs](https://www.fumadocs.dev/). The site is intentionally ready ahead
of the first tagged release: public copy is honest about the 0.1 candidate, and
the documentation mirror is designed for frequent updates.

## Develop locally

```sh
npm install
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
historical benchmark pages, and substitutes plain-text highlighting for Quirl
code until a Shiki grammar is available. `sync:reference` is intentionally
manual because it compiles Quirl, then regenerates the CLI catalog and Lua API
pages from the installed Rust definitions.

## Validate

```sh
npm run lint
npm run types:check
npm run build
```

Set `NEXT_PUBLIC_SITE_URL` to the canonical production origin when deployment is
configured. Until then, metadata uses the local development origin and makes no
claim about an unpublished domain.

## Updating toward release

Before the first public release:

1. Refresh canonical project documentation and run `npm run sync:docs`.
2. Replace remaining prerelease framing only after the release checklist passes.
3. Set `NEXT_PUBLIC_SITE_URL` in the deployment environment.
4. Review the landing-page terminal example against the candidate binary.
5. Validate search, social cards, mobile navigation, and representative long
   architecture/reference pages.
