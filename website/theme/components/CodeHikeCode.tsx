import { InnerToken, Pre } from 'codehike/code';
import type { AnnotationHandler, HighlightedCode } from 'codehike/code';
import { SmoothPre } from './SmoothPre';

const tokenTransitions: AnnotationHandler = {
  name: 'token-transitions',
  PreWithRef: SmoothPre,
  Token: (props) => (
    <InnerToken merge={props} style={{ display: 'inline-block' }} />
  ),
};

interface CodeHikeCodeProps {
  codeblock: HighlightedCode;
}

export default function CodeHikeCode({ codeblock }: CodeHikeCodeProps) {
  const title = codeblock.meta.replace(/\bwalkthrough\b/, '').trim();

  return (
    <div
      className="box-ch-codeblock"
      data-codehike="true"
      data-language={codeblock.lang}
    >
      {title ? <div className="box-ch-codeblock__title">{title}</div> : null}
      <Pre
        code={codeblock}
        handlers={[tokenTransitions]}
        className="box-ch-codeblock__pre"
      />
    </div>
  );
}
