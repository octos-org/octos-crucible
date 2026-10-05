import { Browser, Page, expect } from '@playwright/test';

export const uid = () => Math.random().toString(36).slice(2, 8);
export const PW: Record<string, string> = { ada: 'ada-pass', bob: 'bob-pass', cy: 'cy-pass', dee: 'dee-pass' };

export async function signIn(page: Page, user: string, pw = PW[user]): Promise<void> {
  await page.goto('/login');
  await page.getByLabel('Username').fill(user);
  await page.getByLabel('Password').fill(pw);
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page).toHaveURL(/\/$/);
  await expect(page.getByText(`Signed in as ${user} (`)).toBeVisible();
}

/** A fresh browser session signed in as `user`. */
export async function as(browser: Browser, user: string, pw?: string): Promise<Page> {
  const page = await (await browser.newContext()).newPage();
  await signIn(page, user, pw);
  return page;
}

/** Opens a ticket as the signed-in customer; returns the ticket page URL. */
export async function newTicket(page: Page, title: string, opts: { category?: string; priority?: string } = {}): Promise<string> {
  await page.goto('/tickets/new');
  await page.getByLabel('Title').fill(title);
  if (opts.category) await page.getByLabel('Category').selectOption(opts.category);
  if (opts.priority) await page.getByRole('radio', { name: opts.priority, exact: true }).check();
  await page.getByLabel('Description').fill('This is a long enough description of the problem.');
  await page.getByRole('button', { name: 'Submit ticket', exact: true }).click();
  await expect(page.getByRole('heading', { level: 1, name: title, exact: true })).toBeVisible();
  return page.url();
}

export function row(page: Page, title: string) {
  return page.getByRole('row').filter({ has: page.getByRole('link', { name: title, exact: true }) });
}
