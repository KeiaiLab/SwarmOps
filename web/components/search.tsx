'use client';
// Search over the docs with Orama in the browser: the index is the static file /api/search,
// written at build time, so searching sends nothing to any server.
//
// A query that is exactly a command, MCP tool or error code name (/api/lookup, also static) puts
// that page first, above the engine's results:
//
//   E_BUSY      ->  [E_BUSY  REFERENCE]  /docs/reference/errors#e_busy
//                   ...the engine's results...
import { useEffect, useMemo, useState } from 'react';
import {
  SearchDialog,
  SearchDialogClose,
  SearchDialogContent,
  SearchDialogHeader,
  SearchDialogIcon,
  SearchDialogInput,
  SearchDialogList,
  SearchDialogOverlay,
  type SearchItemType,
  type SharedProps,
} from 'fumadocs-ui/components/dialog/search';
import { useStaticSearch, type SearchResultItem } from 'fumadocs-core/search/client';

const LOOKUP_URL = '/api/lookup';

// The page kind a lookup hit is tagged with: every exact name is a reference page's.
const LOOKUP_KIND = 'Reference';

type Lookup = Map<string, { name: string; url: string }>;

// Fetched once per page load, when the dialog first opens.
let lookupLoad: Promise<Lookup> | null = null;

/** The lookup table, keyed by lower-case name; an unreachable table is an empty one. */
function loadLookup(): Promise<Lookup> {
  if (!lookupLoad) {
    lookupLoad = fetch(LOOKUP_URL)
      .then((res): Promise<Record<string, string>> => (res.ok ? res.json() : Promise.resolve({})))
      .then((table): Lookup => new Map(Object.entries(table).map(([name, url]) => [name.toLowerCase(), { name, url }])))
      .catch((): Lookup => new Map());
  }
  return lookupLoad;
}

/** The result row of an exact-name hit: the name, and the page kind as its tag. */
function LookupHit({ name }: { name: string }) {
  return (
    <span className="flex items-center justify-between gap-3">
      <span className="font-mono">{name}</span>
      <span className="rounded border border-fd-border px-2 font-mono text-xs uppercase tracking-wide text-fd-muted-foreground">
        {LOOKUP_KIND}
      </span>
    </span>
  );
}

export default function StaticSearchDialog(props: SharedProps) {
  const search = useStaticSearch({});
  const [lookup, setLookup] = useState<Lookup | null>(null);

  // 1. The table, on the first open.
  useEffect(() => {
    if (!props.open || lookup) {
      return;
    }
    void loadLookup().then(setLookup);
  }, [props.open, lookup]);

  // 2. An exact name (case aside) first, then the engine's results.
  const query = search.search.trim().toLowerCase();
  const hit = query === '' ? undefined : lookup?.get(query);
  const items = useMemo((): (SearchItemType | SearchResultItem)[] | undefined => {
    if (!hit) {
      return undefined;
    }
    const first: SearchItemType = {
      id: `lookup:${hit.name}`,
      type: 'page',
      url: hit.url,
      content: <LookupHit name={hit.name} />,
    };
    return [first, ...(search.data?.items ?? [])];
  }, [hit, search.data]);

  return (
    <SearchDialog {...search} {...props}>
      <SearchDialogOverlay />
      <SearchDialogContent>
        <SearchDialogHeader>
          <SearchDialogIcon />
          <SearchDialogInput />
          <SearchDialogClose />
        </SearchDialogHeader>
        <SearchDialogList items={items} />
      </SearchDialogContent>
    </SearchDialog>
  );
}
