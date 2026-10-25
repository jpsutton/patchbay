import type { ReactNode, SVGProps } from "react";

/**
 * Line icons drawn on Lucide's 24-unit grid (ISC license).
 *
 * The viewBox crops Lucide's 2-unit margin, so at 20px one unit is exactly one screen pixel and
 * 2-unit strokes land on whole pixels: crisp on 1x displays, where Lucide's usual 16px rendering
 * smears every line across two pixel rows. Strokes may poke past the crop by a pixel, hence
 * `overflow: visible`.
 *
 * Toolbar icons use the standard 18px size, which would scale those lines off the pixel grid.
 * They pass a second drawing, redrawn by hand on an 18-unit grid with 2-unit strokes on whole
 * coordinates, and render it 1:1 at 18px. Any other size scales the 24-grid drawing.
 */
type IconProps = SVGProps<SVGSVGElement> & { size?: number | string };

function icon(paths: ReactNode, grid18?: ReactNode) {
  return function Icon({ size = 18, strokeWidth = 2, ...rest }: IconProps) {
    const snapped = grid18 !== undefined && Number(size) === 18;
    return (
      <svg
        xmlns="http://www.w3.org/2000/svg"
        width={size}
        height={size}
        viewBox={snapped ? "0 0 18 18" : "2 2 20 20"}
        overflow="visible"
        fill="none"
        stroke="currentColor"
        strokeWidth={strokeWidth}
        strokeLinecap="round"
        strokeLinejoin="round"
        aria-hidden="true"
        {...rest}
      >
        {snapped ? grid18 : paths}
      </svg>
    );
  };
}

export const MenuIcon = icon(
  <>
    <path d="M4 6h16" />
    <path d="M4 12h12" />
    <path d="M4 18h8" />
  </>,
  <>
    <path d="M2 4h14" />
    <path d="M2 9h10" />
    <path d="M2 14h7" />
  </>,
);

/** A session list: three rows with bullets. */
export const ListIcon = icon(
  <>
    <path d="M8 6h13" />
    <path d="M8 12h13" />
    <path d="M8 18h13" />
    <path d="M3 6h.01" />
    <path d="M3 12h.01" />
    <path d="M3 18h.01" />
  </>,
);

export const PanelLeftIcon = icon(
  <>
    <rect x="3" y="3" width="18" height="18" rx="2" />
    <path d="M9 3v18" />
  </>,
  <>
    <rect x="1" y="1" width="16" height="16" rx="2" />
    <path d="M6 1v16" />
  </>,
);

export const ArrowLeftIcon = icon(
  <>
    <path d="m12 19-7-7 7-7" />
    <path d="M19 12H5" />
  </>,
  <>
    <path d="M9 15 3 9l6-6" />
    <path d="M15 9H3" />
  </>,
);

export const ArrowRightIcon = icon(
  <>
    <path d="M5 12h14" />
    <path d="m12 5 7 7-7 7" />
  </>,
  <>
    <path d="M3 9h12" />
    <path d="m9 3 6 6-6 6" />
  </>,
);

export const ChatIcon = icon(
  <path d="M7.9 20A9 9 0 1 0 4 16.1L2 22Z" />,
);

export const ChatsIcon = icon(
  <>
    <path d="M14 9a2 2 0 0 1-2 2H6l-4 4V4a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2z" />
    <path d="M18 9h2a2 2 0 0 1 2 2v11l-4-4h-6a2 2 0 0 1-2-2v-1" />
  </>,
  <>
    <path d="M11 6a2 2 0 0 1-2 2H5l-4 4V3a2 2 0 0 1 2-2h6a2 2 0 0 1 2 2z" />
    <path d="M15 6a2 2 0 0 1 2 2v9l-4-4H9a2 2 0 0 1-2-2" />
  </>,
);

