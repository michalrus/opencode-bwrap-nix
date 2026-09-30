# Web browsing

Every opencode session and subagent has its own headless Chromium with a throwaway profile. Other agents never see your tabs, and you never see theirs.

- The browser starts on your first browser tool call and needs about 1 GiB of memory while it runs.
- When a browsing task is done, call `browser_close`. It closes the browser and frees the memory at once. An idle browser is also closed automatically, but much later.
- `browser_close` discards all tabs. Do not call it between the steps of one task; the next browser call starts a new, empty browser.
