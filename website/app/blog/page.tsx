import Link from 'next/link';
import type { Metadata } from 'next';
import { HomeLayout } from 'fumadocs-ui/layouts/home';
import { baseOptions } from '@/lib/layout.shared';
import { blogSource } from '@/lib/blog-source';

export const metadata: Metadata = {
  title: 'Blog — Quirl',
  description: 'Notes on building Quirl: design decisions, releases, and what changes along the way.',
};

function CassetteReels() {
  return (
    <span className="cassette-reels" aria-hidden="true">
      <span />
      <span />
    </span>
  );
}

export default function BlogIndexPage() {
  const posts = [...blogSource.getPages()].sort((a, b) =>
    String(b.data.date).localeCompare(String(a.data.date)) ||
    a.url.localeCompare(b.url),
  );

  return (
    <HomeLayout {...baseOptions()}>
      <main className="landing-shell blog-shell">
        <section className="hero blog-hero">
          <div className="pill">
            <span className="dot" />
            From the workshop
          </div>
          <p className="kicker">The Quirl blog</p>
          <h1 className="glitch-h1">
            <span className="line">
              Notes from behind
            </span>
            <span className="line">
              the whisk.
            </span>
          </h1>
          <p className="lede">
            How Quirl gets built, what changes, and why — written as it
            happens.
          </p>
        </section>

        <section aria-labelledby="posts-title">
          <p className="section-label" id="posts-title">
            Posts
          </p>
          <div className="blog-post-list">
            {posts.map((post) => (
              <Link key={post.url} href={post.url} className="cassette blog-post-card">
                <div className="cassette-label">
                  <span>{post.data.date}</span>
                  <CassetteReels />
                </div>
                <div className="cassette-body">
                  <h2>{post.data.title}</h2>
                  {post.data.description ? <p>{post.data.description}</p> : null}
                </div>
              </Link>
            ))}
          </div>
        </section>
      </main>
    </HomeLayout>
  );
}
