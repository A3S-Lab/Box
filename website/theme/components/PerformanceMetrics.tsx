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
              <strong>{metric.value}</strong>
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
