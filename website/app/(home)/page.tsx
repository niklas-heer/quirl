import Image from 'next/image';
import Link from 'next/link';
import { HeroDemo } from '@/components/hero-demo';

function ReelDots() {
  return (
    <span className="reels" aria-hidden="true">
      <span />
      <span />
    </span>
  );
}

function CassetteReels() {
  return (
    <span className="cassette-reels" aria-hidden="true">
      <span />
      <span />
    </span>
  );
}

export default function HomePage() {
  return (
    <main className="landing-shell">
      <section className="hero">
        <div className="pill">
          <span className="dot" />
          v0.2.0 · Linux and macOS
        </div>
        <p className="kicker">A well-stirred shell</p>
        <h1 className="glitch-h1">
          <span className="line">
            Your shell, with a
          </span>
          <span className="line">
            richer vocabulary.
          </span>
        </h1>
        <p className="lede">
          Quirl keeps the Bash and Zsh commands you already know, then adds
          explicit typed data pipelines and one sandboxed Lua 5.5.1 SDK for
          configuration, scripts, prompts, completions, and plugins.
        </p>
        <div className="actions">
          <Link
            href="/docs/getting-started/installation"
            className="button button-primary"
          >
            <ReelDots />
            Install Quirl
          </Link>
          <a
            href="https://github.com/niklas-heer/quirl"
            className="button button-secondary"
          >
            <ReelDots />
            View on GitHub
          </a>
        </div>
        <p className="hero-note">
          Available now · Homebrew and verified native release archives
        </p>
      </section>

      <div className="demo-wrap">
        <div
          className="cassette demo-cassette"
          aria-label="Quirl v0.2.0 recording"
        >
          <div className="cassette-label">
            <span>SIDE A — QUIRL v0.2.0</span>
            <CassetteReels />
          </div>
          <HeroDemo />
          <p id="demo-recording-context" style={{ padding: '1rem', margin: 0 }}>
            Recorded with a local macOS ARM release-profile build of v0.2.0 from commit <code>3958920</code>.
            {' '}See the <a href="/quirl-demo-provenance.json">recording provenance</a>,
            {' '}<Link href="/docs/getting-started/first-session">first-session guide</Link>,
            {' '}and <Link href="/docs/research/benchmarks/release-v0.2.0">release measurements</Link>.
            {' '}Search results are candidates to review.
          </p>
        </div>
      </div>

      <section aria-labelledby="principles-title">
        <p className="section-label">One coherent system</p>
        <div className="section-heading">
          <h2 id="principles-title">
            Familiar at the prompt. Explicit underneath.
          </h2>
          <p>
            Quirl gives command bytes, structured values, and extension code
            distinct boundaries—then makes moving between them deliberate.
          </p>
        </div>
        <div className="feature-grid">
          <article className="cassette">
            <div className="cassette-label">
              <span>01</span>
              <CassetteReels />
            </div>
            <div className="cassette-body">
              <h3>Familiar command mode</h3>
              <p>
                Quoting, redirects, byte pipes, boolean lists, and jobs run
                natively, with Bash and Zsh islands for the rest.
              </p>
            </div>
          </article>
          <article className="cassette">
            <div className="cassette-label">
              <span>02</span>
              <CassetteReels />
            </div>
            <div className="cassette-body">
              <h3>Typed data pipelines</h3>
              <p>
                Switch explicitly into structured values and use focused
                transforms instead of parsing columns with text tools.
              </p>
            </div>
          </article>
          <article className="cassette">
            <div className="cassette-label">
              <span>03</span>
              <CassetteReels />
            </div>
            <div className="cassette-body">
              <h3>Sandboxed Lua</h3>
              <p>
                One restricted, resource-budgeted Lua 5.5.1 runtime powers the
                programmable surface behind a Rust-validated boundary.
              </p>
            </div>
          </article>
          <article className="cassette">
            <div className="cassette-label">
              <span>04</span>
              <CassetteReels />
            </div>
            <div className="cassette-body">
              <h3>Semantic completion</h3>
              <p>
                One catalog drives completion, contextual help, generated
                docs, and AI-facing command metadata.
              </p>
            </div>
          </article>
        </div>
      </section>

      <section>
        <p className="section-label">Designed around boundaries</p>
        <div className="section-heading">
          <h2>Three modes of work. No hidden coercion.</h2>
          <p>
            Command mode moves bytes. Data mode moves typed values. Lua
            handles general-purpose logic inside a constrained runtime. The
            boundary is visible every time you cross it.
          </p>
        </div>

        <div className="ribbon-wrap">
          <svg
            viewBox="0 0 1000 200"
            xmlns="http://www.w3.org/2000/svg"
            role="img"
            aria-label="Command, data, and Lua cassettes connected by tape into Quirl"
          >
            <rect
              x="10"
              y="70"
              width="150"
              height="60"
              rx="10"
              fill="var(--lp-panel)"
              stroke="var(--lp-magenta)"
              strokeWidth="2"
            />
            <text
              x="85"
              y="105"
              textAnchor="middle"
              fill="var(--lp-text)"
              fontFamily="var(--font-display), cursive"
              fontSize="15"
            >
              CMD
            </text>
            <rect
              x="290"
              y="70"
              width="150"
              height="60"
              rx="10"
              fill="var(--lp-panel)"
              stroke="var(--lp-cyan)"
              strokeWidth="2"
            />
            <text
              x="365"
              y="105"
              textAnchor="middle"
              fill="var(--lp-text)"
              fontFamily="var(--font-display), cursive"
              fontSize="15"
            >
              DATA
            </text>
            <rect
              x="570"
              y="70"
              width="150"
              height="60"
              rx="10"
              fill="var(--lp-panel)"
              stroke="var(--lp-amber)"
              strokeWidth="2"
            />
            <text
              x="645"
              y="105"
              textAnchor="middle"
              fill="var(--lp-text)"
              fontFamily="var(--font-display), cursive"
              fontSize="15"
            >
              LUA
            </text>
            <rect
              x="850"
              y="60"
              width="140"
              height="80"
              rx="10"
              fill="var(--lp-panel)"
              stroke="var(--lp-lime)"
              strokeWidth="2.5"
            />
            <text
              x="920"
              y="105"
              textAnchor="middle"
              fill="var(--lp-lime)"
              fontFamily="var(--font-display), cursive"
              fontSize="16"
            >
              QUIRL
            </text>

            <path
              d="M160,100 C210,100 240,100 290,100"
              fill="none"
              stroke="rgba(255,255,255,0.35)"
              strokeWidth="3"
              strokeDasharray="2 6"
            />
            <path
              d="M440,100 C480,100 530,100 570,100"
              fill="none"
              stroke="rgba(255,255,255,0.35)"
              strokeWidth="3"
              strokeDasharray="2 6"
            />
            <path
              d="M720,100 C770,100 800,100 850,100"
              fill="none"
              stroke="rgba(255,255,255,0.35)"
              strokeWidth="3"
              strokeDasharray="2 6"
            />
          </svg>
        </div>
        <Link
          href="/docs/architecture/product-and-language-design"
          className="text-link"
        >
          Read the language design →
        </Link>
      </section>

      <section>
        <div className="cassette status-cassette">
          <div className="cassette-label">
            <span>RELEASE NOTES</span>
            <CassetteReels />
          </div>
          <div className="cassette-body">
            <h2>The 0.2 release is here.</h2>
            <p>
              Quirl 0.2.0 is available for Linux and macOS through Homebrew
              and immutable native release archives. The docs separate
              current usage and runtime contracts from long-term direction
              and exact-artifact evidence.
            </p>
            <Link href="/docs" className="text-link">
              Explore everything →
            </Link>
          </div>
        </div>
      </section>

      <footer className="landing-footer">
        <div className="footer-brand">
          <Image src="/logo.png" width={22} height={22} alt="" />
          <span>Quirl</span>
        </div>
        <p>Everything you need, mixed in.</p>
        <nav>
          <Link href="/docs/project/security">Security</Link>
          <Link href="/docs/contributing">Contributing</Link>
          <a href="https://github.com/niklas-heer/quirl">GitHub</a>
        </nav>
      </footer>
    </main>
  );
}
