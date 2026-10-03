import { expect, test } from '@playwright/test';
import { addTodo, openHome, todoItem, uniqueText } from './support/e2e';

test('REQ-2-1: Mark a Todo Complete - Scenario 1', async ({ page }, testInfo) => {
  const text = uniqueText(testInfo, 'Pay rent');

  await openHome(page);
  await addTodo(page, text);
  const box = todoItem(page, text).getByRole('checkbox', { name: text, exact: true });
  await expect(box).not.toBeChecked();

  await box.check();
  await expect(box).toBeChecked();

  await page.reload();
  await expect(todoItem(page, text).getByRole('checkbox', { name: text, exact: true })).toBeChecked();
});

test('REQ-2-1: Mark a Todo Complete - Scenario 2', async ({ page }, testInfo) => {
  const text = uniqueText(testInfo, 'Pay rent');

  await openHome(page);
  await addTodo(page, text);
  const box = page.getByRole('checkbox', { name: text, exact: true });
  await box.check();
  await expect(box).toBeChecked();
  await page.reload();
  await expect(box).toBeChecked();

  await box.uncheck();
  await expect(box).not.toBeChecked();
  await page.reload();
  await expect(box).not.toBeChecked();
});
