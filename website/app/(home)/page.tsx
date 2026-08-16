import Image from 'next/image';
import Link from 'next/link';
import {
  ArrowRight,
  BookOpenText,
  Box,
  Braces,
  ShieldCheck,
  Sparkles,
  TerminalSquare,
} from 'lucide-react';

export default function HomePage() {
  return (
    <main className="landing-shell">
      <section className="hero-section">
        <div className="hero-glow" aria-hidden="true" />
        <div className="hero-copy">
          <div className="release-pill">
            <span className="release-dot" />
            Unreleased Unix candidate · 0.1
          </div>
          <p className="hero-kicker">A well-stirred shell.</p>
          <h1>Your shell, with a richer vocabulary.</h1>
          <p className="hero-lede">
            Quirl keeps the Bash and Zsh commands you already know, then adds
            explicit typed data pipelines and one sandboxed Lua 5.4 SDK for
            configuration, scripts, prompts, completions, and plugins.
          </p>
          <div className="hero-actions">
            <Link href="/docs" className="button button-primary">
              Read the docs <ArrowRight size={16} />
            </Link>
            <a
              href="https://github.com/niklas-heer/quirl"
              className="button button-secondary"
            >
              View on GitHub
            </a>
          </div>
          <p className="hero-note">
            Unreleased prototype · Linux and macOS · Built from source
          </p>
        </div>

        <div className="terminal-wrap" aria-label="Example Quirl session">
          <Image
            className="hero-mark"
            src="/logo.png"
            width={520}
            height={520}
            alt=""
            priority
          />
          <div className="terminal-card">
            <div className="terminal-bar">
              <div className="terminal-lights" aria-hidden="true">
                <span />
                <span />
                <span />
              </div>
              <span>quirl — data mode</span>
              <span className="terminal-mode">DATA</span>
            </div>
            <div className="terminal-body">
              <div className="terminal-line">
                <span className="prompt">❯</span>
                <span className="command">data</span>
              </div>
              <div className="terminal-line terminal-input">
                <span className="prompt data-prompt">∿</span>
                <span>
                  open <strong>deployments.json</strong> | where status ==
                  <em> &quot;failed&quot;</em> | select service region
                </span>
              </div>
              <div className="terminal-table">
                <div className="table-row table-head">
                  <span>service</span>
                  <span>region</span>
                </div>
                <div className="table-row">
                  <span>payments</span>
                  <span>eu-central-1</span>
                </div>
                <div className="table-row">
                  <span>search</span>
                  <span>us-east-1</span>
                </div>
              </div>
              <div className="terminal-line terminal-input">
                <span className="prompt data-prompt">∿</span>
                <span className="cursor" aria-hidden="true" />
              </div>
            </div>
            <div className="terminal-footer">
              <span>Alt-M command mode</span>
              <span>Tab complete</span>
              <span>F1 help</span>
            </div>
          </div>
        </div>
      </section>

      <section className="principles-section" aria-labelledby="principles-title">
        <div className="section-heading">
          <p className="section-label">One coherent system</p>
          <h2 id="principles-title">Familiar at the prompt. Explicit underneath.</h2>
          <p>
            Quirl gives command bytes, structured values, and extension code
            distinct boundaries—then makes moving between them deliberate.
          </p>
        </div>
        <div className="feature-grid">
          <article className="feature-card">
            <TerminalSquare />
            <h3>Familiar command mode</h3>
            <p>
              Quoting, redirects, byte pipes, boolean lists, and jobs run
              natively, with Bash and Zsh islands for the rest.
            </p>
          </article>
          <article className="feature-card">
            <Braces />
            <h3>Typed data pipelines</h3>
            <p>
              Switch explicitly into structured values and use focused
              transforms instead of parsing columns with text tools.
            </p>
          </article>
          <article className="feature-card">
            <ShieldCheck />
            <h3>Sandboxed Lua</h3>
            <p>
              One restricted, resource-budgeted Lua 5.4 runtime powers the
              programmable surface behind a Rust-validated boundary.
            </p>
          </article>
          <article className="feature-card">
            <Sparkles />
            <h3>Semantic completion</h3>
            <p>
              One catalog drives completion, contextual help, generated docs,
              and AI-facing command metadata.
            </p>
          </article>
        </div>
      </section>

      <section className="boundary-section">
        <div className="boundary-copy">
          <p className="section-label">Designed around boundaries</p>
          <h2>Three modes of work. No hidden coercion.</h2>
          <p>
            Command mode moves bytes. Data mode moves typed values. Lua handles
            general-purpose logic inside a constrained runtime. The boundary is
            visible every time you cross it.
          </p>
          <Link href="/docs/architecture/product-and-language-design" className="text-link">
            Read the language design <ArrowRight size={15} />
          </Link>
        </div>
        <div className="boundary-map" aria-label="Quirl execution layers">
          <div className="boundary-node">
            <TerminalSquare />
            <span>Command</span>
            <small>byte streams</small>
          </div>
          <span className="boundary-arrow">→</span>
          <div className="boundary-node boundary-node-accent">
            <Box />
            <span>Data</span>
            <small>typed values</small>
          </div>
          <span className="boundary-arrow">→</span>
          <div className="boundary-node">
            <Braces />
            <span>Lua</span>
            <small>bounded logic</small>
          </div>
        </div>
      </section>

      <section className="status-section">
        <div className="status-icon"><BookOpenText /></div>
        <div>
          <p className="section-label">The foundation is here</p>
          <h2>Documentation for the project as it exists today.</h2>
          <p>
            Quirl is an unreleased 0.1 Unix candidate implementation and a
            fast-moving prototype. The docs separate current usage and runtime
            contracts from long-term direction and exact-artifact evidence.
          </p>
        </div>
        <Link href="/docs" className="button button-secondary">
          Explore everything <ArrowRight size={16} />
        </Link>
      </section>

      <footer className="landing-footer">
        <div className="footer-brand">
          <Image src="/logo.png" width={28} height={28} alt="" />
          <span>Quirl</span>
        </div>
        <p>Everything you need, mixed in.</p>
        <div>
          <Link href="/docs/project/security">Security</Link>
          <Link href="/docs/contributing">Contributing</Link>
          <a href="https://github.com/niklas-heer/quirl">GitHub</a>
        </div>
      </footer>
    </main>
  );
}
