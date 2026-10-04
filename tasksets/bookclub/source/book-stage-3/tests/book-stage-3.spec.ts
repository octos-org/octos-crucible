import { test, expect, Page } from '@playwright/test';
import { uid, signIn, register, signOut, addBook } from './support/book';

async function postReview(page: Page, url: string, rating: string, text: string): Promise<void> {
  await page.goto(url);
  await page.getByLabel('Rating').selectOption(rating);
  await page.getByLabel('Review').fill(text);
  await page.getByRole('button', { name: 'Post review', exact: true }).click();
  await expect(page.getByRole('listitem').filter({ hasText: text })).toHaveCount(1);
}

test('REQ-1..6 still work: accounts, adding a book, book page', async ({ page }) => {
  await signIn(page, 'alice', 'alice123');
  await signOut(page);
  const user = `user${uid()}`, title = `Book ${uid()}`;
  await register(page, user, 'pw');
  await addBook(page, title, 'An Author');
  await expect(page.getByText(`Added by: ${user}`)).toBeVisible();
  const res = await page.goto('/books/no-such-book-999999');
  expect(res?.status()).toBe(404);
});

test('REQ-7: posting a review shows it; posting again replaces it', async ({ page }) => {
  const user = `user${uid()}`, title = `Book ${uid()}`;
  await register(page, user, 'pw');
  const url = await addBook(page, title, 'Writer');
  await postReview(page, url, '4', 'First impressions');
  await expect(page.getByRole('listitem').filter({ hasText: `${user} rated 4/5` })).toHaveCount(1);
  await postReview(page, url, '2', 'Second thoughts');
  await page.reload();
  await expect(page.getByRole('listitem').filter({ hasText: `${user} rated` })).toHaveCount(1);
  await expect(page.getByRole('listitem').filter({ hasText: `${user} rated 2/5` })).toHaveCount(1);
  await expect(page.getByText('First impressions')).toHaveCount(0);
});

test('REQ-7: signed-out visitors see no form', async ({ page }) => {
  await page.goto('/books');
  await page.getByRole('link', { name: 'Dune', exact: true }).click();
  await expect(page.getByText('Sign in to review')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Post review', exact: true })).toHaveCount(0);
});

test('REQ-8: average rating on the book page and in the list', async ({ page }) => {
  const a = `user${uid()}`, b = `user${uid()}`, title = `Book ${uid()}`;
  await register(page, a, 'pw');
  const url = await addBook(page, title, 'Writer');
  await expect(page.getByText('No ratings yet')).toBeVisible();
  const item = () => page.getByRole('listitem').filter({ has: page.getByRole('link', { name: title, exact: true }) });
  await page.goto('/books');
  await expect(item()).toContainText('No ratings yet');
  await postReview(page, url, '4', 'Good');
  await expect(page.getByText('Average rating: 4.0')).toBeVisible();
  await signOut(page);
  await register(page, b, 'pw');
  await postReview(page, url, '5', 'Better');
  await expect(page.getByText('Average rating: 4.5')).toBeVisible();
  await page.goto('/books');
  await expect(item()).toContainText('Average rating: 4.5');
});

test('REQ-9: My reviews lists the signed-in user\'s reviews', async ({ page }) => {
  const user = `user${uid()}`, title = `Book ${uid()}`;
  await register(page, user, 'pw');
  const url = await addBook(page, title, 'Writer');
  await postReview(page, url, '5', 'Loved it');
  await page.goto('/');
  await page.getByRole('link', { name: 'My reviews', exact: true }).click();
  await expect(page).toHaveURL(/\/me\/?$/);
  await expect(page.getByRole('heading', { level: 1, name: 'My reviews', exact: true })).toBeVisible();
  const item = page.getByRole('listitem').filter({ has: page.getByRole('link', { name: title, exact: true }) });
  await expect(item).toContainText('rated 5/5');
  await item.getByRole('link', { name: title, exact: true }).click();
  await expect(page.getByRole('heading', { level: 1, name: title, exact: true })).toBeVisible();
});

test('REQ-9: My reviews needs a sign-in', async ({ page }) => {
  await page.goto('/me');
  await expect(page).toHaveURL(/\/login\/?$/);
});
