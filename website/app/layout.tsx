import { RootProvider } from 'fumadocs-ui/provider/next';
import './global.css';
import { Inter, JetBrains_Mono } from 'next/font/google';
import type { Metadata } from 'next';

const inter = Inter({
  subsets: ['latin'],
  variable: '--font-sans',
});

const mono = JetBrains_Mono({
  subsets: ['latin'],
  variable: '--font-mono',
});

export const metadata: Metadata = {
  title: {
    default: 'Quirl — A well-stirred shell',
    template: '%s · Quirl',
  },
  description:
    'Bash muscle memory, typed data pipelines, and one sandboxed Lua SDK in a single fast Rust binary.',
  metadataBase: new URL(
    process.env.NEXT_PUBLIC_SITE_URL ?? 'http://localhost:3000',
  ),
  openGraph: {
    title: 'Quirl — A well-stirred shell',
    description:
      'Bash muscle memory, typed data pipelines, and one sandboxed Lua SDK.',
    type: 'website',
  },
  twitter: {
    card: 'summary_large_image',
    title: 'Quirl — A well-stirred shell',
    description:
      'Bash muscle memory, typed data pipelines, and one sandboxed Lua SDK.',
  },
};

export default function Layout({ children }: LayoutProps<'/'>) {
  return (
    <html
      lang="en"
      className={`${inter.variable} ${mono.variable}`}
      suppressHydrationWarning
    >
      <body className="flex flex-col min-h-screen">
        <RootProvider>{children}</RootProvider>
      </body>
    </html>
  );
}
