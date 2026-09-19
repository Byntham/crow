import { chromium } from '/opt/browser/node_modules/playwright-core/index.mjs';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { stat } from 'node:fs/promises';
const server = spawn(process.execPath, ['server.mjs'], { stdio: 'ignore' });
let browser;
try {
  for (let attempt = 0; attempt < 100; attempt++) {
    try { if ((await fetch('http://127.0.0.1:8080/')).ok) break; } catch {}
    await new Promise(r => setTimeout(r, 50));
  }
  browser = await chromium.launch({ executablePath: '/usr/bin/chromium', args: ['--no-sandbox','--disable-dev-shm-usage'], headless: true });
  const page = await browser.newPage({ viewport: { width: 1040, height: 800 }, deviceScaleFactor: 1 });
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  page.on('console', message => { if (message.type() === 'error' && !message.text().includes('404')) errors.push(message.text()); });
  const response = await page.goto('http://127.0.0.1:8080/');
  assert.equal(response.status(), 200);
  const button = page.getByRole('button', { name: 'Pay $49.00', exact: true });
  assert.equal(await button.isVisible(), true);
  assert.equal(await button.isEnabled(), true);
  assert.equal(await page.locator('.finish').evaluate(img => img.complete && img.naturalWidth === 600), true);
  await page.screenshot({ path: '/tmp/checkout.png', fullPage: true });
  await button.click();
  assert.equal(await page.getByRole('status').textContent(), 'Order confirmed. Thank you, Alex.');
  assert.equal(await page.getByRole('status').isVisible(), true);
  assert.equal(await button.isDisabled(), true);
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ checks: { http200: true, paymentButtonPresentAndVisible: true, accessibleNameCorrect: true, graphicLoaded: true, clickConfirmsOrder: true, noJavaScriptErrors: true }, screenshot: '/tmp/checkout.png', screenshotBytes: (await stat('/tmp/checkout.png')).size, result: 'passed' }));
} finally { if (browser) await browser.close(); server.kill(); }
