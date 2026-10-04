import { test, expect } from '@playwright/test';
import { uid, signIn, register, signOut } from './support/book';

test('REQ-1: home page heading and account links', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByRole('heading', { level: 1, name: 'Book Club', exact: true })).toBeVisible();
  await page.getByRole('link', { name: 'Sign in', exact: true }).click();
  await expect(page).toHaveURL(/\/login\/?$/);
  await page.goto('/');
  await page.getByRole('link', { name: 'Create account', exact: true }).click();
  await expect(page).toHaveURL(/\/register\/?$/);
});

test('REQ-2: alice signs in, stays signed in after reload, signs out', async ({ page }) => {
  await page.goto('/login');
  await expect(page.getByRole('heading', { level: 1, name: 'Sign in', exact: true })).toBeVisible();
  await signIn(page, 'alice', 'alice123');
  await expect(page).toHaveURL(/\/$/);
  await expect(page.getByRole('link', { name: 'Sign in', exact: true })).toHaveCount(0);
  await page.reload();
  await expect(page.getByText('Signed in as alice')).toBeVisible();
  await signOut(page);
  await expect(page.getByText('Signed in as alice').filter({ visible: true })).toHaveCount(0);
});

test('REQ-2: wrong password is refused', async ({ page }) => {
  await page.goto('/login');
  await page.getByLabel('Username').fill('alice');
  await page.getByLabel('Password').fill('wrong-password');
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page.getByText('Invalid username or password')).toBeVisible();
  await page.goto('/');
  await expect(page.getByText('Signed in as alice').filter({ visible: true })).toHaveCount(0);
});

test('REQ-3: a new account is signed in and can sign in again', async ({ page }) => {
  const user = `user${uid()}`;
  await page.goto('/register');
  await expect(page.getByRole('heading', { level: 1, name: 'Create account', exact: true })).toBeVisible();
  await register(page, user, 'pw-123');
  await signOut(page);
  await signIn(page, user, 'pw-123');
});

test('REQ-3: taken or empty usernames are refused', async ({ page }) => {
  await page.goto('/register');
  await page.getByLabel('Username').fill('alice');
  await page.getByLabel('Password').fill('whatever');
  await page.getByRole('button', { name: 'Create account', exact: true }).click();
  await expect(page.getByText('Username already taken')).toBeVisible();
  await page.goto('/register');
  await page.getByLabel('Password').fill('whatever');
  await page.getByRole('button', { name: 'Create account', exact: true }).click();
  await expect(page.getByText('Username and password are required')).toBeVisible();
  await page.goto('/');
  await expect(page.getByText(/Signed in as/).filter({ visible: true })).toHaveCount(0);
});
