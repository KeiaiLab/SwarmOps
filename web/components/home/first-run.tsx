// The first commands after the install, with what the 0.11.0 build printed in an empty
// temporary directory, then the MCP registration command of docs/develop/mcp.md, byte for byte.
import { SelectFigure } from '@/components/markdown-page';
import { C, H2, LINK, PARA, Section } from './parts';
import { OutputText } from './sample';

// Pasted from the 0.11.0 build; ids cut to eight hex digits and `meta` to `{…}`.
const COMMANDS = ['swarmops --version', 'swarmops init /tmp/dev-guide/acme --name acme'];
const OUTPUT = [
  'swarmops 0.11.0',
  '{"ok":true,"op":"repo_init","api":1,"result":{"repo_id":"7325b237…","name":"acme","store_version":2,"changesets":0,"refs":0},"meta":{…}}',
];

// docs/develop/mcp.md, "Register the server with Claude Code".
const MCP_ADD = 'claude mcp add swarmops -e SWARMOPS_AGENT=agent/worker-1 -- swarmops mcp --repo /tmp/dev-guide/acme';
const MCP_GUIDE_URL = '/docs/develop/mcp#register-the-server-with-claude-code';

export function FirstRun() {
  return (
    <Section id="first-run" n={1} label="First commands">
      <h2 id="first-run-title" className={H2}>
        A store in two commands
      </h2>
      <p className={PARA + ' mt-4'}>
        Ids are cut to eight hex digits and <C>meta</C> to <C>{'{…}'}</C>; no line is left out.
      </p>

      {/* 1. The two commands and what they printed. */}
      <div className="code-ex mt-8">
        <SelectFigure label="Shell" tabIndex={0}>
          <code>{COMMANDS.join('\n')}</code>
        </SelectFigure>
        <figure className="code-out not-prose">
          <figcaption>
            <span className="code-label">Output</span>
          </figcaption>
          <OutputText lines={OUTPUT} />
        </figure>
      </div>

      {/* 2. The store, given to one Claude Code agent over MCP. */}
      <h3 className="mt-12 text-base font-semibold text-foreground">Give the store to an agent</h3>
      <div className="mt-4">
        <SelectFigure label="Shell" tabIndex={0}>
          <code>{MCP_ADD}</code>
        </SelectFigure>
      </div>
      <p className={PARA + ' mt-4'}>
        That is Claude Code&apos;s <C>local</C> scope: one entry in its <C>.claude.json</C>, private to you, in the
        directory you ran it in; <C>claude mcp remove swarmops -s local</C> takes it out again.{' '}
        <a className={LINK} href={MCP_GUIDE_URL}>
          MCP integration
        </a>{' '}
        has the rest.
      </p>
    </Section>
  );
}