export const CodeIcon = icon(
  <>
    <path d="m18 16 4-4-4-4" />
    <path d="m6 8-4 4 4 4" />
    <path d="m14.5 4-5 16" />
  </>,
  <>
    <path d="m13 5 4 4-4 4" />
    <path d="M5 5 1 9l4 4" />
    <path d="M11 2 7 16" />
  </>,
);

export const PlusIcon = icon(
  <>
    <path d="M5 12h14" />
    <path d="M12 5v14" />
  </>,
);

export const PlusCircleIcon = icon(
  <>
    <circle cx="12" cy="12" r="10" />
    <path d="M8 12h8" />
    <path d="M12 8v8" />
  </>,
  <>
    <circle cx="9" cy="9" r="8" />
    <path d="M6 9h6" />
    <path d="M9 6v6" />
  </>,
);

export const FolderPlusIcon = icon(
  <>
    <path d="M12 10v6" />
    <path d="M9 13h6" />
    <path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z" />
  </>,
);

export const SearchIcon = icon(
  <>
    <circle cx="11" cy="11" r="8" />
    <path d="m21 21-4.3-4.3" />
  </>,
  <>
    <circle cx="8" cy="8" r="7" />
    <path d="m17 17-4.05-4.05" />
  </>,
);

export const SettingsIcon = icon(
  <>
    <path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z" />
    <circle cx="12" cy="12" r="3" />
  </>,
  <>
    <path d="M9.18 1h-.35a1.6 1.6 0 0 0-1.6 1.6v.14a1.6 1.6 0 0 1-.8 1.38l-.34 .2a1.6 1.6 0 0 1-1.6 0l-.12-.06a1.6 1.6 0 0 0-2.18 .58l-.18 .3a1.6 1.6 0 0 0 .58 2.18l.12 .08a1.6 1.6 0 0 1 .8 1.38v.41a1.6 1.6 0 0 1-.8 1.39l-.12 .07a1.6 1.6 0 0 0-.58 2.18l.18 .3a1.6 1.6 0 0 0 2.18 .58l.12-.06a1.6 1.6 0 0 1 1.6 0l.34 .2a1.6 1.6 0 0 1 .8 1.38V15.4a1.6 1.6 0 0 0 1.6 1.6h.35a1.6 1.6 0 0 0 1.6-1.6v-.14a1.6 1.6 0 0 1 .8-1.38l.34-.2a1.6 1.6 0 0 1 1.6 0l.12 .06a1.6 1.6 0 0 0 2.18-.58l.18-.31a1.6 1.6 0 0 0-.58-2.18l-.12-.06a1.6 1.6 0 0 1-.8-1.39v-.4a1.6 1.6 0 0 1 .8-1.39l.12-.07a1.6 1.6 0 0 0 .58-2.18l-.18-.3a1.6 1.6 0 0 0-2.18-.58l-.12 .06a1.6 1.6 0 0 1-1.6 0l-.34-.2a1.6 1.6 0 0 1-.8-1.38V2.6a1.6 1.6 0 0 0-1.6-1.6z" />
    <circle cx="9" cy="9" r="2.4" />
  </>,
);

export const GaugeIcon = icon(
  <>
    <path d="m12 14 4-4" />
    <path d="M3.34 19a10 10 0 1 1 17.32 0" />
  </>,
);

export const KeyboardIcon = icon(
  <>
    <path d="M10 8h.01" />
    <path d="M12 12h.01" />
    <path d="M14 8h.01" />
    <path d="M16 12h.01" />
    <path d="M18 8h.01" />
    <path d="M6 8h.01" />
    <path d="M7 16h10" />
    <path d="M8 12h.01" />
    <rect width="20" height="16" x="2" y="4" rx="2" />
  </>,
  <>
    <path d="M4 6h.01" />
    <path d="M7 6h.01" />
    <path d="M11 6h.01" />
    <path d="M14 6h.01" />
    <path d="M6 9h.01" />
    <path d="M9 9h.01" />
    <path d="M12 9h.01" />
    <path d="M5 12h8" />
    <rect width="16" height="14" x="1" y="2" rx="2" />
  </>,
);

