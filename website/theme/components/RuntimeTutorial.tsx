import { useState } from 'react';
import {
  InnerLine,
  Pre,
  type AnnotationHandler,
  type HighlightedCode,
} from 'codehike/code';
import {
  Selectable,
  SelectionProvider,
  useSelectedIndex,
} from 'codehike/utils/selection';
import runtimeTutorialData from '../generated/runtime-tutorial.json';

type Locale = 'zh' | 'en';

type Localized = {
  zh: string;
  en: string;
};

type RuntimeTutorialStep = {
  id: string;
  layer: string;
  filename: string;
  language: string;
  title: Localized;
  body: Localized;
  note: Localized;
  tags: string[];
  focus: [number, number];
  code: string;
  highlighted: HighlightedCode;
};

const runtimeTutorialSteps =
  runtimeTutorialData as unknown as RuntimeTutorialStep[];

const copy = {
  zh: {
    ariaLabel: 'A3S Box TypeScript SDK 代码示例',
    stageTitle: 'TypeScript SDK',
    step: '步骤',
    stepsTitle: '运行步骤',
    stepsHint: '滚动或点击',
    note: '说明',
    copy: '复制',
    copied: '已复制',
  },
  en: {
    ariaLabel: 'A3S Box TypeScript SDK example',
    stageTitle: 'TypeScript SDK',
    step: 'STEP',
    stepsTitle: 'RUN STEPS',
    stepsHint: 'scroll or click',
    note: 'NOTE',
    copy: 'Copy',
    copied: 'Copied',
  },
} as const;

function localeValue(value: Localized, locale: Locale) {
  return value[locale];
}

const runtimeFocusHandler: AnnotationHandler = {
  name: 'focus',
  onlyIfAnnotated: true,
  Line: (props) => (
    <InnerLine
      className="box-code-line"
      data-line-number={props.lineNumber}
      merge={props}
    />
  ),
  AnnotatedLine: ({ annotation: _annotation, ...props }) => (
    <InnerLine
      className="box-code-line is-focused"
      data-focus="true"
      data-line-number={props.lineNumber}
      merge={props}
    />
  ),
};

function TutorialCode({
  labels,
  step,
}: {
  labels: (typeof copy)[Locale];
  step: RuntimeTutorialStep;
}) {
  const [copied, setCopied] = useState(false);

  async function copyCode() {
    try {
      await navigator.clipboard.writeText(step.code);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1_400);
    } catch {
      setCopied(false);
    }
  }

  return (
    <div className="box-tutorial-code" data-tutorial-code={step.id}>
      <header>
        <span>
          <i aria-hidden="true" />
          {step.filename}
        </span>
        <span>{step.language.toUpperCase()}</span>
        <button onClick={copyCode} type="button">
          {copied ? labels.copied : labels.copy}
        </button>
      </header>
      <Pre
        code={step.highlighted}
        handlers={[runtimeFocusHandler]}
        key={step.id}
      />
    </div>
  );
}

function RuntimeStepRail({
  activeIndex,
  labels,
  locale,
  setActiveIndex,
}: {
  activeIndex: number;
  labels: (typeof copy)[Locale];
  locale: Locale;
  setActiveIndex: (index: number) => void;
}) {
  return (
    <div className="box-tutorial-rail">
      <header>
        <span>{labels.stepsTitle}</span>
        <small>{labels.stepsHint}</small>
      </header>
      <div>
        {runtimeTutorialSteps.map((step, index) => (
          <button
            aria-current={activeIndex === index ? 'step' : undefined}
            className={[
              activeIndex === index ? 'is-active' : '',
              activeIndex > index ? 'is-complete' : '',
            ]
              .filter(Boolean)
              .join(' ')}
            key={step.id}
            onClick={() => setActiveIndex(index)}
            type="button"
          >
            <span>{String(index + 1).padStart(2, '0')}</span>
            <span>
              <small>{step.layer.replace(/^\d+\s*\/\s*/, '')}</small>
              <strong>{localeValue(step.title, locale)}</strong>
            </span>
            <i aria-hidden="true" />
          </button>
        ))}
      </div>
    </div>
  );
}

function RuntimeTutorialStage({
  labels,
  locale,
}: {
  labels: (typeof copy)[Locale];
  locale: Locale;
}) {
  const [selectedIndex, setSelectedIndex] = useSelectedIndex();
  const activeIndex = Math.min(
    Math.max(selectedIndex, 0),
    runtimeTutorialSteps.length - 1,
  );
  const step = runtimeTutorialSteps[activeIndex];

  return (
    <div className="box-tutorial-stage">
      <div className="box-tutorial-stage-toolbar">
        <span>{labels.stageTitle}</span>
        <span aria-live="polite">
          {labels.step} {String(activeIndex + 1).padStart(2, '0')} /{' '}
          {String(runtimeTutorialSteps.length).padStart(2, '0')}
        </span>
      </div>
      <div className="box-tutorial-stage-grid">
        <TutorialCode labels={labels} step={step} />
        <div className="box-tutorial-stage-side">
          <RuntimeStepRail
            activeIndex={activeIndex}
            labels={labels}
            locale={locale}
            setActiveIndex={setSelectedIndex}
          />
          <div className="box-tutorial-note">
            <span>
              {labels.note} · {step.layer}
            </span>
            <p>{localeValue(step.note, locale)}</p>
          </div>
        </div>
      </div>
    </div>
  );
}

function RuntimeTutorialSteps({
  labels,
  locale,
}: {
  labels: (typeof copy)[Locale];
  locale: Locale;
}) {
  const [, setSelectedIndex] = useSelectedIndex();

  return (
    <div className="box-tutorial-steps">
      {runtimeTutorialSteps.map((step, index) => (
        <Selectable
          className="box-tutorial-step"
          data-tutorial-step={step.id}
          index={index}
          key={step.id}
          selectOn={['scroll']}
        >
          <button
            onClick={() => setSelectedIndex(index)}
            onFocus={() => setSelectedIndex(index)}
            onMouseEnter={() => setSelectedIndex(index)}
            type="button"
          >
            <span className="box-tutorial-step-number">
              {String(index + 1).padStart(2, '0')}
            </span>
            <span className="box-tutorial-step-layer">{step.layer}</span>
            <h3>{localeValue(step.title, locale)}</h3>
            <p>{localeValue(step.body, locale)}</p>
            <span className="box-tutorial-step-tags">
              {step.tags.map((tag) => (
                <i key={tag}>{tag}</i>
              ))}
            </span>
            <span className="box-tutorial-step-progress" aria-hidden="true" />
          </button>
          <div className="box-tutorial-mobile-preview">
            <div>
              <span>{labels.stepsTitle}</span>
              <strong>{localeValue(step.title, locale)}</strong>
            </div>
            <TutorialCode labels={labels} step={step} />
          </div>
        </Selectable>
      ))}
    </div>
  );
}

export function RuntimeTutorial({ locale }: { locale: Locale }) {
  const labels = copy[locale];

  return (
    <SelectionProvider
      aria-label={labels.ariaLabel}
      className="box-runtime-tutorial rp-not-doc"
      data-runtime-tutorial="true"
      rootMargin="-42% 0px -42% 0px"
    >
      <RuntimeTutorialSteps labels={labels} locale={locale} />
      <aside className="box-tutorial-sticky">
        <RuntimeTutorialStage labels={labels} locale={locale} />
      </aside>
    </SelectionProvider>
  );
}
