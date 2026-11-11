// The closing section: one sentence and the same primary action as the first screen.
import { H2, PARA, PrimaryAction, Section } from './parts';

export function Start() {
  return (
    <Section id="start" n={6} label="Start">
      <h2 id="start-title" className={H2}>
        Start here
      </h2>
      <p className={PARA + ' mt-4 mb-8'}>
        The quick start: a store, a commit, a land, a conflict and the op log.
      </p>
      <PrimaryAction className="max-sm:w-full" />
    </Section>
  );
}
