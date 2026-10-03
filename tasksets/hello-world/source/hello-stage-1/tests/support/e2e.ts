import { Page } from '@playwright/test';

export function baseUrl(): string {
  return process.env.BASE_URL ?? 'http://127.0.0.1:3000';
}

export async function openHome(page: Page): Promise<void> {
  await page.goto(baseUrl());
}
