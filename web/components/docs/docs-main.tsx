'use client';
// A docs page whose `main` carries the id the skip link targets. Fumadocs' own container puts
// the page's props on the `article` inside `main`; this one is the same frame with the id on
// `main`:
//
//   main#main            the grid cell
//     article#nd-page    breadcrumb, page, footer
import type { ComponentProps } from 'react';
import { DocsPage } from 'fumadocs-ui/layouts/docs/page';
import { MAIN_ID } from '@/components/skip-link';

// Fumadocs' article classes (layouts/docs/page/slots/container), unchanged.
const ARTICLE =
  'flex flex-col min-w-0 w-full max-w-[900px] px-4 py-6 gap-4 md:px-6 md:pt-8 xl:px-8 xl:pt-14 md:in-data-[sidebar-collapsed=true]:pt-16 xl:in-data-[sidebar-collapsed=true]:pt-14';

function Main({ className, ...props }: ComponentProps<'article'>) {
  return (
    <main id={MAIN_ID} className="grid [grid-area:main] justify-items-center" data-layout-main="">
      <article id="nd-page" data-layout-content="" {...props} className={className ? `${ARTICLE} ${className}` : ARTICLE} />
    </main>
  );
}

export function DocsMain(props: ComponentProps<typeof DocsPage>) {
  return <DocsPage {...props} slots={{ ...props.slots, container: Main }} />;
}
