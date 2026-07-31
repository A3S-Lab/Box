import { Content, useLang, withBase } from '@rspress/core/runtime';
import { AgentSkillSection } from './AgentSkillSection';
import { BoxInstallSwitcher } from './BoxInstallSwitcher';
import { CanvasGridEffect } from './CanvasGridEffect';
import { PerformanceMetrics } from './PerformanceMetrics';
import { PremiumInteractions } from './PremiumInteractions';
import { RuntimeFeatureShowcase } from './RuntimeFeatureShowcase';
import { RuntimeTerminalShowcase } from './RuntimeTerminalShowcase';
import { homeContent, platformRows, sdkCards } from './home-content';

function ArrowIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true">
      <path d="M5 12h14M13 6l6 6-6 6" />
    </svg>
  );
}

function AnimatedButtonBorder() {
  return (
    <span aria-hidden="true" className="box-button-orbit">
      <span className="box-button-orbit-gradient" />
    </span>
  );
}

function GitHubIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true">
      <path
        d="M12 2.5a9.5 9.5 0 0 0-3 18.5c.5.1.65-.2.65-.48v-1.83c-2.73.6-3.3-1.16-3.3-1.16-.45-1.13-1.1-1.43-1.1-1.43-.9-.62.07-.6.07-.6 1 .07 1.52 1.02 1.52 1.02.88 1.52 2.32 1.08 2.89.82.09-.64.34-1.08.63-1.33-2.18-.25-4.47-1.09-4.47-4.84 0-1.07.38-1.94 1.02-2.63-.1-.25-.44-1.25.1-2.6 0 0 .83-.26 2.72 1a9.4 9.4 0 0 1 4.95 0c1.89-1.26 2.72-1 2.72-1 .54 1.35.2 2.35.1 2.6.63.69 1.02 1.56 1.02 2.63 0 3.76-2.3 4.59-4.48 4.83.36.31.67.92.67 1.86v2.66c0 .28.18.59.67.48A9.5 9.5 0 0 0 12 2.5Z"
        fill="currentColor"
        stroke="none"
      />
    </svg>
  );
}

