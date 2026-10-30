// Content collections. The docs are the repository's own Markdown (../docs and ../CHANGELOG.md),
// never copied: the site renders what GitHub shows. Product pages are web/content/.
import * as path from 'node:path';
import type { Element, ElementContent, Root, RootContent } from 'hast';
import type { VFile } from 'vfile';
import { defineCollections, defineConfig } from 'fumadocs-mdx/config';
import { pageSchema } from 'fumadocs-core/source/schema';
import { z } from 'zod';
import { remarkRepoLinks } from './lib/remark-repo-links';
import { firstHeading, firstParagraph } from './lib/markdown-meta';

// The build runs in web/; the repository is its parent.
const REPO_ROOT = path.resolve('..');

// Product pages, whose bold opening sentences become lead-ins.
const PRODUCT_DIR = path.resolve('content/product');

// Repository docs: title and description from the text, front matter optional.
export const docs = defineCollections({
  type: 'doc',
  dir: '..',
  files: [
    'docs/**/*.md',
    'CHANGELOG.md',
    '!docs/superpowers/**',
    '!docs/reference/notes/**',
    '!docs/examples/**',
  ],
  schema: ({ source, path: file }) => {
    const title = firstHeading(source, path.basename(file, '.md'));
    return pageSchema.extend({
      title: z
        .string()
        .default(title),
      description: z
        .string()
        .default(firstParagraph(source) ?? title),
    });
  },
});

// Product pages and the roadmap: Markdown with title and description front matter.
export const product = defineCollections({
  type: 'doc',
  dir: 'content/product',
  files: ['**/*.md'],
  schema: pageSchema.extend({ description: z.string() }),
});

export const roadmap = defineCollections({
  type: 'doc',
  dir: 'content',
  files: ['roadmap.md'],
  schema: pageSchema.extend({ description: z.string() }),
});

// Code colours: the readable-site design board's five checked colours per theme (Tokens board,
// "Code colours"), each at least 5.5:1 on the code background. Command names, unquoted
// arguments and operators stay plain.
interface CodePalette {
  plain: string;
  background: string;
  flags: string;
  strings: string;
  variables: string;
  comments: string;
}

const LIGHT_CODE: CodePalette = {
  plain: '#2b2e35',
  background: '#f3f3f0',
  flags: '#5a3bd6',
  strings: '#116b3d',
  variables: '#9a4500',
  comments: '#5c626c',
};

const DARK_CODE: CodePalette = {
  plain: '#d1d5dc',
  background: '#181b21',
  flags: '#b9a8ff',
  strings: '#7fd19b',
  variables: '#f0a35a',
  comments: '#9da4b0',
};

// A Shiki (TextMate) theme from a palette. A deeper scope wins, so `string.unquoted` (a shell
// argument) is plain while `constant.other.option` inside it (a flag) is coloured.
function codeTheme(name: string, type: 'light' | 'dark', c: CodePalette) {
  return {
    name,
    type,
    colors: { 'editor.foreground': c.plain, 'editor.background': c.background },
    tokenColors: [
      { settings: { foreground: c.plain, background: c.background } },
      { scope: ['comment', 'punctuation.definition.comment', 'meta.output'], settings: { foreground: c.comments } },
      { scope: ['string', 'punctuation.definition.string'], settings: { foreground: c.strings } },
      { scope: ['string.unquoted', 'keyword.operator', 'entity.name.function', 'support.function'], settings: { foreground: c.plain } },
      {
        scope: ['variable', 'punctuation.definition.variable', 'constant.numeric', 'constant.language'],
        settings: { foreground: c.variables },
      },
      {
        scope: ['constant.other.option', 'support.type.property-name', 'entity.name.tag', 'keyword', 'storage'],
        settings: { foreground: c.flags },
      },
    ],
  };
}

