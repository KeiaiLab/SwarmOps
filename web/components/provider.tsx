'use client';
// The docs' theme (dark/light, following the system first) and static search dialog.
//
// `/docs?search=1` opens the dialog on load: the product header's Search link and shortcut lead
// there. With scripts off the same link is simply the docs home.
import dynamic from 'next/dynamic';
import { RootProvider } from 'fumadocs-ui/provider/next';
import { useSearchContext } from 'fumadocs-ui/contexts/search';
import { useEffect, useRef, type ReactNode } from 'react';

// The dialog and its search engine load when the reader first opens search.
const SearchDialog = dynamic(() => import('@/components/search'), { ssr: false });

// The query parameter that asks for the dialog, and its value.
const SEARCH_PARAM = 'search';
const SEARCH_ON = '1';

/** Opens the search dialog once when the address asks for it. */
function OpenSearchOnLoad() {
  const { setOpenSearch } = useSearchContext();
  const done = useRef(false);
  useEffect(() => {
    if (done.current) {
      return;
    }
    done.current = true;
    if (new URLSearchParams(window.location.search).get(SEARCH_PARAM) === SEARCH_ON) {
      setOpenSearch(true);
    }
  }, [setOpenSearch]);
  return null;
}

export function Provider({ children }: { children: ReactNode }) {
  return (
    <RootProvider search={{ SearchDialog }}>
      <OpenSearchOnLoad />
      {children}
    </RootProvider>
  );
}
