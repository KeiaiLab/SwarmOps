// The root layout: metadata every page inherits. Providers live in the two sections' layouts
// (app/docs, app/(product)), so the product pages load no docs script. No web font: the system
// stack in global.css, so a page requests nothing from another origin.
import type { Metadata, Viewport } from 'next';
import type { ReactNode } from 'react';
import { SITE_NAME, SITE_TAGLINE, SITE_URL } from '@/lib/site';
import './global.css';

export const metadata: Metadata = {
  metadataBase: new URL(SITE_URL),
  title: { default: `${SITE_NAME}: a git replacement for coding-agent swarms`, template: `%s | ${SITE_NAME}` },
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

export const viewport: Viewport = {
  themeColor: [
    { media: '(prefers-color-scheme: dark)', color: '#0b0e14' },
    { media: '(prefers-color-scheme: light)', color: '#ffffff' },
  ],
};

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="en" suppressHydrationWarning>
      <body className="flex min-h-screen flex-col">
        {children}
      </body>
    </html>
  );
}
