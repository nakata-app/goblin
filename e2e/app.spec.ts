import { test, expect } from '@playwright/test';

test.describe('goblin UI E2E', () => {
  test('ana sayfa yuklenir ve baslik goruntulenir', async ({ page }) => {
    await page.goto('/');
    await expect(page.locator('.panel-header-title')).toHaveText('goblin');
  });

  test('input alani goruntulenir', async ({ page }) => {
    await page.goto('/');
    const input = page.locator('.input-bar textarea');
    await expect(input).toBeVisible();
    await expect(input).toBeEnabled();
  });

  test('karakter idle state\'te goruntulenir', async ({ page }) => {
    await page.goto('/');
    const avatar = page.locator('.goblin-avatar');
    await expect(avatar).toBeVisible();
    await expect(page.locator('.goblin-state-text')).toContainText('Hazir');
  });

  test('status bar goruntulenir', async ({ page }) => {
    await page.goto('/');
    const statusBar = page.locator('.status-bar');
    await expect(statusBar).toBeVisible();
    await expect(statusBar).toContainText('deepseek');
  });

  test('⌘K ile command palette acilir', async ({ page }) => {
    await page.goto('/');
    await page.keyboard.press('Meta+k');
    await expect(page.locator('.command-palette')).toBeVisible();
    await page.keyboard.press('Escape');
    await expect(page.locator('.command-palette')).not.toBeVisible();
  });
});
