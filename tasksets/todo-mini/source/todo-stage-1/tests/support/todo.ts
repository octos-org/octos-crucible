import { Page, Locator, expect } from '@playwright/test';

export const uid = () => Math.random().toString(36).slice(2, 8);

export function item(page: Page, text: string): Locator {
  return page.getByRole('listitem').filter({ hasText: text });
}

export async function addTask(page: Page, text: string): Promise<void> {
  await page.getByLabel('New task').fill(text);
  await page.getByRole('button', { name: 'Add', exact: true }).click();
  await expect(item(page, text)).toHaveCount(1);
}

export async function remaining(page: Page): Promise<number> {
  const el = page.getByText(/Remaining:\s*\d+/).first();
  await expect(el).toBeVisible();
  return Number(((await el.textContent()) ?? '').match(/Remaining:\s*(\d+)/)![1]);
}
