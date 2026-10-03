import { expect, test } from '@playwright/test';
import { addTodo, openHome, todoItem, todoList, uniqueText } from './support/e2e';

test('REQ-1-1: View the Todo List - Scenario 1', async ({ page }) => {
  await openHome(page);

  await expect(page.getByRole('heading', { level: 1, name: 'Todos', exact: true })).toBeVisible();
  await expect(todoList(page)).toBeAttached();
  await expect(page.getByRole('textbox', { name: 'New todo', exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Add', exact: true })).toBeEnabled();
});

test('REQ-1-1: View the Todo List - Scenario 2', async ({ page }, testInfo) => {
  const text = uniqueText(testInfo, 'Buy milk');

  await openHome(page);
  await addTodo(page, text);
  await page.reload();

  await expect(todoItem(page, text)).toHaveCount(1);
  await expect(todoItem(page, text)).toBeVisible();
});