// Code figures, built at compile time so they need no script:
//
//   +-- SHELL ---------------------------- [Copy] --+   the command: Fumadocs' code block, its
//   | so land "$C3" --key "$(uuidgen)"              |   label in the strip beside the copy button
//   +-- OUTPUT -------------------------------------+   what it printed, joined below: quieter,
//   | {"ok":<wbr>true,<wbr>"op":<wbr>"land",...      |   wrapping after , : { [ /
//   | > Show 9 more lines                           |   over 12 lines: 8, the rest in <details>
//   +-----------------------------------------------+
const SHELL_LANGS = new Set(['sh', 'bash', 'shell', 'zsh', 'shellscript', 'console', 'shellsession']);
const OUTPUT_LANGS = new Set(['text', 'txt', 'plaintext', 'json']);
const LABELS: Record<string, string> = {
  json: 'JSON',
  yaml: 'YAML',
  yml: 'YAML',
  toml: 'TOML',
  python: 'Python',
  rust: 'Rust',
  nginx: 'Nginx',
  apache: 'Apache',
  mermaid: 'Mermaid',
};
const SHELL_LABEL = 'Shell';
const TEXT_LABEL = 'Text';
const SYNTAX_LABEL = 'Syntax';
const OUTPUT_LABEL = 'Output';
const ERROR_LABEL = 'Output, an error';
const FOLD_OVER = 12;
const FOLD_SHOW = 8;
const BREAK_AFTER = /([,:{[/])/;
const ERROR_LINE = /^error[:[]/;
const NOT_OK = /"ok":\s*false/;
const LANGUAGE_CLASS = 'language-';

function classesOf(node: Element): string[] {
  const value = node.properties.className ?? node.properties.class;
  if (Array.isArray(value)) {
    return value.map(String);
  }
  return typeof value === 'string' ? value.split(/\s+/) : [];
}

function addClass(node: Element, name: string) {
  node.properties.className = [...classesOf(node), name];
  delete node.properties.class;
}

function textOf(node: RootContent | ElementContent): string {
  if (node.type === 'text') {
    return node.value;
  }
  if (node.type !== 'element') {
    return '';
  }
  return node.children
    .map(textOf)
    .join('');
}

function el(tagName: string, className: string[], children: ElementContent[]): Element {
  return { type: 'element', tagName, properties: className.length > 0 ? { className } : {}, children };
}

function isBlank(node: RootContent | ElementContent): boolean {
  return node.type === 'text' && node.value.trim() === '';
}

// The highlighted block's language, from the class `addLanguageClass` puts on its <code>.
function codeLang(node: RootContent | ElementContent | undefined): string | null {
  if (node?.type !== 'element' || node.tagName !== 'pre') {
    return null;
  }
  const code = node.children.find((child): child is Element => child.type === 'element' && child.tagName === 'code');
  const lang = code ? classesOf(code).find((name) => name.startsWith(LANGUAGE_CLASS)) : undefined;
  return lang ? lang.slice(LANGUAGE_CLASS.length) : null;
}

// Output text with a <wbr> after each , : { [ /, so JSON wraps instead of scrolling.
function breakable(text: string): ElementContent[] {
  return text
    .split(BREAK_AFTER)
    .filter((part) => part !== '')
    .flatMap((part): ElementContent[] =>
      BREAK_AFTER.test(part) ? [{ type: 'text', value: part }, el('wbr', [], [])] : [{ type: 'text', value: part }],
    );
}

function outputText(lines: string[]): Element {
  return el('samp', ['code-out-text'], breakable(lines.join('\n')));
}

// The output block as a figure of its own (no <pre>: MDX would give it a copy button).
function outputFigure(pre: Element): Element {
  // 1. The lines, as Shiki split them.
  const code = pre.children.find((child): child is Element => child.type === 'element' && child.tagName === 'code');
  const lines = (code?.children ?? [])
    .filter((child): child is Element => child.type === 'element')
    .map(textOf);

  // 2. An error says so in its label and gets an ink edge.
  const text = lines.join('\n');
  const error = ERROR_LINE.test(text.trimStart()) || NOT_OK.test(text);
  const caption = el('figcaption', [], [el('span', ['code-label'], [{ type: 'text', value: error ? ERROR_LABEL : OUTPUT_LABEL }])]);
  const figure = el('figure', ['code-out', 'not-prose', ...(error ? ['code-out-err'] : [])], [caption]);

  // 3. Short output in full; long output shows its first lines and folds the rest.
  if (lines.length <= FOLD_OVER) {
    figure.children.push(outputText(lines));
    return figure;
  }
  const rest = lines.length - FOLD_SHOW;
  const summary = el('summary', [], [el('span', [], [{ type: 'text', value: `Show ${rest} more lines` }])]);
  figure.children.push(
    outputText(lines.slice(0, FOLD_SHOW)),
    el('details', ['code-fold'], [summary, outputText(lines.slice(FOLD_SHOW))]),
  );
  return figure;
}

// A block's label: from its language, "Syntax" for the text under a Syntax heading.
function labelOf(lang: string, before: RootContent | ElementContent | undefined): string {
  if (SHELL_LANGS.has(lang)) {
    return SHELL_LABEL;
  }
  if (lang in LABELS) {
    return LABELS[lang];
  }
  const heading = before?.type === 'element' && /^h[1-6]$/.test(before.tagName);
  if (heading && textOf(before).trim() === SYNTAX_LABEL) {
    return SYNTAX_LABEL;
  }
  return OUTPUT_LANGS.has(lang) ? TEXT_LABEL : lang;
}

// Shiki hands each block back wrapped in a root of its own; lifting it out puts a command and
// its output side by side.
function unwrapRoots(parent: Root | Element) {
  const kids = parent.children as (RootContent | ElementContent | Root)[];
  parent.children = kids.flatMap((kid) => (kid.type === 'root' ? kid.children : [kid])) as typeof parent.children;
}

// Labels and joins the code blocks among one parent's children.
function figureChildren(parent: Root | Element) {
  unwrapRoots(parent);
  const kids = parent.children as (RootContent | ElementContent)[];
  let before: RootContent | ElementContent | undefined;
  for (let i = 0; i < kids.length; i++) {
    const node = kids[i];
    const lang = codeLang(node);
    if (node.type !== 'element' || lang === null) {
      if (!isBlank(node)) {
        before = node;
      }
      continue;
    }

    // 1. The label goes into Fumadocs' caption strip (its `title`); the file icon is dropped.
    const label = labelOf(lang, before);
    delete node.properties.icon;
    node.properties.title = label;
    node.properties.dataLabel = label;
    addClass(node, 'code-block');
    before = node;

    // 2. A command and the output right after it become one figure.
    let next = i + 1;
    while (next < kids.length && isBlank(kids[next])) {
      next++;
    }
    const nextLang = codeLang(kids[next]);
    if (!SHELL_LANGS.has(lang) || nextLang === null || !OUTPUT_LANGS.has(nextLang)) {
      continue;
    }
    const joined = el('div', ['code-ex'], [node, outputFigure(kids[next] as Element)]);
    kids.splice(i, next - i + 1, joined);
  }
}

// Product pages: a list item that opens with a bold sentence ("**Full fsync, always.** ...")
// marks it as a lead-in, which the stylesheet sets as a block.
function markLeadIns(node: Root | Element) {
  for (const child of node.children) {
    if (child.type !== 'element') {
      continue;
    }
    markLeadIns(child);
    if (child.tagName !== 'li') {
      continue;
    }
    const first = child.children.find((kid) => !isBlank(kid));
    const opener = first?.type === 'element' && first.tagName === 'p' ? first.children.find((kid) => !isBlank(kid)) : first;
    if (opener?.type === 'element' && opener.tagName === 'strong' && textOf(opener).trim().endsWith('.')) {
      addClass(opener, 'lead-in');
    }
  }
}

function rehypeCodeFigures() {
  return (tree: Root, file: VFile) => {
    // 1. Every parent's children, the page itself first.
    const parents: (Root | Element)[] = [tree];
    while (parents.length > 0) {
      const parent = parents.pop() as Root | Element;
      figureChildren(parent);
      for (const child of parent.children) {
        // A joined figure is done; a block holds no blocks.
        if (child.type === 'element' && child.tagName !== 'pre' && !classesOf(child).includes('code-ex')) {
          parents.push(child);
        }
      }
    }

    // 2. Lead-ins on product pages only.
    if (!path.relative(PRODUCT_DIR, file.path).startsWith('..')) {
      markLeadIns(tree);
    }
  };
}

export default defineConfig({
  mdxOptions: {
    // Links first, so the image plugin sees site URLs.
    remarkPlugins: (plugins) => [[remarkRepoLinks, { root: REPO_ROOT }], ...plugins],
    // Code figures after Shiki, which runs first.
    rehypePlugins: (plugins) => [...plugins, rehypeCodeFigures],
    // Offline builds: no remote image is fetched, and an image without a size renders unsized.
    remarkImageOptions: { useImport: false, external: false, onError: 'ignore' },
    rehypeCodeOptions: {
      themes: {
        light: codeTheme('swarmops-light', 'light', LIGHT_CODE),
        dark: codeTheme('swarmops-dark', 'dark', DARK_CODE),
      },
      // The rehype step below reads each block's language from this class.
      addLanguageClass: true,
    },
  },
});
