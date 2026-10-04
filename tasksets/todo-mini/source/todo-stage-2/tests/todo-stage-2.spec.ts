import { test, expect } from '@playwright/test';
import { uid, item, addTask, remaining } from './support/todo';

test('REQ-1..3 still work: heading, add, persistence', async ({ page }) => {
  const text = `Keep working ${uid()}`;
  await page.goto('/');
  await expect(page.getByRole('heading', { level: 1, name: 'Todo', exact: true })).toBeVisible();
  await addTask(page, text);
  await expect(page.getByLabel('New task')).toHaveValue('');
  await page.reload();
  await expect(item(page, text)).toHaveCount(1);
});

test('REQ-4: checking a task is stored', async ({ page }) => {
  const text = `Pay rent ${uid()}`;
  await page.goto('/');
  await addTask(page, text);
  const box = page.getByRole('checkbox', { name: text, exact: true });
  await expect(box).not.toBeChecked();
  await box.check();
  await page.waitForTimeout(500);
  await page.reload();
  await expect(page.getByRole('checkbox', { name: text, exact: true })).toBeChecked();
  await page.getByRole('checkbox', { name: text, exact: true }).uncheck();
  await page.waitForTimeout(500);
  await page.reload();
  await expect(page.getByRole('checkbox', { name: text, exact: true })).not.toBeChecked();
});

test('REQ-5: deleting a task removes it', async ({ page }) => {
  const text = `Old idea ${uid()}`;
  await page.goto('/');
  await addTask(page, text);
  await page.getByRole('button', { name: `Delete ${text}`, exact: true }).click();
  await expect(item(page, text)).toHaveCount(0);
  await page.reload();
  await expect(item(page, text)).toHaveCount(0);
});

test('REQ-6: remaining counter follows add, check, uncheck and delete', async ({ page }) => {
  const text = `Count me ${uid()}`;
  await page.goto('/');
  const n = await remaining(page);
  await addTask(page, text);
  await expect.poll(() => remaining(page)).toBe(n + 1);
  await page.getByRole('checkbox', { name: text, exact: true }).check();
  await expect.poll(() => remaining(page)).toBe(n);
  await page.getByRole('checkbox', { name: text, exact: true }).uncheck();
  await expect.poll(() => remaining(page)).toBe(n + 1);
  await page.getByRole('button', { name: `Delete ${text}`, exact: true }).click();
  await expect.poll(() => remaining(page)).toBe(n);
});
