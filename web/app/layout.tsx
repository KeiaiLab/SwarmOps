// The root layout: metadata every page inherits. Providers live in the two sections' layouts
// (app/docs, app/(product)), so the product pages load no docs script; the site script (theme
// before paint) comes first in every page's body. No web font: the system stack in global.css,
// so a page requests nothing from another origin.
import type { Metadata, Viewport } from 'next';
import type { ReactNode } from 'react';
import { SiteScript } from '@/components/site-script';
import { SITE_NAME, SITE_TAGLINE, SITE_URL } from '@/lib/site';
import './global.css';

export const metadata: Metadata = {
  metadataBase: new URL(SITE_URL),
  title: { default: `${SITE_NAME}: version control for swarms of coding agents`, template: `%s | ${SITE_NAME}` },
  description: SITE_TAGLINE,
  icons: {
    icon: [
      { url: '/favicon.ico', sizes: 'any' },
      { url: '/favicon.svg', type: 'image/svg+xml' },
    ],
    apple: '/apple-touch-icon.png',
  },
  manifest: '/site.webmanifest',
  openGraph: {
    type: 'website',
    siteName: SITE_NAME,
    images: [{ url: '/og-image.png', width: 1280, height: 640 }],
  },
  twitter: { card: 'summary_large_image' },
};

// The browser's chrome in the page's own background (global.css, --background).
export const viewport: Viewport = {
  themeColor: [
    { media: '(prefers-color-scheme: dark)', color: '#111317' },
    { media: '(prefers-color-scheme: light)', color: '#fbfbf9' },
  ],
};

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="en" suppressHydrationWarning>
      <body className="flex min-h-screen flex-col">
        <SiteScript />
        {children}
      </body>
    </html>
  );
}
