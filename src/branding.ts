/**
 * Front-end branding constants.
 *
 * This file is the SINGLE source of truth for the user-facing product name.
 * Everything backend-side (Tauri productName, Cargo crate names, IPC events,
 * localStorage keys, MIME types, DOM event names…) intentionally keeps the
 * upstream "sinew" identifier so we can merge updates from the open-source
 * repo (https://github.com/Paseru/sinew) without conflicts on the back.
 *
 * If upstream rewords visible UI strings, the merge will conflict on the
 * lines that reference `APP_NAME` — keep our version.
 */
export const APP_NAME = "yusAi";

