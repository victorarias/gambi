#!/usr/bin/env node
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');

const { chromium } = require('@playwright/test');
const { startGambi } = require('../playwright/tests/helpers/gambi');

async function resolveGambiBin() {
  if (process.env.GAMBI_BIN) {
    return path.resolve(process.env.GAMBI_BIN);
  }
  return path.resolve(__dirname, '..', 'target', 'debug', 'gambi');
}

async function ensureFile(filePath) {
  await fs.access(filePath);
}

async function capture() {
  const binPath = await resolveGambiBin();
  await ensureFile(binPath);

  const configHome = await fs.mkdtemp(path.join(os.tmpdir(), 'gambi-readme-shots-'));
  const browser = await chromium.launch({ headless: true });

  let gambi;
  try {
    gambi = await startGambi({ binPath, configHome, execEnabled: true });

    const page = await browser.newPage({ viewport: { width: 1540, height: 980 } });
    await page.goto(gambi.baseUrl, { waitUntil: 'domcontentloaded' });
    await page.waitForTimeout(1200);

    const dashboardPath = path.resolve(__dirname, '..', 'docs', 'screenshots', 'admin-dashboard.png');
    await page.screenshot({ path: dashboardPath, fullPage: true });

    await page.fill('#server-name', 'fixture');
    await page.fill('#server-url', gambi.fixtureServerUrl);
    await page.click('#server-add-form button[type=submit]');
    await page.waitForTimeout(700);

    await page.selectOption('#override-server', 'fixture');
    await page.fill('#override-tool', 'fixture_echo');
    await page.fill('#override-description', 'Fixture tool description override from admin UI');
    await page.click('#override-set-form button[type=submit]');
    await page.waitForTimeout(900);

    const overridesPath = path.resolve(__dirname, '..', 'docs', 'screenshots', 'admin-overrides.png');
    await page.screenshot({ path: overridesPath, fullPage: true });

    console.log('Captured screenshots:');
    console.log(`- ${dashboardPath}`);
    console.log(`- ${overridesPath}`);
  } finally {
    await browser.close();
    if (gambi) {
      await gambi.stop();
    }
    await fs.rm(configHome, { recursive: true, force: true });
  }
}

capture().catch((err) => {
  console.error(err);
  process.exit(1);
});
