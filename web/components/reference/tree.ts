// The rendered Markdown of a page as a flat list of top-level blocks, cut into sections. A page's
// MDX renders to one fragment of blocks (h1, p, pre, h2, ...); the reference components find the
// blocks they replace by their component, so they never parse the HTML:
//
//   [h1, p, pre(yaml), h2 Syntax, pre, h2 Required arguments, h3 --message, p, p, h3 ...]
//    \__ lead ____________/ \__ section ______/ \__ section: lead [], parts [--message, ...] __/
import { Children, Fragment, isValidElement, type ElementType, type ReactElement, type ReactNode } from 'react';
import { nodeText } from '@/lib/node-text';

type Props = { id?: string; children?: ReactNode; href?: string };
export type Block = ReactElement<Props>;

/** One heading and the blocks under it, up to the next heading of its level. */
export interface Part {
  heading: Block;
  id: string;
  title: string;
  body: Block[];
}

/** Whether a node is an element rendered by `type`. */
export function isA(node: ReactNode, type: ElementType): node is Block {
  return isValidElement(node) && node.type === type;
}

/** The top-level blocks of a rendered MDX body; the newlines between them are dropped. */
export function blocksOf(content: ReactNode): Block[] {
  const root = isValidElement<Props>(content) ? content.props.children : content;
  return Children.toArray(root).flatMap((node) => {
    // A code block comes wrapped in a fragment of its own: its block is the one inside.
    if (isA(node, Fragment)) {
      return blocksOf(node);
    }
    return isValidElement<Props>(node) ? [node] : [];
  });
}

/** Cuts blocks at each heading of `type`: the blocks before the first, then one part per heading. */
export function cut(blocks: Block[], type: ElementType): { lead: Block[]; parts: Part[] } {
  const lead: Block[] = [];
  const parts: Part[] = [];
  for (const block of blocks) {
    if (isA(block, type)) {
      parts.push({
        heading: block,
        id: block.props.id ?? '',
        title: nodeText(block.props.children),
        body: [],
      });
      continue;
    }
    (parts.at(-1)?.body ?? lead).push(block);
  }
  return { lead, parts };
}

/** The children of an element as a list of nodes. */
export function childrenOf(block: Block): ReactNode[] {
  return Children.toArray(block.props.children);
}

/** The node itself when it is a link (an element with an `href`), else null. */
export function asLink(node: ReactNode): Block | null {
  if (!isValidElement<Props>(node) || node.props.href === undefined) {
    return null;
  }
  return node;
}