export function HomeLayout() {
  const lang = useLang();
  const isChinese = lang.startsWith('zh');
  const locale = isChinese ? 'zh' : 'en';
  const copy = homeContent[locale];
  const languagePrefix = isChinese ? '' : '/en';
  const docLink = (href: string) => withBase(`${languagePrefix}${href}`);

  return (
    <main className="box-home">
      <PremiumInteractions />
      <div className="box-global-grid" aria-hidden="true">
        <CanvasGridEffect
          cellSize={54}
          className="box-global-grid-canvas"
          intensity={0.62}
          interactionScope="page"
        />
      </div>

      <section className="box-hero" aria-labelledby="box-hero-title">
        <div className="box-hero-copy">
          <div className="box-eyebrow">
            <span />
            {copy.eyebrow}
          </div>
          <h1 id="box-hero-title">
            {copy.heroLineOne}
            <span>{copy.heroLineTwo}</span>
          </h1>
          <p className="box-hero-subtitle">{copy.heroSubtitle}</p>
          <div className="box-hero-actions">
            <a
              className="box-button box-button--primary"
              href={docLink('/guide/quick-start.html')}
            >
              <AnimatedButtonBorder />
              {copy.getStarted}
              <ArrowIcon />
            </a>
            <a
              className="box-button box-button--secondary"
              href="https://github.com/A3S-Lab/Box"
              target="_blank"
              rel="noreferrer"
            >
              <GitHubIcon />
              {copy.viewSource}
            </a>
          </div>
          <BoxInstallSwitcher labels={copy.install} />
        </div>

        <div className="box-hero-visual">
          <RuntimeTerminalShowcase locale={locale} />
        </div>
      </section>

      <nav
        className="box-signal-strip"
        aria-label={isChinese ? '核心内容导航' : 'Core content navigation'}
      >
        {copy.signals.map((signal) => (
          <a href={signal.href} key={signal.title}>
            <strong>{signal.title}</strong>
            <span>{signal.detail}</span>
          </a>
        ))}
      </nav>

      <RuntimeFeatureShowcase
        locale={locale}
        platformHref={docLink('/reference/platforms.html')}
      />

      <PerformanceMetrics
        copy={copy.performance}
        reportHref={docLink('/reference/performance.html')}
      />

      <section
        className="box-section box-platforms"
        id="platform-support"
        aria-labelledby="platform-support-title"
      >
        <div className="box-section-heading box-section-heading--split">
          <div>
            <span>{copy.platformKicker}</span>
            <h2 id="platform-support-title">{copy.platformTitle}</h2>
          </div>
          <p>{copy.platformBody}</p>
        </div>
        <div className="box-platform-table">
          <div className="box-platform-row box-platform-row--header">
            {copy.platformHeaders.map((header) => (
              <span key={header}>{header}</span>
            ))}
          </div>
          {platformRows.map((row, index) => (
            <div className="box-platform-row" key={row.platform}>
              <strong>{row.platform}</strong>
              <code>{row.backend}</code>
              <span>{row.architecture}</span>
              <span>{copy.platformStatuses[index]}</span>
            </div>
          ))}
        </div>
        <a
          className="box-inline-link"
          href={docLink('/reference/platforms.html')}
        >
          {copy.platformLink}
          <ArrowIcon />
        </a>
      </section>

      <section
        className="box-section box-capabilities"
        id="runtime-capabilities"
        aria-labelledby="runtime-capabilities-title"
      >
        <div className="box-section-heading box-section-heading--split">
          <div>
            <span>{copy.capabilityKicker}</span>
            <h2 id="runtime-capabilities-title">{copy.capabilityTitle}</h2>
          </div>
          <p>{copy.capabilityBody}</p>
        </div>
        <div className="box-capability-grid">
          {copy.capabilities.map((card) => (
            <article
              key={card.index}
              className={`${card.className} box-premium-surface`}
            >
              <div className="box-card-index">{card.index}</div>
              <span>{card.eyebrow}</span>
              <h3>{card.title}</h3>
              <p>{card.body}</p>
              <div className="box-tags">
                {card.tags.map((tag) => (
                  <code key={tag}>{tag}</code>
                ))}
              </div>
            </article>
          ))}
        </div>
      </section>

      <section
        className="box-section box-sdks"
        id="native-sdks"
        aria-labelledby="native-sdks-title"
      >
        <div className="box-section-heading">
          <span>{copy.sdkKicker}</span>
          <h2 id="native-sdks-title">{copy.sdkTitle}</h2>
          <p>{copy.sdkBody}</p>
        </div>
        <div className="box-sdk-grid">
          {sdkCards.map((sdk) => (
            <a
              className="box-premium-surface"
              key={sdk.language}
              href={docLink(sdk.href)}
            >
              <header>
                <span>{sdk.language}</span>
                <ArrowIcon />
              </header>
              <strong>{sdk.packageName}</strong>
              <p>{copy.sdkNotes[sdk.id]}</p>
              <code>{sdk.command}</code>
            </a>
          ))}
        </div>
      </section>

      <section id="sdk-code-tour" className="box-section box-home-code-tour">
        <Content />
      </section>

      <AgentSkillSection
        guideHref={docLink('/guide/agent-skill.html')}
        locale={locale}
      />

      <section className="box-cta" id="home-cta">
        <div>
          <span>{copy.ctaKicker}</span>
          <h2>{copy.ctaTitle}</h2>
        </div>
        <div>
          <a
            className="box-button box-button--primary"
            href={docLink('/guide/quick-start.html')}
          >
            {copy.openQuickStart}
            <ArrowIcon />
          </a>
          <a
            className="box-button box-button--secondary"
            href="https://github.com/A3S-Lab/Box"
            target="_blank"
            rel="noreferrer"
          >
            <GitHubIcon />
            {copy.viewSource}
          </a>
        </div>
      </section>
    </main>
  );
}
