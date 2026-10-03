// Fixture test pack for the scorer's end-to-end check: two pass, one is
// meant to fail (exercises error text + screenshot).
import { expect, test } from '@playwright/test';

test('shows the heading', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByRole('heading', { name: 'Hello fixture' })).toBeVisible();
});

test('button updates the page', async ({ page }) => {
  await page.goto('/');
  await page.getByRole('button', { name: 'Click me' }).click();
  await expect(page.locator('#out')).toHaveText('clicked');
});

test('missing feature', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByRole('link', { name: 'Sign in' })).toBeVisible({ timeout: 2000 });
});
