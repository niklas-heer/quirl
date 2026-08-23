import { loader } from 'fumadocs-core/source';
import { defineDocs } from 'fumadocs-mdx/macro';
import { pageSchema } from 'fumadocs-core/source/schema';
import { z } from 'zod';

const blogSchema = pageSchema.extend({
  date: z.string(),
});

const blogDocs = defineDocs({
  dir: 'content/blog',
  docs: {
    schema: blogSchema,
  },
});

// See https://fumadocs.dev/docs/headless/source-api for more info
export const blogSource = loader({
  baseUrl: '/blog',
  source: blogDocs.toFumadocsSource(),
});
