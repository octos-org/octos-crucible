import { test, expect } from '@playwright/test';
import { uid, item, addTask, remaining } from './support/todo';

test('REQ-1..6 still work: add, complete, delete, counter', async ({ page }) => {
  const text = `Still here ${uid()}`;
  await page.goto('/');
  const n = await remaining(page);
  await addTask(page, text);
  await expect.poll(() => remaining(page)).toBe(n + 1);
  await page.getByRole('checkbox', { name: text, exact: true }).check();
  await expect.poll(() => remaining(page)).toBe(n);
  await page.reload();
  await expect(page.getByRole('checkbox', { name: text, exact: true })).toBeChecked();
  await page.getByRole('button', { name: `Delete ${text}`, exact: true }).click();
  await expect(item(page, text)).toHaveCount(0);
});

test('REQ-7: filters show active, completed and all tasks', async ({ page }) => {
  const a = `Read book ${uid()}`, b = `Cook dinner ${uid()}`;
  await page.goto('/');
  await addTask(page, a);
  await addTask(page, b);
  await page.getByRole('checkbox', { name: b, exact: true }).check();
  await page.waitForTimeout(500);
  const n = await remaining(page);
  await page.getByRole('button', { name: 'Active', exact: true }).click();
  await expect(item(page, a)).toHaveCount(1);
  await expect(item(page, b)).toHaveCount(0);
  expect(await remaining(page)).toBe(n);
  await page.getByRole('button', { name: 'Completed', exact: true }).click();
  await expect(item(page, b)).toHaveCount(1);
  await expect(item(page, a)).toHaveCount(0);
  await page.getByRole('button', { name: 'All', exact: true }).click();
  await expect(item(page, a)).toHaveCount(1);
  await expect(item(page, b)).toHaveCount(1);
});

test('REQ-8: clear completed deletes done tasks only', async ({ page }) => {
  const done = `Water plants ${uid()}`, open = `Call mom ${uid()}`;
  await page.goto('/');
  await addTask(page, done);
  await addTask(page, open);
  await page.getByRole('checkbox', { name: done, exact: true }).check();
  await page.waitForTimeout(500);
  await page.getByRole('button', { name: 'Clear completed', exact: true }).click();
  await expect(item(page, done)).toHaveCount(0);
  await page.reload();
  await expect(item(page, done)).toHaveCount(0);
  await expect(item(page, open)).toHaveCount(1);
});

test('REQ-9: stats page counts stored tasks', async ({ page }) => {
  const text = `Stat task ${uid()}`;
  await page.goto('/');
  await addTask(page, text);
  await page.getByRole('checkbox', { name: text, exact: true }).check();
  await page.waitForTimeout(500);
  await page.reload();
  const total = await page.getByRole('listitem').count();
  const done = await page.getByRole('checkbox', { checked: true }).count();
  await page.getByRole('link', { name: 'Stats', exact: true }).click();
  await expect(page).toHaveURL(/\/stats\/?$/);
  await expect(page.getByRole('heading', { level: 1, name: 'Stats', exact: true })).toBeVisible();
  await expect(page.getByText(new RegExp(`Total:\\s*${total}(?!\\d)`))).toBeVisible();
  await expect(page.getByText(new RegExp(`Done:\\s*${done}(?!\\d)`))).toBeVisible();
  await page.getByRole('link', { name: 'Back', exact: true }).click();
  await expect(page.getByRole('heading', { level: 1, name: 'Todo', exact: true })).toBeVisible();
});
