import { test, expect } from '@playwright/test';
import { uid, signIn, register, signOut, addBook } from './support/book';

test('REQ-1..3 still work: sign in, sign out, create account', async ({ page }) => {
  await signIn(page, 'alice', 'alice123');
  await signOut(page);
  await register(page, `user${uid()}`, 'pw');
});

test('REQ-4: Books link and the seeded book, signed out', async ({ page }) => {
  await page.goto('/');
  await page.getByRole('link', { name: 'Books', exact: true }).click();
  await expect(page).toHaveURL(/\/books\/?$/);
  await expect(page.getByRole('heading', { level: 1, name: 'Books', exact: true })).toBeVisible();
  await expect(page.getByRole('listitem').filter({ has: page.getByRole('link', { name: 'Dune', exact: true }) })).toHaveCount(1);
});

test('REQ-6: book page of the seeded book', async ({ page }) => {
  await page.goto('/books');
  await page.getByRole('link', { name: 'Dune', exact: true }).click();
  await expect(page).toHaveURL(/\/books\/[^/]+$/);
  await expect(page.getByRole('heading', { level: 1, name: 'Dune', exact: true })).toBeVisible();
  await expect(page.getByText('Author: Frank Herbert')).toBeVisible();
  await expect(page.getByText('Added by: alice')).toBeVisible();
  await page.getByRole('link', { name: 'All books', exact: true }).click();
  await expect(page).toHaveURL(/\/books\/?$/);
});

test('REQ-5: a signed-in user adds a book', async ({ page }) => {
  const user = `user${uid()}`, title = `Book ${uid()}`;
  await register(page, user, 'pw');
  await addBook(page, title, 'Some Author');
  await expect(page.getByText('Author: Some Author')).toBeVisible();
  await expect(page.getByText(`Added by: ${user}`)).toBeVisible();
  await signOut(page);
  await page.goto('/books');
  await expect(page.getByRole('link', { name: title, exact: true })).toBeVisible();
});

test('REQ-5: no form when signed out; empty fields refused', async ({ page }) => {
  await page.goto('/books');
  await expect(page.getByRole('heading', { level: 1, name: 'Books', exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Add book', exact: true })).toHaveCount(0);
  await signIn(page, 'alice', 'alice123');
  await page.goto('/books');
  const before = await page.getByRole('listitem').count();
  await page.getByLabel('Title').fill(`Lonely ${uid()}`);
  await page.getByRole('button', { name: 'Add book', exact: true }).click();
  await expect(page.getByText('Title and author are required')).toBeVisible();
  await page.goto('/books');
  await expect(page.getByRole('listitem')).toHaveCount(before);
});

test('REQ-6: unknown book is a 404', async ({ page }) => {
  const res = await page.goto('/books/no-such-book-999999');
  expect(res?.status()).toBe(404);
  await expect(page.getByText('Book not found')).toBeVisible();
});
