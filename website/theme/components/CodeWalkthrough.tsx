'use client';

import {
  Children,
  isValidElement,
  type KeyboardEvent,
  type ReactNode,
} from 'react';
import {
  Selection,
  Selectable,
  SelectionProvider,
} from 'codehike/utils/selection';

export function ScrollyCoding({
  children,
  ariaLabel,
}: {
  children: ReactNode;
  ariaLabel: string;
}) {
  const rootMargin =
    typeof window !== 'undefined' &&
    window.matchMedia('(max-width: 900px)').matches
      ? '-65% 0px -20% 0px'
      : '-20% 0px -55% 0px';

  return (
    <SelectionProvider
      className="box-code-walkthrough rp-not-doc"
      rootMargin={rootMargin}
      aria-label={ariaLabel}
    >
      {children}
    </SelectionProvider>
  );
}

export function ScrollySteps({ children }: { children: ReactNode }) {
  return <div className="box-code-walkthrough__steps">{children}</div>;
}

export function ScrollyStep({
  children,
  index,
}: {
  children: ReactNode;
  index: number;
}) {
  function selectWithKeyboard(event: KeyboardEvent<HTMLDivElement>) {
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      event.currentTarget.click();
    }
  }

  return (
    <Selectable
      index={index}
      selectOn={['click', 'scroll']}
      className="box-code-walkthrough__step"
      role="button"
      tabIndex={0}
      onKeyDown={selectWithKeyboard}
    >
      <span className="box-code-walkthrough__number">
        {String(index + 1).padStart(2, '0')}
      </span>
      <div>{children}</div>
    </Selectable>
  );
}

export function ScrollyCode({
  children,
  title,
}: {
  children: ReactNode;
  title: string;
}) {
  const snapshots = Children.toArray(children).filter(isValidElement);

  return (
    <div
      className="box-code-walkthrough__code"
      data-code-walkthrough="true"
      role="region"
      aria-label={title}
    >
      <header>
        <div aria-hidden="true">
          <span />
          <span />
          <span />
        </div>
        <strong>{title}</strong>
        <small>Code Hike</small>
      </header>
      <div className="box-code-walkthrough__viewport" aria-live="polite">
        <Selection from={snapshots} />
      </div>
      <footer>
        <span>token transitions</span>
        <span>{snapshots.length} snapshots</span>
      </footer>
    </div>
  );
}