export const GitBranchIcon = icon(
  <>
    <path d="M6 3v12" />
    <circle cx="18" cy="6" r="3" />
    <circle cx="6" cy="18" r="3" />
    <path d="M18 9a9 9 0 0 1-9 9" />
  </>,
);

export const ChevronRightIcon = icon(<path d="m9 18 6-6-6-6" />);

export const StarIcon = icon(
  <path d="M11.525 2.295a.53.53 0 0 1 .95 0l2.31 4.679a2.123 2.123 0 0 0 1.595 1.16l5.166.756a.53.53 0 0 1 .294.904l-3.736 3.638a2.123 2.123 0 0 0-.611 1.878l.882 5.14a.53.53 0 0 1-.771.56l-4.618-2.428a2.122 2.122 0 0 0-1.973 0L6.396 21.01a.53.53 0 0 1-.77-.56l.881-5.139a2.122 2.122 0 0 0-.611-1.879L2.16 9.795a.53.53 0 0 1 .294-.906l5.165-.755a2.122 2.122 0 0 0 1.597-1.16z" />,
);

export const XIcon = icon(
  <>
    <path d="M18 6 6 18" />
    <path d="m6 6 12 12" />
  </>,
);

export const GitPullRequestIcon = icon(
  <>
    <circle cx="18" cy="18" r="3" />
    <circle cx="6" cy="6" r="3" />
    <path d="M13 6h3a2 2 0 0 1 2 2v7" />
    <path d="M6 9v12" />
  </>,
);

export const GitCommitIcon = icon(
  <>
    <circle cx="12" cy="12" r="3" />
    <path d="M3 12h6" />
    <path d="M15 12h6" />
  </>,
);

export const FileDiffIcon = icon(
  <>
    <path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z" />
    <path d="M14 2v4a2 2 0 0 0 2 2h4" />
    <path d="M9 10h6" />
    <path d="M12 7v6" />
    <path d="M9 17h6" />
  </>,
  <>
    <path d="M12 1H4a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V5Z" />
    <path d="M11 1v2a2 2 0 0 0 2 2h3" />
    <path d="M7 7h4" />
    <path d="M9 5v4" />
    <path d="M7 12h4" />
  </>,
);

export const MessageCodeIcon = icon(
  <>
    <path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" />
    <path d="m10 8-2 2 2 2" />
    <path d="m14 8 2 2-2 2" />
  </>,
);

// Window controls: kept small inside the 20px box, on whole-pixel coordinates.
export const MinimizeIcon = icon(<path d="M7 12h10" />, <path d="M5 9h8" />);

export const MaximizeIcon = icon(<rect x="7" y="7" width="10" height="10" rx="1" />, <rect x="5" y="5" width="8" height="8" rx="1" />);

export const RestoreIcon = icon(
  <>
    <rect x="7" y="9" width="8" height="8" rx="1" />
    <path d="M9 9V8a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v6a1 1 0 0 1-1 1h-1" />
  </>,
  <>
    <rect x="5" y="7" width="6" height="6" rx="1" />
    <path d="M7 7V6a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v4a1 1 0 0 1-1 1h-1" />
  </>,
);

export const CloseIcon = icon(
  <>
    <path d="M17 7 7 17" />
    <path d="m7 7 10 10" />
  </>,
  <>
    <path d="M13 5 5 13" />
    <path d="m5 5 8 8" />
  </>,
);

export const DownloadIcon = icon(
  <>
    <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
    <path d="m7 10 5 5 5-5" />
    <path d="M12 15V3" />
  </>,
  <>
    <path d="M17 12v3a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2v-3" />
    <path d="m5 8 4 4 4-4" />
    <path d="M9 12V1" />
  </>,
);

