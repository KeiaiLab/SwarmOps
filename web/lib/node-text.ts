// The text of a React node. TOC titles are the headings' rendered content: a heading that holds a
// link would put an <a> inside the TOC's own link, which HTML forbids, so the TOC takes the text.
import { isValidElement, type ReactNode } from 'react';

export function nodeText(node: ReactNode): string {
  if (typeof node === 'string' || typeof node === 'number') {
    return String(node);
  }
  if (Array.isArray(node)) {
    return node
      .map(nodeText)
      .join('');
  }
  if (isValidElement<{ children?: ReactNode }>(node)) {
    return nodeText(node.props.children);
  }
  return '';
}
