// The site script, inline, as the first child of <body>: it sets the theme before the first paint.
// It sits in the root layout, not in the product frame, because a link from a docs page renders
// the product page on the client, where React never runs an inline script it inserts.
import { SITE_SCRIPT } from '@/lib/site-script';

export function SiteScript() {
  return <script dangerouslySetInnerHTML={{ __html: SITE_SCRIPT }} />;
}
