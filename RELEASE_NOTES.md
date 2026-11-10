# Quirl 0.6.1

A patch release: `printf "a\n"` now prints a line feed, as in Bash and Zsh,
and the TLS library Quirl uses for downloads gets its security fixes.

### Fixed

- A backslash inside double quotes is kept unless it precedes `$`, a backtick,
  `"`, `\`, or a line feed, as in Bash and Zsh. Quirl dropped it before any
  character, so `printf "a\n"` printed `an` instead of `a` and a line feed.
  Arguments passed to Cobra completion are decoded the same way.

### Security

- rustls is updated from 0.23.43 to 0.23.46. Quirl uses it, through `ureq`,
  only to download its completion database and command model over HTTPS. The
  update fixes [GHSA-2mjx-qc3c-rqvc](https://github.com/advisories/GHSA-2mjx-qc3c-rqvc)
  (TLS 1.3 handshake messages accepted across encryption levels),
  [GHSA-c8m5-c952-6cm4](https://github.com/advisories/GHSA-c8m5-c952-6cm4)
  (secrets in a session's `Debug` output),
  [GHSA-w44r-93j7-qv28](https://github.com/advisories/GHSA-w44r-93j7-qv28)
  (a forged ServerHello could limit later handshakes to a classical X25519
  key share), and
  [GHSA-h2v8-394m-p7vv](https://github.com/advisories/GHSA-h2v8-394m-p7vv)
  (Brotli certificate decompression, which Quirl's build does not enable).