export const TerminalIcon = icon(
  <>
    <path d="m7 11 2-2-2-2" />
    <path d="M11 13h4" />
    <rect x="3" y="3" width="18" height="18" rx="2" />
  </>,
  <>
    <path d="m5 5 3 3-3 3" />
    <path d="M10 11h3" />
    <rect x="1" y="1" width="16" height="16" rx="2" />
  </>,
);

export const WidthIcon = icon(
  <>
    <path d="m18 8 4 4-4 4" />
    <path d="M2 12h20" />
    <path d="m6 8-4 4 4 4" />
  </>,
  <>
    <path d="m14 5 4 4-4 4" />
    <path d="M0 9h18" />
    <path d="m4 5-4 4 4 4" />
  </>,
);

export const RestartIcon = icon(
  <>
    <path d="M21 12a9 9 0 1 1-9-9c2.52 0 4.93 1 6.74 2.74L21 8" />
    <path d="M21 3v5h-5" />
  </>,
  <>
    <path d="M17 9a8 8 0 1 1-8-8c2.24 0 4.38.89 5.99 2.44L17 5" />
    <path d="M17 1v4h-4" />
  </>,
);

export const ScrollTextIcon = icon(
  <>
    <path d="M15 12h-5" />
    <path d="M15 8h-5" />
    <path d="M19 17V5a2 2 0 0 0-2-2H4" />
    <path d="M8 21h12a2 2 0 0 0 2-2v-1a1 1 0 0 0-1-1H11a1 1 0 0 0-1 1v1a2 2 0 1 1-4 0V5a2 2 0 1 0-4 0v2a1 1 0 0 0 1 1h3" />
  </>,
  <>
    <path d="M12 9H8" />
    <path d="M12 5H8" />
    <path d="M15 13V3a2 2 0 0 0-2-2H3" />
    <path d="M7 17h8a2 2 0 0 0 2-2v-1a1 1 0 0 0-1-1H10a1 1 0 0 0-1 1v1a2 2 0 1 1-4 0V3a2 2 0 1 0-4 0v1a1 1 0 0 0 1 1h3" />
  </>,
);

export const PaperclipIcon = icon(
  <path d="m16 6-8.414 8.586a2 2 0 0 0 2.829 2.829l8.414-8.586a4 4 0 1 0-5.657-5.657l-8.379 8.551a6 6 0 1 0 8.485 8.485l8.379-8.551" />,
);

export const ArrowUpIcon = icon(
  <>
    <path d="m5 12 7-7 7 7" />
    <path d="M12 19V5" />
  </>,
);

export const ChevronDownIcon = icon(<path d="m6 9 6 6 6-6" />);

export const ChevronLeftIcon = icon(<path d="m15 18-6-6 6-6" />);

export const CopyIcon = icon(
  <>
    <rect width="14" height="14" x="8" y="8" rx="2" ry="2" />
    <path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2" />
  </>,
);

export const InfoIcon = icon(
  <>
    <circle cx="12" cy="12" r="10" />
    <path d="M12 16v-4" />
    <path d="M12 8h.01" />
  </>,
);

export const CheckIcon = icon(<path d="M20 6 9 17l-5-5" />);

export const PencilIcon = icon(
  <>
    <path d="M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z" />
    <path d="m15 5 4 4" />
  </>,
);

export const SlidersIcon = icon(
  <>
    <path d="M14 17H5" />
    <path d="M19 7h-9" />
    <circle cx="17" cy="17" r="3" />
    <circle cx="7" cy="7" r="3" />
  </>,
);

export const ActivityIcon = icon(
  <path d="M22 12h-2.48a2 2 0 0 0-1.93 1.46l-2.35 8.36a.25.25 0 0 1-.48 0L9.24 2.18a.25.25 0 0 0-.48 0l-2.35 8.36A2 2 0 0 1 4.49 12H2" />,
);

export const FileTextIcon = icon(
  <>
    <path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z" />
    <path d="M14 2v4a2 2 0 0 0 2 2h4" />
    <path d="M10 9H8" />
    <path d="M16 13H8" />
    <path d="M16 17H8" />
  </>,
);

