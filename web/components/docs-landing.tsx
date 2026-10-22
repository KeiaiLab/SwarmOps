// The /docs landing page when docs/README.md does not exist: one card per sidebar group, each
// listing its pages, so every page is one click from /docs.
import Link from 'next/link';
import { DocsBody, DocsPage } from 'fumadocs-ui/layouts/docs/page';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import type * as PageTree from 'fumadocs-core/page-tree';
import { NAV_GROUPS } from '@/lib/docs-paths';
import { source } from '@/lib/source';

/** The pages of a sidebar folder and its sub-folders, in sidebar order. */
function pagesOf(node: PageTree.Node): { name: string; url: string }[] {
  if (node.type === 'page') {
    return [{ name: String(node.name), url: node.url }];
  }
  if (node.type === 'folder') {
    return [...(node.index ? [node.index] : []), ...node.children].flatMap(pagesOf);
  }
  return [];
}

export function DocsLanding() {
  // 1. Each group's root folder in the sidebar, with its pages.
  const folders = source
    .getPageTree()
    .children.filter((node) => node.type === 'folder');
  const groups = NAV_GROUPS
    .map((group) => ({
      ...group,
      pages: folders
        .filter((folder) => folder.name === group.title)
        .flatMap(pagesOf),
    }))
    .filter((group) => group.pages.length > 0);

  // 2. One card per group.
  return (
    <DocsPage toc={[]}>
      <DocsBody>
        <h1>Documentation</h1>
        <p>The SwarmOps documentation, rendered from the repository&apos;s Markdown.</p>
      </DocsBody>
      <div className="not-prose mt-6 grid gap-4 sm:grid-cols-2">
        {groups.map((group) => (
          <Card key={group.key}>
            <CardHeader>
              <CardTitle>
                <h2>{group.title}</h2>
              </CardTitle>
              <CardDescription>{group.description}</CardDescription>
            </CardHeader>
            <CardContent>
              <ul className="space-y-1 text-sm">
                {group.pages.map((page) => (
                  <li key={page.url}>
                    <Link className="text-primary hover:underline" href={page.url}>
                      {page.name}
                    </Link>
                  </li>
                ))}
              </ul>
            </CardContent>
          </Card>
        ))}
      </div>
    </DocsPage>
  );
}
