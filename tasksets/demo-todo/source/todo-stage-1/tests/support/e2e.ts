import { expect, Locator, Page, TestInfo } from '@playwright/test';

export function baseUrl(): string {
  return process.env.BASE_URL ?? 'http://127.0.0.1:3000';
}

export async function openHome(page: Page): Promise<void> {
  await page.goto(baseUrl());
  await expect(todoList(page)).toBeAttached();
}

export function todoList(page: Page): Locator {
  return page.getByRole('list', { name: 'Todo list', exact: true });
}

/** The list item of `text` (texts are unique per test, see uniqueText). */
export function todoItem(page: Page, text: string): Locator {
  return todoList(page).getByRole('listitem').filter({ hasText: text });
}

/** Todos live in one shared backend: keep each test's texts unique. */
let counter = 0;
export function uniqueText(testInfo: TestInfo, base: string): string {
  counter += 1;
  const suffix = `${testInfo.workerIndex}${testInfo.retry}${counter}-${Date.now().toString(36)}`;
  return `${base} ${suffix}`;
}

export async function addTodo(page: Page, text: string): Promise<void> {
  await page.getByRole('textbox', { name: 'New todo', exact: true }).fill(text);
  await page.getByRole('button', { name: 'Add', exact: true }).click();
  await expect(todoItem(page, text)).toHaveCount(1);
}