export const TrashIcon = icon(
  <>
    <path d="M3 6h18" />
    <path d="M19 6v14c0 1-1 2-2 2H7c-1 0-2-1-2-2V6" />
    <path d="M8 6V4c0-1 1-2 2-2h4c1 0 2 1 2 2v2" />
  </>,
);

export const ArchiveIcon = icon(
  <>
    <rect x="2" y="3" width="20" height="5" rx="1" />
    <path d="M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8" />
    <path d="M10 12h4" />
  </>,
);

export const MoveIcon = icon(
  <>
    <path d="M8 3 4 7l4 4" />
    <path d="M4 7h16" />
    <path d="m16 21 4-4-4-4" />
    <path d="M20 17H4" />
  </>,
);

export const GlobeIcon = icon(
  <>
    <circle cx="12" cy="12" r="10" />
    <path d="M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20" />
    <path d="M2 12h20" />
  </>,
);

export const BrainIcon = icon(
  <>
    <path d="M12 18V5" />
    <path d="M15 13a4.17 4.17 0 0 1-3-4 4.17 4.17 0 0 1-3 4" />
    <path d="M17.598 6.5A3 3 0 1 0 12 5a3 3 0 1 0-5.598 1.5" />
    <path d="M17.997 5.125a4 4 0 0 1 2.526 5.77" />
    <path d="M18 18a4 4 0 0 0 2-7.464" />
    <path d="M19.967 17.483A4 4 0 1 1 12 18a4 4 0 1 1-7.967-.517" />
    <path d="M6 18a4 4 0 0 1-2-7.464" />
    <path d="M6.003 5.125a4 4 0 0 0-2.526 5.77" />
  </>,
);

export const DotIcon = icon(<circle cx="12" cy="12" r="1.5" />);

export const GripVerticalIcon = icon(
  <>
    <circle cx="9" cy="6" r="1" />
    <circle cx="9" cy="12" r="1" />
    <circle cx="9" cy="18" r="1" />
    <circle cx="15" cy="6" r="1" />
    <circle cx="15" cy="12" r="1" />
    <circle cx="15" cy="18" r="1" />
  </>,
);

export const MonitorIcon = icon(
  <>
    <rect width="20" height="14" x="2" y="3" rx="2" />
    <path d="M8 21h8" />
    <path d="M12 17v4" />
  </>,
);

export const SpeakerIcon = icon(
  <>
    <path d="M11 5 6 9H2v6h4l5 4V5z" />
    <path d="M15.54 8.46a5 5 0 0 1 0 7.07" />
    <path d="M19.07 4.93a10 10 0 0 1 0 14.14" />
  </>,
);

export const StopIcon = icon(<rect x="6" y="6" width="12" height="12" rx="1" fill="currentColor" stroke="none" />);

export const MonitorUpIcon = icon(
  <>
    <path d="m9 10 3-3 3 3" />
    <path d="M12 13V7" />
    <rect width="20" height="14" x="2" y="3" rx="2" />
    <path d="M12 17v4" />
    <path d="M8 21h8" />
  </>,
  <>
    <path d="m6 7 3-3 3 3" />
    <path d="M9 10V4" />
    <rect width="16" height="12" x="1" y="1" rx="2" />
    <path d="M9 13v4" />
    <path d="M5 17h8" />
  </>,
);

export const TriangleAlertIcon = icon(
  <>
    <path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3" />
    <path d="M12 9v4" />
    <path d="M12 17h.01" />
  </>,
);

/** Three linked computers: the mesh. */
export const MeshIcon = icon(
  <>
    <circle cx="5" cy="6" r="2" />
    <circle cx="19" cy="6" r="2" />
    <circle cx="12" cy="18" r="2" />
    <path d="M7 6h10" />
    <path d="m6 8 5 8.3" />
    <path d="m18 8-5 8.3" />
  </>,
);
