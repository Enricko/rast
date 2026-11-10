// Original RAST icon set (inline SVG, 24x24 grid, stroke based).

const PATHS: Record<string, string> = {
  logo: `<path d="M12 2.6 20.4 7v10L12 21.4 3.6 17V7z" fill="none" stroke="currentColor" stroke-width="1.7"/><path d="M12 7.2 15.8 9.4v4.4L12 16l-3.8-2.2V9.4z" fill="currentColor"/>`,
  files: `<path d="M3.5 6.2h5.1l1.8 2.3h10.1v9.9a1.6 1.6 0 0 1-1.6 1.6H3.5z" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linejoin="round"/><path d="M3.5 12.6h17" stroke="currentColor" stroke-width="1.3" opacity=".55"/>`,
  folder: `<path d="M3.5 6.2h5.1l1.8 2.3h10.1v9.9a1.6 1.6 0 0 1-1.6 1.6H3.5z" fill="currentColor" opacity=".85"/>`,
  file: `<path d="M6.2 2.8h8.2l4.2 4.2v14.2H6.2z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><path d="M14.3 2.9v4.3h4.2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/>`,
  image: `<rect x="3.6" y="4.8" width="16.8" height="14.4" rx="2" fill="none" stroke="currentColor" stroke-width="1.6"/><circle cx="8.6" cy="9.6" r="1.6" fill="currentColor"/><path d="m4.8 17 4.6-4.8 3.4 3 2.6-2.6 3.4 4.4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/>`,
  upload: `<path d="M12 16.5V4.8M7.4 9.2 12 4.6l4.6 4.6" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/><path d="M4.4 15.8v3.1a1.6 1.6 0 0 0 1.6 1.6h12a1.6 1.6 0 0 0 1.6-1.6v-3.1" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/>`,
  download: `<path d="M12 4.6v11.7M7.4 11.9 12 16.5l4.6-4.6" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/><path d="M4.4 15.8v3.1a1.6 1.6 0 0 0 1.6 1.6h12a1.6 1.6 0 0 0 1.6-1.6v-3.1" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/>`,
  trash: `<path d="M4.6 6.6h14.8M9.4 3.8h5.2M6.6 6.6l.9 13.1a1.5 1.5 0 0 0 1.5 1.4h6a1.5 1.5 0 0 0 1.5-1.4l.9-13.1" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/><path d="M10 10.2v7M14 10.2v7" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>`,
  restore: `<path d="M4.4 12a7.6 7.6 0 1 0 2.4-5.5" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/><path d="M4.4 4.6v4.4h4.4" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>`,
  rename: `<path d="M4.4 19.6 16.9 7.1l2 2L6.4 21.6H4.4z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><path d="m14.6 6.9 2.2 2.2" stroke="currentColor" stroke-width="1.6"/>`,
  copy: `<rect x="8.4" y="8.4" width="11.2" height="11.2" rx="1.7" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M15.6 5.6H5.9a1.5 1.5 0 0 0-1.5 1.5v9.7" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>`,
  move: `<path d="M12 3.8v11.4M8.2 11.4 12 15.2l3.8-3.8" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/><path d="M4.6 20.2h14.8" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/>`,
  search: `<circle cx="10.8" cy="10.8" r="6.2" fill="none" stroke="currentColor" stroke-width="1.7"/><path d="m15.4 15.4 4.4 4.4" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/>`,
  refresh: `<path d="M19.4 12a7.4 7.4 0 1 1-2.2-5.2" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/><path d="M19.6 4.6v4.6h-4.6" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>`,
  plus: `<path d="M12 5.2v13.6M5.2 12h13.6" stroke="currentColor" stroke-width="1.9" stroke-linecap="round"/>`,
  close: `<path d="m6.2 6.2 11.6 11.6M17.8 6.2 6.2 17.8" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/>`,
  minimize: `<path d="M5.6 12h12.8" stroke="currentColor" stroke-width="2" stroke-linecap="round"/>`,
  maximize: `<rect x="5.6" y="5.6" width="12.8" height="12.8" rx="1.4" fill="none" stroke="currentColor" stroke-width="1.7"/><path d="M9 3.8h9.9a1.3 1.3 0 0 1 1.3 1.3v9.9" fill="none" stroke="currentColor" stroke-width="1.4" opacity=".5"/>`,
  chevronLeft: `<path d="M14.6 5.6 8.2 12l6.4 6.4" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>`,
  chevronRight: `<path d="m9.4 5.6 6.4 6.4-6.4 6.4" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>`,
  chevronDown: `<path d="m5.6 9.4 6.4 6.4 6.4-6.4" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>`,
  settings: `<circle cx="12" cy="12" r="3.1" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M12 3.4v2.4M12 18.2v2.4M3.4 12h2.4M18.2 12h2.4M5.9 5.9l1.7 1.7M16.4 16.4l1.7 1.7M18.1 5.9l-1.7 1.7M7.6 16.4l-1.7 1.7" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>`,
  user: `<circle cx="12" cy="8.4" r="3.6" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M4.8 20.2c.9-3.8 3.7-5.8 7.2-5.8s6.3 2 7.2 5.8" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>`,
  users: `<circle cx="9.2" cy="8.6" r="3.2" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M3.4 19.4c.7-3.2 3-4.9 5.8-4.9s5.1 1.7 5.8 4.9" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/><path d="M15.4 5.9a3 3 0 0 1 0 5.6M17 14.9c2 .5 3.3 2 3.6 4.2" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>`,
  logout: `<path d="M14.2 7.6V5.4a1.6 1.6 0 0 0-1.6-1.6H5.8A1.6 1.6 0 0 0 4.2 5.4v13.2a1.6 1.6 0 0 0 1.6 1.6h6.8a1.6 1.6 0 0 0 1.6-1.6v-2.2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/><path d="M9.8 12h10.4m-3.2-3.2L20.2 12l-3.2 3.2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>`,
  shield: `<path d="M12 3.2 19 6v5.6c0 4.3-2.9 7.4-7 9.2-4.1-1.8-7-4.9-7-9.2V6z" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linejoin="round"/><path d="m8.6 12 2.4 2.4 4.4-4.6" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>`,
  cpu: `<rect x="6.4" y="6.4" width="11.2" height="11.2" rx="1.8" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M9.6 3.4v3M14.4 3.4v3M9.6 17.6v3M14.4 17.6v3M3.4 9.6h3M3.4 14.4h3M17.6 9.6h3M17.6 14.4h3" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>`,
  memory: `<rect x="3.6" y="7.6" width="16.8" height="9" rx="1.6" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M7.4 10.6v3.2M10.6 10.6v3.2M13.8 10.6v3.2M17 10.6v3.2" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>`,
  disk: `<ellipse cx="12" cy="6.4" rx="7.4" ry="2.8" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M4.6 6.4v11.2c0 1.6 3.3 2.9 7.4 2.9s7.4-1.3 7.4-2.9V6.4" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M4.6 12c0 1.6 3.3 2.9 7.4 2.9s7.4-1.3 7.4-2.9" fill="none" stroke="currentColor" stroke-width="1.4"/>`,
  network: `<circle cx="12" cy="12" r="8.4" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M3.8 12h16.4M12 3.6c2.4 2.4 3.6 5.3 3.6 8.4s-1.2 6-3.6 8.4c-2.4-2.4-3.6-5.3-3.6-8.4S9.6 6 12 3.6z" fill="none" stroke="currentColor" stroke-width="1.4"/>`,
  docker: `<path d="M4.2 10.4h15.2c-.4 4.7-3.2 7.6-7.5 7.6-4.4 0-7-2.7-7.7-7.6z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><path d="M6.6 7.6h2.6v2.2H6.6zM10.2 7.6h2.6v2.2h-2.6zM13.8 7.6h2.6v2.2h-2.6zM10.2 4.4h2.6v2.2h-2.6z" fill="none" stroke="currentColor" stroke-width="1.3"/><path d="M19.4 10.4c1.2-.4 2-.2 2.6.4" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/>`,
  backup: `<path d="M12 3.4c4.7 0 8.4 3.1 8.4 6.9S16.7 17.2 12 17.2c-1.2 0-2.4-.2-3.4-.6" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/><path d="M3.6 13.6v4.2c0 1.6 1.9 2.9 4.2 2.9 1 0 2-.2 2.7-.6" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/><path d="M8.4 3.8v7.2l3.2-1.9 3.2 1.9V3.8" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"/>`,
  logs: `<path d="M5.4 3.8h13.2v16.4H5.4z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><path d="M8.6 8.2h7.2M8.6 12h7.2M8.6 15.8h4.6" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>`,
  play: `<path d="M7.4 4.8 19 12 7.4 19.2z" fill="currentColor"/>`,
  stop: `<rect x="6.2" y="6.2" width="11.6" height="11.6" rx="1.4" fill="currentColor"/>`,
  restart: `<path d="M20 12a8 8 0 1 1-2.6-5.9" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/><path d="M20 3.8v4.8h-4.8" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>`,
  terminal: `<rect x="3.4" y="4.8" width="17.2" height="14.4" rx="2" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="m7.4 9.4 3 2.6-3 2.6M12.6 15h4" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>`,
  external: `<path d="M13.4 4.6h6v6M19.4 4.6 11 13" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/><path d="M10.6 6.2H6.2a1.6 1.6 0 0 0-1.6 1.6v10a1.6 1.6 0 0 0 1.6 1.6h10a1.6 1.6 0 0 0 1.6-1.6v-4.4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>`,
  check: `<path d="m5.2 12.6 4.4 4.4L18.8 6.8" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>`,
  warning: `<path d="M12 3.8 21.2 20H2.8z" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linejoin="round"/><path d="M12 9.4v4.8M12 17.2v.2" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/>`,
  info: `<circle cx="12" cy="12" r="8.4" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M12 10.8v5.6M12 7.6v.2" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/>`,
  moon: `<path d="M20.2 14.6A8.4 8.4 0 0 1 9.4 3.8a8.4 8.4 0 1 0 10.8 10.8z" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linejoin="round"/>`,
  sun: `<circle cx="12" cy="12" r="4.2" fill="none" stroke="currentColor" stroke-width="1.7"/><path d="M12 2.8v2.4M12 18.8v2.4M2.8 12h2.4M18.8 12h2.4M5.5 5.5l1.7 1.7M16.8 16.8l1.7 1.7M18.5 5.5l-1.7 1.7M7.2 16.8l-1.7 1.7" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>`,
  arrowLeft: `<path d="M20 12H5.2M10.6 6.6 5.2 12l5.4 5.4" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>`,
  arrowUp: `<path d="M12 19.2V5M6.2 10.8 12 5l5.8 5.8" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>`,
  arrowDown: `<path d="M12 4.8V19M6.2 13.2 12 19l5.8-5.8" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>`,
  sort: `<path d="M8.2 5.4v13.2M5 15.4l3.2 3.2 3.2-3.2M15.8 18.6V5.4M12.6 8.6l3.2-3.2 3.2 3.2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>`,
  clock: `<circle cx="12" cy="12" r="8.4" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M12 7.4V12l3.2 2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>`,
  grid: `<rect x="4" y="4" width="6.4" height="6.4" rx="1.2" fill="none" stroke="currentColor" stroke-width="1.6"/><rect x="13.6" y="4" width="6.4" height="6.4" rx="1.2" fill="none" stroke="currentColor" stroke-width="1.6"/><rect x="4" y="13.6" width="6.4" height="6.4" rx="1.2" fill="none" stroke="currentColor" stroke-width="1.6"/><rect x="13.6" y="13.6" width="6.4" height="6.4" rx="1.2" fill="none" stroke="currentColor" stroke-width="1.6"/>`,
  list: `<path d="M8.6 6.4h11M8.6 12h11M8.6 17.6h11" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/><circle cx="4.8" cy="6.4" r="1.2" fill="currentColor"/><circle cx="4.8" cy="12" r="1.2" fill="currentColor"/><circle cx="4.8" cy="17.6" r="1.2" fill="currentColor"/>`,
  eye: `<path d="M2.8 12S6.2 5.8 12 5.8 21.2 12 21.2 12 17.8 18.2 12 18.2 2.8 12 2.8 12z" fill="none" stroke="currentColor" stroke-width="1.6"/><circle cx="12" cy="12" r="2.8" fill="none" stroke="currentColor" stroke-width="1.6"/>`,
  lock: `<rect x="5.4" y="10.4" width="13.2" height="9.4" rx="1.8" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M8.4 10.4V7.8a3.6 3.6 0 0 1 7.2 0v2.6" fill="none" stroke="currentColor" stroke-width="1.6"/>`,
  monitor: `<rect x="3.4" y="4.8" width="17.2" height="11.2" rx="1.8" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M9 20.2h6M12 16v4.2" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>`,
  apps: `<rect x="3.8" y="3.8" width="7" height="7" rx="1.6" fill="none" stroke="currentColor" stroke-width="1.6"/><rect x="13.2" y="3.8" width="7" height="7" rx="1.6" fill="none" stroke="currentColor" stroke-width="1.6"/><rect x="3.8" y="13.2" width="7" height="7" rx="1.6" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M16.7 13.2v7M13.2 16.7h7" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>`,
  control: `<path d="M4.6 7.2h6M14.4 7.2h5M4.6 16.8h5M13.4 16.8h6" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/><circle cx="12.6" cy="7.2" r="2.1" fill="none" stroke="currentColor" stroke-width="1.6"/><circle cx="11.2" cy="16.8" r="2.1" fill="none" stroke="currentColor" stroke-width="1.6"/>`,
  power: `<path d="M12 3.6v8.2" stroke="currentColor" stroke-width="1.9" stroke-linecap="round"/><path d="M6.6 6.6a7.6 7.6 0 1 0 10.8 0" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/>`,
  menu: `<path d="M4.4 7h15.2M4.4 12h15.2M4.4 17h15.2" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/>`,
  edit: `<path d="M4.6 19.4h3.1L19 8.1a2 2 0 0 0 0-2.8l-.3-.3a2 2 0 0 0-2.8 0L4.6 16.3z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><path d="m13.9 7.2 2.9 2.9" stroke="currentColor" stroke-width="1.6"/>`,
  key: `<circle cx="8.4" cy="15.6" r="4.1" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="m11.3 12.7 8.2-8.2M16.6 7.4l2.2 2.2M14.1 9.9l2.2 2.2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>`,
  sessions: `<rect x="3.6" y="5.2" width="12.8" height="9.2" rx="1.6" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M7.6 18.6h12.8V9.4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/>`,
  share: `<circle cx="7.2" cy="12" r="2.7" fill="none" stroke="currentColor" stroke-width="1.6"/><circle cx="17" cy="6.2" r="2.4" fill="none" stroke="currentColor" stroke-width="1.6"/><circle cx="17" cy="17.8" r="2.4" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="m9.6 10.8 5-3.2M9.6 13.2l5 3.2" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>`,
  palette: `<path d="M12 3.6c4.7 0 8.4 3.4 8.4 7.6 0 2.3-1.7 3.8-3.8 3.8h-1.3c-1 0-1.7.8-1.7 1.7 0 .5.2.9.5 1.2.3.4.5.8.5 1.3 0 1.1-.9 1.8-2.6 1.8-4.7 0-8.4-3.7-8.4-8.6 0-4.9 3.7-8.8 8.4-8.8z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><circle cx="8.4" cy="9.4" r="1.1" fill="currentColor"/><circle cx="12.2" cy="7.4" r="1.1" fill="currentColor"/><circle cx="16" cy="9.6" r="1.1" fill="currentColor"/><circle cx="8.8" cy="13.8" r="1.1" fill="currentColor"/>`,
  wallpaper: `<rect x="3.6" y="4.8" width="16.8" height="14.4" rx="2" fill="none" stroke="currentColor" stroke-width="1.6"/><circle cx="9" cy="9.6" r="1.7" fill="currentColor"/><path d="m4.8 17.2 4.8-4.9 3.2 3 2.5-2.6 3.9 4.6" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/>`,
  pin: `<path d="M9.2 3.8h5.6l-1 5 3.2 3.2v1.8H7v-1.8l3.2-3.2z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><path d="M12 13.8v6.4" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>`,
  unpin: `<path d="M9.2 3.8h5.6l-1 5 3.2 3.2v1.8H7v-1.8l3.2-3.2z" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" opacity=".55"/><path d="M12 13.8v6.4" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/><path d="m4.4 4.4 15.2 15.2" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>`,
  more: `<circle cx="12" cy="5.2" r="1.5" fill="currentColor"/><circle cx="12" cy="12" r="1.5" fill="currentColor"/><circle cx="12" cy="18.8" r="1.5" fill="currentColor"/>`,
  wifi: `<path d="M4 9.4a12 12 0 0 1 16 0M6.8 12.6a8 8 0 0 1 10.4 0M9.6 15.8a4 4 0 0 1 4.8 0" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/><circle cx="12" cy="19" r="1.3" fill="currentColor"/>`,
  wifiOff: `<path d="M4 9.4a12 12 0 0 1 5-2.9M15 6.9a12 12 0 0 1 5 2.5M6.8 12.6a8 8 0 0 1 3.2-1.7M17.2 12a8 8 0 0 1-3 1.2M9.6 15.8a4 4 0 0 1 4.8 0" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" opacity=".65"/><circle cx="12" cy="19" r="1.3" fill="currentColor" opacity=".65"/><path d="M4 4.2 20 20.2" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/>`,
  pause: `<rect x="6.6" y="4.8" width="3.6" height="14.4" rx="1.2" fill="currentColor"/><rect x="13.8" y="4.8" width="3.6" height="14.4" rx="1.2" fill="currentColor"/>`,
  calendar: `<rect x="3.8" y="5.2" width="16.4" height="15" rx="2" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M3.8 9.8h16.4M8 3.4v3.4M16 3.4v3.4" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>`,
  filter: `<path d="M3.8 5.4h16.4l-6.4 7.4v6.2l-3.6 1.8v-8z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/>`,
  home: `<path d="M4 10.6 12 3.8l8 6.8V20a1 1 0 0 1-1 1h-4.8v-6.2H9.8V21H5a1 1 0 0 1-1-1z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/>`,
  database: `<ellipse cx="12" cy="6" rx="7.2" ry="2.8" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M4.8 6v12c0 1.6 3.2 2.8 7.2 2.8s7.2-1.2 7.2-2.8V6" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M4.8 11.4c0 1.6 3.2 2.8 7.2 2.8s7.2-1.2 7.2-2.8" fill="none" stroke="currentColor" stroke-width="1.4"/>`,
  activity: `<path d="M3.4 12.4h4l2.6-6.8 4.1 13 2.5-6.2h4" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>`,
  shieldAlert: `<path d="M12 3.2 19 6v5.6c0 4.3-2.9 7.4-7 9.2-4.1-1.8-7-4.9-7-9.2V6z" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linejoin="round"/><path d="M12 7.8v4.6M12 16.4v.2" stroke="currentColor" stroke-width="1.9" stroke-linecap="round"/>`,
  eyeOff: `<path d="M3.4 3.4 20.6 20.6" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/><path d="M9.9 5.9A9.8 9.8 0 0 1 12 5.8c5.8 0 9.2 6.2 9.2 6.2a16 16 0 0 1-3.4 4M6.4 7.4A16.4 16.4 0 0 0 2.8 12s3.4 6.2 9.2 6.2c1.3 0 2.5-.3 3.6-.8" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>`,
  chevronUp: `<path d="m5.6 14.6 6.4-6.4 6.4 6.4" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>`,
  arrowRight: `<path d="M4 12h14.8M13.4 6.6 18.8 12l-5.4 5.4" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>`,
};

