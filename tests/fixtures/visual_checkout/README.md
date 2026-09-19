# Checkout demo

A small static checkout application. No actual payment is processed.

Run `node server.mjs` to start the application on port 8080.
Run `node smoke.mjs` to exercise checkout in headless Chromium and save a screenshot to `/tmp/checkout.png`.
The prepared review image provides Chromium at `/usr/bin/chromium` and Playwright at `/opt/browser/node_modules/playwright-core`.
