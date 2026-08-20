type IconProps = { size?: number };

const svgProps = (size: number) => ({
  width: size,
  height: size,
  viewBox: "0 0 24 24",
  fill: "currentColor",
  "aria-hidden": true as const,
});

export const ScreenShareIcon = ({ size = 24 }: IconProps) => (
  <svg {...svgProps(size)}>
    <path d="M4 4a2 2 0 0 0-2 2v9a2 2 0 0 0 2 2h6v2H7a1 1 0 1 0 0 2h10a1 1 0 1 0 0-2h-3v-2h6a2 2 0 0 0 2-2V6a2 2 0 0 0-2-2H4Zm8 2.5a1 1 0 0 1 .7.29l3.25 3.2a1 1 0 1 1-1.4 1.42L13 9.9v4.35a1 1 0 1 1-2 0V9.9l-1.55 1.51a1 1 0 1 1-1.4-1.42l3.25-3.2a1 1 0 0 1 .7-.29Z" />
  </svg>
);

export const SoundIcon = ({ size = 24 }: IconProps) => (
  <svg {...svgProps(size)}>
    <path d="M12 3.37a1 1 0 0 0-1.63-.77L6.15 6H4a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2.15l4.22 3.4A1 1 0 0 0 12 20.6V3.37Z" />
    <path d="M15.1 8.48a1 1 0 0 1 1.41.09 5.2 5.2 0 0 1 0 6.86 1 1 0 0 1-1.5-1.32 3.2 3.2 0 0 0 0-4.22 1 1 0 0 1 .09-1.41Z" />
    <path d="M17.7 5.75a1 1 0 0 1 1.41.05 9.1 9.1 0 0 1 0 12.4 1 1 0 1 1-1.46-1.36 7.1 7.1 0 0 0 0-9.68 1 1 0 0 1 .05-1.41Z" />
  </svg>
);

export const SoundMutedIcon = ({ size = 24 }: IconProps) => (
  <svg {...svgProps(size)}>
    <path d="M12 3.37a1 1 0 0 0-1.63-.77L6.15 6H4a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2.15l4.22 3.4A1 1 0 0 0 12 20.6V3.37Z" />
    <path d="M15.3 9.3a1 1 0 0 1 1.4 0l1.3 1.3 1.3-1.3a1 1 0 1 1 1.4 1.4L19.4 12l1.3 1.3a1 1 0 0 1-1.4 1.4L18 13.4l-1.3 1.3a1 1 0 0 1-1.4-1.4l1.3-1.3-1.3-1.3a1 1 0 0 1 0-1.4Z" />
  </svg>
);

export const FullscreenIcon = ({ size = 24 }: IconProps) => (
  <svg {...svgProps(size)}>
    <path d="M4 3a1 1 0 0 0-1 1v4a1 1 0 0 0 2 0V5h3a1 1 0 0 0 0-2H4Zm12 0a1 1 0 1 0 0 2h3v3a1 1 0 1 0 2 0V4a1 1 0 0 0-1-1h-4ZM5 16a1 1 0 1 0-2 0v4a1 1 0 0 0 1 1h4a1 1 0 1 0 0-2H5v-3Zm16 0a1 1 0 1 0-2 0v3h-3a1 1 0 1 0 0 2h4a1 1 0 0 0 1-1v-4Z" />
  </svg>
);

export const CollapseIcon = ({ size = 24 }: IconProps) => (
  <svg {...svgProps(size)}>
    <path d="M8 3a1 1 0 0 1 1 1v4a1 1 0 0 1-1 1H4a1 1 0 0 1 0-2h3V4a1 1 0 0 1 1-1Zm8 0a1 1 0 0 1 1 1v3h3a1 1 0 1 1 0 2h-4a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1ZM3 16a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v4a1 1 0 1 1-2 0v-3H4a1 1 0 0 1-1-1Zm12 0a1 1 0 0 1 1-1h4a1 1 0 1 1 0 2h-3v3a1 1 0 1 1-2 0v-4Z" />
  </svg>
);

export const EyeIcon = ({ size = 24 }: IconProps) => (
  <svg {...svgProps(size)}>
    <path d="M12 5c-4.9 0-8.6 3.6-10 6.5a1.2 1.2 0 0 0 0 1C3.4 15.4 7.1 19 12 19s8.6-3.6 10-6.5a1.2 1.2 0 0 0 0-1C20.6 8.6 16.9 5 12 5Zm0 10.5a3.5 3.5 0 1 1 0-7 3.5 3.5 0 0 1 0 7Z" />
  </svg>
);

export const EyeOffIcon = ({ size = 24 }: IconProps) => (
  <svg {...svgProps(size)}>
    <path d="M3.7 2.3a1 1 0 0 0-1.4 1.4l18 18a1 1 0 0 0 1.4-1.4l-2.6-2.6c1.3-1.1 2.3-2.4 2.9-3.7a1.2 1.2 0 0 0 0-1C20.6 8.6 16.9 5 12 5c-1.5 0-2.9.3-4.1.9L3.7 2.3Zm5.8 5.8a3.5 3.5 0 0 1 4.9 4.9L9.5 8.1Z" />
    <path d="M5.3 7.4C3.8 8.6 2.6 10.1 2 11.5a1.2 1.2 0 0 0 0 1C3.4 15.4 7.1 19 12 19c1.2 0 2.4-.2 3.4-.6l-2.1-2.1a3.5 3.5 0 0 1-4.6-4.6L5.3 7.4Z" />
  </svg>
);

export const StopIcon = ({ size = 24 }: IconProps) => (
  <svg {...svgProps(size)}>
    <path d="M5 7a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V7Z" />
  </svg>
);
