import { useCallback, useEffect, useRef, useState } from 'react';
import {
  runtimeTerminalInterfaceCopy,
  runtimeTerminalScenarios,
  type RuntimeTerminalLocale,
  type RuntimeTerminalScenario,
} from './runtime-terminal-content';

type TerminalPhase = 'typing' | 'output' | 'complete';

function ReplayIcon() {
  return (
    <svg aria-hidden="true" viewBox="0 0 20 20">
      <path d="M15.5 7.1A6 6 0 1 0 16 12" />
      <path d="M15.5 3.8v3.7h-3.7" />
    </svg>
  );
}

export function RuntimeTerminalShowcase({
  locale,
}: {
  locale: RuntimeTerminalLocale;
}) {
  const rootRef = useRef<HTMLElement>(null);
  const firstScenario = runtimeTerminalScenarios[0];
  const [activeIndex, setActiveIndex] = useState(0);
  const [phase, setPhase] = useState<TerminalPhase>('complete');
  const [typedCharacters, setTypedCharacters] = useState(
    firstScenario.command.length,
  );
  const [visibleLines, setVisibleLines] = useState(firstScenario.output.length);
  const [playing, setPlaying] = useState(true);
  const [inView, setInView] = useState(true);
  const [pageVisible, setPageVisible] = useState(true);
  const [reducedMotion, setReducedMotion] = useState(true);

  const activeScenario = runtimeTerminalScenarios[activeIndex];
  const ui = runtimeTerminalInterfaceCopy[locale];
  const canAnimate = playing && inView && pageVisible && !reducedMotion;

  const resetScenario = useCallback(
    (scenario: RuntimeTerminalScenario) => {
      if (reducedMotion) {
        setTypedCharacters(scenario.command.length);
        setVisibleLines(scenario.output.length);
        setPhase('complete');
        return;
      }

      setTypedCharacters(0);
      setVisibleLines(0);
      setPhase('typing');
    },
    [reducedMotion],
  );

  const restart = useCallback(() => {
    setPlaying(true);
    resetScenario(activeScenario);
  }, [activeScenario, resetScenario]);

  useEffect(() => {
    const query = window.matchMedia('(prefers-reduced-motion: reduce)');
    const update = () => setReducedMotion(query.matches);
    update();
    query.addEventListener('change', update);
    return () => query.removeEventListener('change', update);
  }, []);

  useEffect(() => {
    resetScenario(activeScenario);
  }, [activeScenario, locale, resetScenario]);

  useEffect(() => {
    const element = rootRef.current;
    if (!element || typeof IntersectionObserver === 'undefined') return;

    const observer = new IntersectionObserver(
      ([entry]) => setInView(entry?.isIntersecting ?? true),
      { rootMargin: '80px' },
    );
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    function updatePageVisibility() {
      setPageVisible(document.visibilityState !== 'hidden');
    }

    updatePageVisibility();
    document.addEventListener('visibilitychange', updatePageVisibility);
    return () =>
      document.removeEventListener('visibilitychange', updatePageVisibility);
  }, []);

  useEffect(() => {
    if (!canAnimate || phase !== 'typing') return;

    if (typedCharacters >= activeScenario.command.length) {
      const timeout = window.setTimeout(() => setPhase('output'), 220);
      return () => window.clearTimeout(timeout);
    }

    const timeout = window.setTimeout(
      () => setTypedCharacters((current) => current + 1),
      typedCharacters === 0 ? 280 : 23,
    );
    return () => window.clearTimeout(timeout);
  }, [activeScenario.command.length, canAnimate, phase, typedCharacters]);

  useEffect(() => {
    if (!canAnimate || phase !== 'output') return;

    if (visibleLines >= activeScenario.output.length) {
      const timeout = window.setTimeout(() => setPhase('complete'), 260);
      return () => window.clearTimeout(timeout);
    }

    const timeout = window.setTimeout(
      () => setVisibleLines((current) => current + 1),
      visibleLines === 0 ? 360 : 310,
    );
    return () => window.clearTimeout(timeout);
  }, [activeScenario.output.length, canAnimate, phase, visibleLines]);

  useEffect(() => {
    if (!canAnimate || phase !== 'complete') return;

    const timeout = window.setTimeout(() => {
      const nextIndex = (activeIndex + 1) % runtimeTerminalScenarios.length;
      setActiveIndex(nextIndex);
    }, 3600);
    return () => window.clearTimeout(timeout);
  }, [activeIndex, canAnimate, phase]);

  function selectScenario(index: number) {
    setPlaying(true);
    if (index === activeIndex) {
      resetScenario(runtimeTerminalScenarios[index]);
      return;
    }
    setActiveIndex(index);
  }

  const status = reducedMotion
    ? ui.ready
    : !playing
      ? ui.paused
      : phase === 'complete'
        ? ui.ready
        : ui.running;

  return (
    <section
      aria-label={ui.region}
      aria-describedby="box-terminal-assistive"
      className="box-terminal box-premium-surface"
      data-terminal-phase={phase}
      ref={rootRef}
    >
      <p className="box-visually-hidden" id="box-terminal-assistive">
        {activeScenario.command}. {activeScenario.summary[locale]}.
      </p>
      <header className="box-terminal-toolbar">
        <div aria-hidden="true">
          <i />
          <i />
          <i />
        </div>
        <strong>a3s-box · capability demo</strong>
        <span aria-live="polite">{status}</span>
        <button
          aria-label={playing ? ui.pause : ui.play}
          disabled={reducedMotion}
          onClick={() => setPlaying((current) => !current)}
          title={reducedMotion ? ui.reduced : playing ? ui.pause : ui.play}
          type="button"
        >
          <span aria-hidden="true">{playing ? 'Ⅱ' : '▶'}</span>
        </button>
      </header>

      <nav className="box-terminal-scenarios" aria-label={ui.scenario}>
        {runtimeTerminalScenarios.map((scenario, index) => (
          <button
            aria-label={`${scenario.label}: ${scenario.command}`}
            aria-pressed={index === activeIndex}
            className={index === activeIndex ? 'is-active' : undefined}
            data-terminal-scenario={scenario.id}
            key={scenario.id}
            onClick={() => selectScenario(index)}
            title={scenario.command}
            type="button"
          >
            <span>{String(index + 1).padStart(2, '0')}</span>
            {scenario.label}
          </button>
        ))}
      </nav>

      <div className="box-terminal-body" aria-hidden="true">
        <p className="box-terminal-prompt">
          <span>$</span>
          <code>{activeScenario.command.slice(0, typedCharacters)}</code>
          {phase === 'typing' && !reducedMotion ? (
            <i className="box-terminal-cursor" />
          ) : null}
        </p>
        <ol className="box-terminal-output">
          {activeScenario.output.map((line, index) => (
            <li
              className={[
                `is-${line.tone}`,
                index < visibleLines ? 'is-visible' : '',
              ]
                .filter(Boolean)
                .join(' ')}
              key={`${activeScenario.id}-${line.label.en}`}
            >
              <span>{line.label[locale]}</span>
              <code>{line.value[locale]}</code>
            </li>
          ))}
        </ol>
        <div
          className={[
            'box-terminal-summary',
            phase === 'complete' ? 'is-visible' : '',
          ]
            .filter(Boolean)
            .join(' ')}
        >
          <i>✓</i>
          <span>{activeScenario.summary[locale]}</span>
        </div>
      </div>

      <footer className="box-terminal-footer">
        <div>
          <b>{String(activeIndex + 1).padStart(2, '0')}</b>
          <span>/</span>
          <span>
            {String(runtimeTerminalScenarios.length).padStart(2, '0')}
          </span>
          <i />
          <code>{activeScenario.label}</code>
        </div>
        <button
          aria-label={ui.replayLabel}
          disabled={reducedMotion}
          onClick={restart}
          title={reducedMotion ? ui.reduced : ui.replayLabel}
          type="button"
        >
          <ReplayIcon />
          {ui.replay}
        </button>
      </footer>
    </section>
  );
}