export function iconSvg(name: string, size = 20): SVGSVGElement {
  const path = PATHS[name] ?? PATHS.file ?? "";
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("width", String(size));
  svg.setAttribute("height", String(size));
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("focusable", "false");
  svg.classList.add("icon");
  svg.innerHTML = path;
  return svg;
}

export function iconEl(name: string, size = 20): HTMLElement {
  const span = document.createElement("span");
  span.className = "icon-wrap";
  span.appendChild(iconSvg(name, size));
  return span;
}

export function hasIcon(name: string): boolean {
  return name in PATHS;
}

/** Icon name for a file based on its MIME type / extension. */
export function fileIcon(entry: { kind: string; name: string; mime?: string }): string {
  if (entry.kind === "dir") return "folder";
  const mime = entry.mime ?? "";
  if (mime.startsWith("image/")) return "image";
  return "file";
}

// ---------------------------------------------------------------------------
// Application icons: coloured, two-tone originals so each app is recognisable
// by function at a glance. Static markup only (no user data, safe as innerHTML).
// ---------------------------------------------------------------------------

const APP_ICONS: Record<string, string> = {
  files: `<path d="M3.2 6.6c0-.9.7-1.6 1.6-1.6h4.1l1.9 2.2h8.4c.9 0 1.6.7 1.6 1.6v8.6c0 .9-.7 1.6-1.6 1.6H4.8c-.9 0-1.6-.7-1.6-1.6V6.6z" fill="#E9A93C"/><path d="M3.2 9.4h17.6v7.4c0 .9-.7 1.6-1.6 1.6H4.8c-.9 0-1.6-.7-1.6-1.6V9.4z" fill="#F2B95A"/><path d="M3.2 6.6c0-.9.7-1.6 1.6-1.6h4.1l1.9 2.2H3.2V6.6z" fill="#C98B2C"/><rect x="12.6" y="10.4" width="6" height="5.4" rx="0.9" fill="#FFF7E8"/><path d="M14 12h3.2M14 13.6h3.2M14 15.2h2" stroke="#D9A035" stroke-width="0.8" stroke-linecap="round"/>`,
  applications: `<rect x="2.8" y="2.8" width="18.4" height="18.4" rx="4.2" fill="#2785D8"/><rect x="6.4" y="6.4" width="4.6" height="4.6" rx="1.1" fill="#FFFFFF"/><rect x="13" y="6.4" width="4.6" height="4.6" rx="1.1" fill="#FFFFFF" opacity="0.82"/><rect x="6.4" y="13" width="4.6" height="4.6" rx="1.1" fill="#FFFFFF" opacity="0.82"/><rect x="13" y="13" width="4.6" height="4.6" rx="1.1" fill="#FFFFFF" opacity="0.62"/>`,
  monitor: `<rect x="2.8" y="4" width="18.4" height="13" rx="2.4" fill="#2B9C74"/><rect x="4.6" y="5.8" width="14.8" height="9.4" rx="1.4" fill="#E6F4EE"/><path d="M6.4 11.4h2.6l1.3-2.6 2.2 4.6 1.5-2.8h3.6" fill="none" stroke="#1F7A5C" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/><path d="M9.4 19.4h5.2M12 17v2.4" stroke="#2B9C74" stroke-width="1.7" stroke-linecap="round"/>`,
  backup: `<path d="M12 3.4a8.4 8.4 0 1 1-7.6 4.9" fill="none" stroke="#D9772A" stroke-width="2.2" stroke-linecap="round"/><path d="M4 3.6v5h5" fill="none" stroke="#D9772A" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/><rect x="7.8" y="10" width="8.4" height="7.6" rx="1.2" fill="#F4C39A"/><path d="M10.4 12.4h3.2M10.4 14.6h3.2" stroke="#C25E19" stroke-width="1" stroke-linecap="round"/>`,
  controlpanel: `<circle cx="12" cy="12" r="3" fill="none" stroke="#5A6673" stroke-width="2"/><path d="M12 2.9v3M12 18.1v3M2.9 12h3M18.1 12h3M5.6 5.6l2.1 2.1M16.3 16.3l2.1 2.1M18.4 5.6l-2.1 2.1M7.7 16.3l-2.1 2.1" stroke="#5A6673" stroke-width="2" stroke-linecap="round"/><circle cx="12" cy="12" r="1.3" fill="#2785D8"/>`,
  logs: `<path d="M5.4 3.6h9.2l4 4v12.8H5.4z" fill="#F3F5F7" stroke="#8C98A6" stroke-width="1.4" stroke-linejoin="round"/><path d="M14.4 3.8v4h4" fill="none" stroke="#8C98A6" stroke-width="1.4" stroke-linejoin="round"/><path d="M8.2 10.4h7.4M8.2 13h7.4M8.2 15.6h4.6" stroke="#2785D8" stroke-width="1.3" stroke-linecap="round"/>`,
};

const APP_ACCENT: Record<string, string> = {
  files: "files",
  applications: "applications",
  monitor: "monitor",
  backup: "backup",
  controlpanel: "controlpanel",
  logs: "logs",
};

export function appAccentClass(appId: string): string {
  return `app-icon--${APP_ACCENT[appId] ?? "controlpanel"}`;
}

/** Coloured application icon (falls back to the monochrome icon set). */
export function appIconSvg(appId: string, fallbackIcon: string, size = 28): SVGSVGElement {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("width", String(size));
  svg.setAttribute("height", String(size));
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("focusable", "false");
  svg.classList.add("icon", "app-icon", appAccentClass(appId));
  const markup = APP_ICONS[appId];
  if (!markup) {
    return iconSvg(fallbackIcon, size);
  }
  svg.innerHTML = markup;
  return svg;
}
