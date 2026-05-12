import { test, expect } from '@playwright/test';

test.describe('goblin UI E2E', () => {
  test('ana sayfa yuklenir ve baslik goruntulenir', async ({ page }) => {
    await page.goto('/');
    await expect(page.locator('.panel-header-title').first()).toHaveText('goblin');
  });

  test('input alani goruntulenir', async ({ page }) => {
    await page.goto('/');
    const input = page.locator('.chat-input');
    await expect(input).toBeVisible();
    await expect(input).toBeEnabled();
  });

  test('karakter idle state\'te goruntulenir', async ({ page }) => {
    await page.goto('/');
    const character = page.locator('.goblin-strip');
    await expect(character).toBeVisible();
    await expect(page.locator('.goblin-status-text')).toContainText('Ready');
  });

  test('status bar goruntulenir', async ({ page }) => {
    await page.goto('/');
    const statusBar = page.locator('.status-bar');
    await expect(statusBar).toBeVisible();
    await expect(statusBar).toContainText('model:');
  });

  test('⌘K ile command palette acilir', async ({ page }) => {
    await page.goto('/');
    await page.click('button:has-text("⌘K")');
    await expect(page.locator('.cmd-palette')).toBeVisible();
    await page.keyboard.press('Escape');
    await expect(page.locator('.cmd-palette')).not.toBeVisible();
  });
});
