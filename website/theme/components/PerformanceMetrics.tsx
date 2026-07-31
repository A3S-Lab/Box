import { useEffect, useRef } from 'react';

type PerformanceMetric = {
  id: string;
  value: string;
  unit: string;
  label: string;
  detail: string;
};

type PerformanceCopy = {
  kicker: string;
  title: string;
  body: string;
  badge: string;
  host: string;
  method: string;
  percentile: string;
  metrics: readonly PerformanceMetric[];
  caution: string;
  report: string;
};

function ExternalArrowIcon() {
  return (
    <svg aria-hidden="true" viewBox="0 0 16 16">
      <path d="M4 12 12 4M6 4h6v6" />
    </svg>
  );
}

function AnimatedMetricValue({ value }: { value: string }) {
  const valueRef = useRef<HTMLSpanElement>(null);

  useEffect(() => {
    const element = valueRef.current;
    const numericValue = Number(value.replace(/,/g, ''));

    if (!element || !Number.isFinite(numericValue)) {
      return;
    }

    const reducedMotion = window.matchMedia(
      '(prefers-reduced-motion: reduce)',
    ).matches;
    if (reducedMotion || typeof IntersectionObserver === 'undefined') {
      element.dataset.animationState = 'complete';
      return;
    }

    const fractionDigits = value.split('.')[1]?.length ?? 0;
    const formatter = new Intl.NumberFormat('en-US', {
      minimumFractionDigits: fractionDigits,
      maximumFractionDigits: fractionDigits,
    });
    let animationFrame = 0;
    let hasStarted = false;

    element.textContent = formatter.format(0);
    element.dataset.animationState = 'pending';

    const observer = new IntersectionObserver(
      ([entry]) => {
        if (!entry?.isIntersecting || hasStarted) {
          return;
        }

        hasStarted = true;
        element.dataset.animationState = 'running';
        observer.disconnect();
        const startedAt = performance.now();
        const duration = 1200;

        const renderFrame = (now: number) => {
          const progress = Math.min((now - startedAt) / duration, 1);
          const easedProgress = 1 - Math.pow(1 - progress, 4);
          element.textContent = formatter.format(numericValue * easedProgress);

          if (progress < 1) {
            animationFrame = window.requestAnimationFrame(renderFrame);
            return;
          }

          element.textContent = value;
          element.dataset.animationState = 'complete';
        };

        animationFrame = window.requestAnimationFrame(renderFrame);
      },
      { threshold: 0.35 },
    );

    observer.observe(element);

    return () => {
      observer.disconnect();
      window.cancelAnimationFrame(animationFrame);
    };
  }, [value]);

  return (
    <strong
      aria-label={value}
      className="box-performance-value-number"
      data-metric-value={value}
    >
      <span aria-hidden="true" className="box-performance-value-measure">
        {value}
      </span>
      <span
        aria-hidden="true"
        className="box-performance-value-animated"
        ref={valueRef}
      >
        {value}
      </span>
    </strong>
  );
}

export function PerformanceMetrics({
  copy,
  reportHref,
}: {
  copy: PerformanceCopy;
  reportHref: string;
}) {
  return (
    <section
      className="box-section box-performance"
      id="performance-benchmarks"
      aria-labelledby="performance-benchmarks-title"
    >
      <div className="box-performance-heading">
        <div>
          <span>{copy.kicker}</span>
          <h2 id="performance-benchmarks-title">{copy.title}</h2>
          <p>{copy.body}</p>
        </div>
        <div className="box-performance-context" aria-label={copy.badge}>
          <strong>
            <i aria-hidden="true" />
            {copy.badge}
          </strong>
          <span>{copy.host}</span>
          <span>{copy.method}</span>
        </div>
      </div>

      <div className="box-performance-grid" role="list">
        {copy.metrics.map((metric, index) => (
          <article
            className="box-performance-metric box-premium-surface"
            data-performance-metric={metric.id}
            key={metric.id}
            role="listitem"
          >
            <header>
              <span>{String(index + 1).padStart(2, '0')}</span>
              <span>{copy.percentile}</span>
            </header>
            <p className="box-performance-value">
              <AnimatedMetricValue value={metric.value} />
              <span>{metric.unit}</span>
            </p>
            <h3>{metric.label}</h3>
            <p className="box-performance-detail">{metric.detail}</p>
          </article>
        ))}
      </div>

      <footer className="box-performance-footer">
        <p>
          <i aria-hidden="true" />
          {copy.caution}
        </p>
        <a href={reportHref}>
          {copy.report}
          <ExternalArrowIcon />
        </a>
      </footer>
    </section>
  );
}
