// The first focusable element of every page: a link past the header to the page's `main`.
export const MAIN_ID = 'main';

export function SkipLink() {
  return (
    <a className="skip-link" href={`#${MAIN_ID}`}>
      Skip to content
    </a>
  );
}
