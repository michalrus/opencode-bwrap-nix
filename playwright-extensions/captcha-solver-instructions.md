# Web browsing and CAPTCHAs

The Playwright browser runs headless with a private, throwaway profile per session and the 2Captcha solver extension.

- When a page shows a CAPTCHA (reCAPTCHA, Turnstile, GeeTest, Arkose Labs/FunCaptcha, Amazon WAF, image puzzles), the extension solves it on its own. Do not try to solve it yourself, and do not click the widget.
- Wait with `browser_wait_for` (about 30 s), then take a new snapshot. The `Solve with 2Captcha` control in the page reports progress in its `data-state` attribute: `ready` → `solving` → `solved`. Repeat the wait while it is `solving`.
- After `solved`, submit the form or continue as normal. If the state has not changed after 3 minutes, or it reports an error, stop and tell the user; do not retry in a loop, every attempt costs money.
- hCaptcha is not supported by the solver. If a page shows hCaptcha, stop and tell the user.
- A 403 or "Access denied" page without a CAPTCHA widget is an IP or fingerprint block. The extension cannot help; report it to the user.
