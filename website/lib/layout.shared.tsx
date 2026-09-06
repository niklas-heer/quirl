import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared';
import Image from 'next/image';
import { appName, gitConfig } from './shared';

export function baseOptions(): BaseLayoutProps {
  return {
    nav: {
      title: (
        <span className="brand-lockup">
          <Image src="/logo.png" alt="" width={28} height={28} priority />
          <span>{appName}</span>
          <span className="brand-status">v0.3.0</span>
        </span>
      ),
    },
    links: [
      { text: 'Documentation', url: '/docs', active: 'nested-url' },
      { text: 'Blog', url: '/blog', active: 'nested-url' },
      { text: 'Changelog', url: '/docs/project/changelog' },
    ],
    githubUrl: `https://github.com/${gitConfig.user}/${gitConfig.repo}`,
  };
}
