import { test, expect } from '@playwright/test';
import { uid, item, addTask } from './support/todo';

test('REQ-1: home page shows heading, input and Add button', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByRole('heading', { level: 1, name: 'Todo', exact: true })).toBeVisible();
  await expect(page.getByLabel('New task')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Add', exact: true })).toBeVisible();
});

test('REQ-2: adding a task lists it and clears the input', async ({ page }) => {
  const text = `Buy milk ${uid()}`;
  await page.goto('/');
  await page.getByLabel('New task').fill(`  ${text}  `);
  await page.getByRole('button', { name: 'Add', exact: true }).click();
  await expect(item(page, text)).toHaveCount(1);
  await expect(page.getByLabel('New task')).toHaveValue('');
  const before = await page.getByRole('listitem').count();
  await page.getByRole('button', { name: 'Add', exact: true }).click();
  await page.waitForTimeout(500);
  await expect(page.getByRole('listitem')).toHaveCount(before);
});

test('REQ-3: tasks persist across reloads and browser sessions', async ({ page, browser }) => {
  const text = `Walk the dog ${uid()}`;
  await page.goto('/');
  await addTask(page, text);
  await page.reload();
  await expect(item(page, text)).toHaveCount(1);
  const other = await browser.newContext();
  const page2 = await other.newPage();
  await page2.goto('/');
  await expect(item(page2, text)).toHaveCount(1);
  await other.close();
});
