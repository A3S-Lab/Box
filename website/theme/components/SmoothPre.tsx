import React from 'react';
import { InnerPre, getPreRef } from 'codehike/code';
import type { CustomPreProps } from 'codehike/code';
import {
  calculateTransitions,
  getStartingSnapshot,
  type TokenTransitionsSnapshot,
} from 'codehike/utils/token-transitions';

const duration = 600;

export class SmoothPre extends React.Component<
  CustomPreProps,
  Record<string, never>,
  TokenTransitionsSnapshot
> {
  private readonly preRef: React.RefObject<HTMLPreElement>;

  constructor(props: CustomPreProps) {
    super(props);
    this.preRef = getPreRef(props);
  }

  render() {
    return <InnerPre merge={this.props} style={{ position: 'relative' }} />;
  }

  getSnapshotBeforeUpdate(): TokenTransitionsSnapshot {
    return this.preRef.current ? getStartingSnapshot(this.preRef.current) : [];
  }

  componentDidUpdate(
    _previousProps: Readonly<CustomPreProps>,
    _previousState: Readonly<Record<string, never>>,
    snapshot: TokenTransitionsSnapshot,
  ) {
    if (
      !this.preRef.current ||
      window.matchMedia('(prefers-reduced-motion: reduce)').matches
    ) {
      return;
    }

    for (const { element, keyframes, options } of calculateTransitions(
      this.preRef.current,
      snapshot,
    )) {
      const frames: Keyframe[] = [{}, {}];

      if (keyframes.translateX && keyframes.translateY) {
        frames[0].transform = `translate(${keyframes.translateX[0]}px, ${keyframes.translateY[0]}px)`;
        frames[1].transform = `translate(${keyframes.translateX[1]}px, ${keyframes.translateY[1]}px)`;
      }
      if (keyframes.color) {
        frames[0].color = keyframes.color[0];
        frames[1].color = keyframes.color[1];
      }
      if (keyframes.opacity) {
        frames[0].opacity = keyframes.opacity[0];
        frames[1].opacity = keyframes.opacity[1];
      }

      element.animate(frames, {
        delay: options.delay * duration,
        duration: options.duration * duration,
        easing: options.easing,
        fill: options.fill,
      });
    }
  }
}
