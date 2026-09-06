import { ImageResponse } from 'next/og';

export const alt = 'Quirl — A well-stirred shell';
export const size = { width: 1200, height: 630 };
export const contentType = 'image/png';

export default function OpenGraphImage() {
  return new ImageResponse(
    <div
      style={{
        alignItems: 'stretch',
        background: '#0e0e13',
        color: '#fafaf8',
        display: 'flex',
        flexDirection: 'column',
        height: '100%',
        justifyContent: 'space-between',
        overflow: 'hidden',
        padding: '64px 72px',
        position: 'relative',
        width: '100%',
      }}
    >
      <div
        style={{
          background:
            'radial-gradient(circle at center, rgba(124, 58, 237, 0.8), rgba(224, 68, 124, 0.28) 38%, rgba(247, 147, 26, 0) 72%)',
          borderRadius: '999px',
          display: 'flex',
          height: 760,
          position: 'absolute',
          right: -250,
          top: -260,
          width: 760,
        }}
      />
      <div
        style={{
          alignItems: 'center',
          display: 'flex',
          fontSize: 28,
          fontWeight: 700,
          gap: 16,
          letterSpacing: '-0.03em',
        }}
      >
        <span
          style={{
            background: 'linear-gradient(120deg, #f7931a, #e0447c, #7c3aed)',
            borderRadius: 999,
            display: 'flex',
            height: 34,
            width: 34,
          }}
        />
        Quirl
        <span
          style={{
            border: '1px solid #3b3848',
            borderRadius: 999,
            color: '#aaa6b5',
            display: 'flex',
            fontSize: 14,
            letterSpacing: '0.08em',
            marginLeft: 8,
            padding: '7px 11px',
          }}
        >
          v0.3.0
        </span>
      </div>
      <div style={{ display: 'flex', flexDirection: 'column', maxWidth: 850 }}>
        <div
          style={{
            background: 'linear-gradient(100deg, #f7931a, #e0447c, #a78bfa)',
            backgroundClip: 'text',
            color: 'transparent',
            display: 'flex',
            fontSize: 26,
            fontWeight: 650,
            marginBottom: 22,
          }}
        >
          A well-stirred shell.
        </div>
        <div
          style={{
            display: 'flex',
            fontSize: 72,
            fontWeight: 700,
            letterSpacing: '-0.06em',
            lineHeight: 0.98,
          }}
        >
          Your shell, with a richer vocabulary.
        </div>
      </div>
      <div
        style={{
          borderTop: '1px solid #2d2b37',
          color: '#aaa6b5',
          display: 'flex',
          fontSize: 19,
          justifyContent: 'space-between',
          paddingTop: 24,
        }}
      >
        <span>Bash muscle memory · typed data · sandboxed Lua</span>
        <span>Rust · Linux · macOS</span>
      </div>
    </div>,
    size,
  );
}
