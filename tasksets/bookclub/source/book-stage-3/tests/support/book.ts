import { Page, expect } from '@playwright/test';

export const uid = () => Math.random().toString(36).slice(2, 8);

export async function signIn(page: Page, user: string, pw: string): Promise<void> {
  await page.goto('/login');
  await page.getByLabel('Username').fill(user);
  await page.getByLabel('Password').fill(pw);
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page.getByText(`Signed in as ${user}`)).toBeVisible();
}

export async function register(page: Page, user: string, pw: string): Promise<void> {
  await page.goto('/register');
  await page.getByLabel('Username').fill(user);
  await page.getByLabel('Password').fill(pw);
  await page.getByRole('button', { name: 'Create account', exact: true }).click();
  await expect(page.getByText(`Signed in as ${user}`)).toBeVisible();
}

export async function signOut(page: Page): Promise<void> {
  await page.goto('/');
  await page.getByRole('button', { name: 'Sign out', exact: true }).click();
  await expect(page.getByRole('link', { name: 'Sign in', exact: true })).toBeVisible();
}

/** Adds a book as the signed-in user; returns the URL of its page. */
export async function addBook(page: Page, title: string, author: string): Promise<string> {
  await page.goto('/books');
  await page.getByLabel('Title').fill(title);
  await page.getByLabel('Author').fill(author);
  await page.getByRole('button', { name: 'Add book', exact: true }).click();
  const link = page.getByRole('link', { name: title, exact: true });
  await expect(link).toBeVisible();
  await link.click();
  await expect(page.getByRole('heading', { level: 1, name: title, exact: true })).toBeVisible();
  return page.url();
}
