import { expect, test } from '@playwright/test';
import { openHome, todoItem, todoList, uniqueText } from './support/e2e';

test('REQ-1-2: Add a Todo - Scenario 1', async ({ page }, testInfo) => {
  const text = uniqueText(testInfo, 'Buy milk');

  await openHome(page);
  const input = page.getByRole('textbox', { name: 'New todo', exact: true });
  await input.fill(text);
  await page.getByRole('button', { name: 'Add', exact: true }).click();

  await expect(todoItem(page, text)).toHaveCount(1);
  await expect(input).toHaveValue('');
});

test('REQ-1-2: Add a Todo - Scenario 2', async ({ page }, testInfo) => {
  const text = uniqueText(testInfo, 'Walk the dog');

  await openHome(page);
  await page.getByRole('textbox', { name: 'New todo', exact: true }).fill(`  ${text}  `);
  await page.getByRole('textbox', { name: 'New todo', exact: true }).press('Enter');
  await expect(todoItem(page, text)).toHaveCount(1);

  await page.reload();
  await expect(todoItem(page, text)).toHaveCount(1);
  await expect(todoList(page)).toContainText(text);
});

test('REQ-1-2: Add a Todo - Scenario 3', async ({ page }) => {
  await openHome(page);
  await page.getByRole('textbox', { name: 'New todo', exact: true }).fill('   ');
  await page.getByRole('button', { name: 'Add', exact: true }).click();

  await expect(page.getByText('Todo text is required', { exact: true })).toBeVisible();
});
