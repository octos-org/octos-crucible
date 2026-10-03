import { expect, test } from '@playwright/test';
import { addTodo, openHome, todoItem, uniqueText } from './support/e2e';

test('REQ-2-2: Delete a Todo - Scenario 1', async ({ page }, testInfo) => {
  const old = uniqueText(testInfo, 'Old task');
  const keep = uniqueText(testInfo, 'Keep task');

  await openHome(page);
  await addTodo(page, old);
  await addTodo(page, keep);

  await page.getByRole('button', { name: `Delete ${old}`, exact: true }).click();
  await expect(todoItem(page, old)).toHaveCount(0);
  await expect(todoItem(page, keep)).toHaveCount(1);

  await page.reload();
  await expect(todoItem(page, keep)).toHaveCount(1);
  await expect(todoItem(page, old)).toHaveCount(0);
});

test('REQ-2-2: Delete a Todo - Scenario 2 (stage 1 still works)', async ({ page }, testInfo) => {
  const text = uniqueText(testInfo, 'Short lived');

  // Adding with Enter (stage 1) still works next to the new controls.
  await openHome(page);
  await page.getByRole('textbox', { name: 'New todo', exact: true }).fill(text);
  await page.getByRole('textbox', { name: 'New todo', exact: true }).press('Enter');
  await expect(todoItem(page, text)).toHaveCount(1);

  await todoItem(page, text).getByRole('button', { name: `Delete ${text}`, exact: true }).click();
  await expect(todoItem(page, text)).toHaveCount(0);
});
