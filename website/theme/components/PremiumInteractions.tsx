import { useEffect, useRef } from 'react';

type PointerPosition = {
  clientX: number;
  clientY: number;
  target: EventTarget | null;
};

/**
 * Coordinates pointer lighting for Box surfaces. Layout and presentation stay
 * in CSS; this component only publishes local pointer coordinates.
 */
export function PremiumInteractions() {
  const anchorRef = useRef<HTMLSpanElement>(null);

  useEffect(() => {
    const host = anchorRef.current?.closest<HTMLElement>('.box-home');
    if (!host) return undefined;

    const hero = host.querySelector<HTMLElement>('.box-hero');
    const motionPreference = window.matchMedia(
      '(prefers-reduced-motion: reduce)',
    );
    let activeSurface: HTMLElement | null = null;
    let animationFrame = 0;
    let latestPointer: PointerPosition | null = null;

    const clearActiveSurface = () => {
      activeSurface?.classList.remove('is-pointer-active');
      activeSurface = null;
    };

    const paintPointer = () => {
      animationFrame = 0;
      const pointer = latestPointer;
      if (!pointer || motionPreference.matches) {
        clearActiveSurface();
        return;
      }

      const target =
        pointer.target instanceof Element ? pointer.target : undefined;
      const surface =
        target?.closest<HTMLElement>('.box-premium-surface') ?? null;

      if (surface && host.contains(surface)) {
        if (surface !== activeSurface) {
          clearActiveSurface();
          activeSurface = surface;
          surface.classList.add('is-pointer-active');
        }

        const bounds = surface.getBoundingClientRect();
        surface.style.setProperty(
          '--box-spot-x',
          `${pointer.clientX - bounds.left}px`,
        );
        surface.style.setProperty(
          '--box-spot-y',
          `${pointer.clientY - bounds.top}px`,
        );
      } else {
        clearActiveSurface();
      }

      if (hero && target && hero.contains(target)) {
        const bounds = hero.getBoundingClientRect();
        const x = ((pointer.clientX - bounds.left) / bounds.width) * 100;
        const y = ((pointer.clientY - bounds.top) / bounds.height) * 100;
        hero.style.setProperty(
          '--box-hero-x',
          `${Math.max(0, Math.min(x, 100))}%`,
        );
        hero.style.setProperty(
          '--box-hero-y',
          `${Math.max(0, Math.min(y, 100))}%`,
        );
      } else {
        hero?.style.removeProperty('--box-hero-x');
        hero?.style.removeProperty('--box-hero-y');
      }
    };

    const handlePointerMove = (event: PointerEvent) => {
      if (event.pointerType === 'touch') return;
      latestPointer = {
        clientX: event.clientX,
        clientY: event.clientY,
        target: event.target,
      };
      if (!animationFrame) {
        animationFrame = window.requestAnimationFrame(paintPointer);
      }
    };

    const handlePointerLeave = () => {
      latestPointer = null;
      clearActiveSurface();
      hero?.style.removeProperty('--box-hero-x');
      hero?.style.removeProperty('--box-hero-y');
    };

    const handleMotionChange = () => {
      if (motionPreference.matches) handlePointerLeave();
    };

    host.dataset.premiumEffects = 'ready';
    host.addEventListener('pointermove', handlePointerMove);
    host.addEventListener('pointerleave', handlePointerLeave);
    motionPreference.addEventListener('change', handleMotionChange);

    return () => {
      window.cancelAnimationFrame(animationFrame);
      clearActiveSurface();
      delete host.dataset.premiumEffects;
      host.removeEventListener('pointermove', handlePointerMove);
      host.removeEventListener('pointerleave', handlePointerLeave);
      motionPreference.removeEventListener('change', handleMotionChange);
    };
  }, []);

  return (
    <span
      aria-hidden="true"
      className="box-premium-effects-anchor"
      ref={anchorRef}
    />
  );
}
