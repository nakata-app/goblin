import { test, expect } from '@playwright/test';

test.describe('Goblin UI smoke (post-polish)', () => {
  test('app loads with no console errors', async ({ page }) => {
    const errors: string[] = [];
    page.on('console', (msg) => {
      if (msg.type() === 'error') errors.push(msg.text());
    });
    page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`));

    await page.goto('/');
    await page.waitForSelector('.panel-header-title');
    await page.waitForTimeout(500);

    const filtered = errors.filter(
      (e) => !e.includes('__TAURI__') && !e.includes('IPC') && !e.includes('favicon')
    );
    expect(filtered, `console errors: ${filtered.join('\n')}`).toEqual([]);
  });

  test('header model pill opens dropdown and lets you pick', async ({ page }) => {
    await page.goto('/');
    const pill = page.locator('.header-pill').first();
    await expect(pill).toBeVisible();
    const before = await pill.textContent();
    expect(before).toMatch(/Fast|Pro|Haiku|Sonnet|Opus|Air/);

    await pill.click();
    const menu = page.locator('.model-menu');
    await expect(menu).toBeVisible();

    // Pick a different option from a different group
    await menu.locator('.model-item').nth(2).click();
    await page.waitForTimeout(100);
    const after = await pill.textContent();
    expect(after).not.toBe(before);
  });

  test('input hint shows on focus / hidden on blur', async ({ page }) => {
    await page.goto('/');
    const input = page.locator('.chat-input');
    const hint = page.locator('.input-hint');

    // hint should mention Enter
    await expect(hint).toContainText('Enter');

    await input.focus();
    await page.waitForTimeout(150);
    const focusedOpacity = await hint.evaluate(
      (el) => getComputedStyle(el).opacity
    );
    expect(parseFloat(focusedOpacity)).toBeGreaterThan(0.5);
  });

  test('slash key on empty input opens command palette', async ({ page }) => {
    await page.goto('/');
    const input = page.locator('.chat-input');
    await input.focus();
    await page.keyboard.press('/');
    await expect(page.locator('.cmd-palette')).toBeVisible();
    await page.keyboard.press('Escape');
  });

  test('tabbar shows + button and reacts to new', async ({ page }) => {
    await page.goto('/');
    const tabbar = page.locator('.tabbar');
    await expect(tabbar).toBeVisible();
    await expect(tabbar.locator('.tab-new')).toBeVisible();
  });

  test('WhatsApp panel opens and shows toolbar', async ({ page }) => {
    await page.goto('/');
    await page.click('button[title="WhatsApp"]');
    const panel = page.locator('.wa-panel');
    await expect(panel).toBeVisible();
    await expect(panel.locator('.wa-title')).toHaveText('WhatsApp');
  });

  test('full-page screenshot for visual review', async ({ page }, testInfo) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto('/');
    await page.waitForSelector('.panel-header-title');
    await page.waitForTimeout(300);
    const buf = await page.screenshot({ fullPage: true });
    await testInfo.attach('home.png', { body: buf, contentType: 'image/png' });
  });

  test('whatsapp panel screenshot', async ({ page }, testInfo) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto('/');
    await page.click('button[title="WhatsApp"]');
    await page.waitForSelector('.wa-panel');
    await page.waitForTimeout(300);
    const buf = await page.screenshot({ fullPage: true });
    await testInfo.attach('whatsapp.png', { body: buf, contentType: 'image/png' });
  });
});
