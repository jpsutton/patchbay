import type { SVGProps } from "react";

/** Fill for the knockout ring a badge dot carries to separate itself from what it sits on. */
const RING_FILL = { panel: "fill-panel", bg: "fill-bg" } as const;

type DotProps = Omit<SVGProps<SVGSVGElement>, "width" | "height" | "viewBox" | "children"> & {
  /** Diameter in CSS pixels. */
  size?: number;
  /** Draws an outline of this stroke width instead of a filled disc. */
  hollow?: number;
  /** A soft same-color halo of this width around the disc. */
  halo?: number;
  /** A 2px knockout ring in this surface color around the disc. */
  ring?: keyof typeof RING_FILL;
  /** Tooltip text. */
  title?: string;
};

/**
 * A status light. The color comes from `text-*` (currentColor).
 *
 * Drawn as an SVG circle on purpose: a `rounded-full` box has its edges snapped to the pixel grid, so
 * at a fractional position (a flex row with text-sized siblings) one dimension rounds differently
 * from the other and the dot looks oblong. A vector circle stays round at any position.
 */
export function Dot({ size = 8, hollow, halo, ring, title, className = "", ...rest }: DotProps) {
  const c = size / 2;
  return (
    <svg
      width={size}
      height={size}
      viewBox={`0 0 ${size} ${size}`}
      className={`inline-block shrink-0 overflow-visible ${className}`}
      aria-hidden={rest["aria-label"] || title ? undefined : true}
      {...rest}
    >
      {title && <title>{title}</title>}
      {ring && <circle cx={c} cy={c} r={c + 2} className={RING_FILL[ring]} />}
      {halo ? <circle cx={c} cy={c} r={c + halo} fill="currentColor" fillOpacity={0.16} /> : null}
      {hollow ? (
        <circle cx={c} cy={c} r={c - hollow / 2} fill="none" stroke="currentColor" strokeWidth={hollow} />
      ) : (
        <circle cx={c} cy={c} r={c} fill="currentColor" />
      )}
    </svg>
  );
}
