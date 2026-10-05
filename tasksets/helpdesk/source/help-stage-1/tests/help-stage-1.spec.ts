import { test, expect } from '@playwright/test';
import { uid, signIn, as, newTicket, row } from './support/help';

test('REQ-1-1: a seeded user signs in and stays signed in', async ({ page }) => {
  await page.goto('/login');
  await expect(page.getByRole('heading', { level: 1, name: 'Sign in', exact: true })).toBeVisible();
  await signIn(page, 'bob');
  await page.reload();
  await expect(page.getByText('Signed in as bob (agent)')).toBeVisible();
});

test('REQ-1-1: wrong password is refused', async ({ page }) => {
  await page.goto('/login');
  await page.getByLabel('Username').fill('cy');
  await page.getByLabel('Password').fill('nope-nope');
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page.getByText('Invalid username or password')).toBeVisible();
  await expect(page).toHaveURL(/\/login/);
});

test('REQ-1-1: signed-in pages send visitors to the sign-in page', async ({ page }) => {
  await page.goto('/tickets');
  await expect(page).toHaveURL(/\/login/);
  await page.goto('/tickets/new');
  await expect(page).toHaveURL(/\/login/);
});

test('REQ-1-2: customer navigation', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByRole('heading', { level: 1, name: 'Helpdesk', exact: true })).toBeVisible();
  await expect(page.getByRole('link', { name: 'Sign in', exact: true })).toBeVisible();
  await signIn(page, 'cy');
  await expect(page.getByText('Signed in as cy (customer)')).toBeVisible();
  await expect(page.getByRole('link', { name: 'Tickets', exact: true })).toBeVisible();
  await expect(page.getByRole('link', { name: 'Users', exact: true })).toHaveCount(0);
  await page.getByRole('link', { name: 'New ticket', exact: true }).click();
  await expect(page).toHaveURL(/\/tickets\/new$/);
});

test('REQ-1-2: admin and agent navigation, sign out', async ({ browser }) => {
  const ada = await as(browser, 'ada');
  await expect(ada.getByText('Signed in as ada (admin)')).toBeVisible();
  await expect(ada.getByRole('link', { name: 'Tickets', exact: true })).toBeVisible();
  await expect(ada.getByRole('link', { name: 'New ticket', exact: true })).toHaveCount(0);
  await ada.getByRole('link', { name: 'Users', exact: true }).click();
  await expect(ada).toHaveURL(/\/users$/);
  const bob = await as(browser, 'bob');
  await expect(bob.getByRole('link', { name: 'Tickets', exact: true })).toBeVisible();
  await expect(bob.getByRole('link', { name: 'New ticket', exact: true })).toHaveCount(0);
  await expect(bob.getByRole('link', { name: 'Users', exact: true })).toHaveCount(0);
  await bob.getByRole('button', { name: 'Sign out', exact: true }).click();
  await expect(bob.getByRole('link', { name: 'Sign in', exact: true })).toBeVisible();
});

test('REQ-2-1: invalid ticket shows every error and keeps the values', async ({ browser }) => {
  const cy = await as(browser, 'cy');
  const title = `Hi${uid()}`.slice(0, 4);
  await cy.goto('/tickets/new');
  await expect(cy.getByRole('heading', { level: 1, name: 'New ticket', exact: true })).toBeVisible();
  await cy.getByLabel('Title').fill(title);
  await cy.getByLabel('Description').fill('too short');
  await cy.getByRole('button', { name: 'Submit ticket', exact: true }).click();
  await expect(cy.getByText('Title must be 5 to 80 characters')).toBeVisible();
  await expect(cy.getByText('Description must be at least 20 characters')).toBeVisible();
  await expect(cy.getByLabel('Title')).toHaveValue(title);
  await expect(cy.getByLabel('Description')).toHaveValue('too short');
  await cy.goto('/tickets');
  await expect(cy.getByRole('link', { name: title, exact: true })).toHaveCount(0);
});

test('REQ-2-1: a valid ticket is stored, Normal is the default priority', async ({ browser }) => {
  const cy = await as(browser, 'cy');
  await cy.goto('/tickets/new');
  await expect(cy.getByRole('group', { name: 'Priority' }).getByRole('radio', { name: 'Normal', exact: true })).toBeChecked();
  const title = `Cannot log in ${uid()}`;
  await newTicket(cy, title, { category: 'Account' });
  await expect(cy).toHaveURL(/\/tickets\/[^/]+$/);
  await expect(cy.getByText('Status: Open')).toBeVisible();
  await expect(cy.getByText('Category: Account')).toBeVisible();
  await expect(cy.getByText('Priority: Normal')).toBeVisible();
  await expect(cy.getByText('Created by: cy')).toBeVisible();
});

test('REQ-2-1: staff may not open the new-ticket page', async ({ browser }) => {
  for (const user of ['bob', 'ada']) {
    const page = await as(browser, user);
    const res = await page.goto('/tickets/new');
    expect(res?.status()).toBe(403);
    await expect(page.getByText('Forbidden')).toBeVisible();
  }
});

test('REQ-2-2: staff see every detail of a ticket', async ({ browser }) => {
  const cy = await as(browser, 'cy');
  const title = `Printer is on fire ${uid()}`;
  const url = await newTicket(cy, title, { category: 'Technical', priority: 'High' });
  const bob = await as(browser, 'bob');
  await bob.goto(url);
  await expect(bob.getByRole('heading', { level: 1, name: title, exact: true })).toBeVisible();
  for (const t of ['Status: Open', 'Category: Technical', 'Priority: High', 'Created by: cy']) await expect(bob.getByText(t)).toBeVisible();
});

test('REQ-2-2: other customers get 403, unknown tickets 404', async ({ browser }) => {
  const cy = await as(browser, 'cy');
  const url = await newTicket(cy, `Private matter ${uid()}`);
  const dee = await as(browser, 'dee');
  const res = await dee.goto(url);
  expect(res?.status()).toBe(403);
  await expect(dee.getByText('Forbidden')).toBeVisible();
  const res2 = await dee.goto('/tickets/no-such-ticket-424242');
  expect(res2?.status()).toBe(404);
  await expect(dee.getByText('Ticket not found')).toBeVisible();
});

test('REQ-2-3: the list shows what each role may see, newest first', async ({ browser }) => {
  const cy = await as(browser, 'cy'), dee = await as(browser, 'dee'), bob = await as(browser, 'bob');
  const a = `Older cy ticket ${uid()}`, b = `Newer dee ticket ${uid()}`;
  await newTicket(cy, a);
  await newTicket(dee, b);
  await cy.goto('/tickets');
  await expect(cy.getByRole('heading', { level: 1, name: 'Tickets', exact: true })).toBeVisible();
  for (const h of ['Title', 'Status', 'Priority', 'Created by']) await expect(cy.getByRole('columnheader', { name: h, exact: true })).toBeVisible();
  await expect(row(cy, a)).toHaveCount(1);
  await expect(row(cy, b)).toHaveCount(0);
  await dee.goto('/tickets');
  await expect(row(dee, b)).toHaveCount(1);
  await expect(row(dee, a)).toHaveCount(0);
  await bob.goto('/tickets');
  await expect(row(bob, a)).toContainText('cy');
  await expect(row(bob, b)).toContainText('dee');
  const titles = await bob.getByRole('row').allInnerTexts();
  expect(titles.findIndex((t) => t.includes(b))).toBeLessThan(titles.findIndex((t) => t.includes(a)));
  await row(bob, a).getByRole('link', { name: a, exact: true }).click();
  await expect(bob.getByRole('heading', { level: 1, name: a, exact: true })).toBeVisible();
});

test('REQ-2-4: search and priority filter', async ({ browser }) => {
  const cy = await as(browser, 'cy'), bob = await as(browser, 'bob');
  const k = uid();
  const low = `Refund please ${k}`, high = `Refund again ${k}`, other = `Something else ${k}`;
  await newTicket(cy, low, { priority: 'Low' });
  await newTicket(cy, high, { priority: 'High' });
  await newTicket(cy, other, { priority: 'High' });
  await bob.goto('/tickets');
  await bob.getByLabel('Search').fill(`REFUND AGAIN ${k.toUpperCase()}`.slice(0, 12));
  await bob.getByLabel('Priority filter').selectOption('High');
  await bob.getByRole('button', { name: 'Apply', exact: true }).click();
  await expect(row(bob, high)).toHaveCount(1);
  await expect(row(bob, low)).toHaveCount(0);
  await expect(row(bob, other)).toHaveCount(0);
  await expect(bob.getByLabel('Search')).toHaveValue('REFUND AGAIN');
  await expect(bob.getByLabel('Priority filter')).toHaveValue('High');
  await bob.getByLabel('Search').fill(k);
  await bob.getByLabel('Priority filter').selectOption('All');
  await bob.getByRole('button', { name: 'Apply', exact: true }).click();
  for (const t of [low, high, other]) await expect(row(bob, t)).toHaveCount(1);
});

test('REQ-3: an admin creates a user who can sign in; others get 403', async ({ browser }) => {
  const ada = await as(browser, 'ada');
  const name = `eve${uid()}`;
  await ada.goto('/users');
  await expect(ada.getByRole('heading', { level: 1, name: 'Users', exact: true })).toBeVisible();
  await expect(ada.getByRole('row').filter({ hasText: 'bob' })).toContainText('agent');
  await ada.getByLabel('Username').fill(name);
  await ada.getByLabel('Password').fill('secret1');
  await ada.getByLabel('Role').selectOption('agent');
  await ada.getByRole('button', { name: 'Create user', exact: true }).click();
  await expect(ada.getByRole('row').filter({ hasText: name })).toContainText('agent');
  const eve = await as(browser, name, 'secret1');
  await expect(eve.getByText(`Signed in as ${name} (agent)`)).toBeVisible();
  const res = await eve.goto('/users');
  expect(res?.status()).toBe(403);
});

test('REQ-3: invalid users are refused with every message', async ({ browser }) => {
  const ada = await as(browser, 'ada');
  await ada.goto('/users');
  await ada.getByLabel('Username').fill('b!');
  await ada.getByLabel('Password').fill('123');
  await ada.getByRole('button', { name: 'Create user', exact: true }).click();
  await expect(ada.getByText('Username must be 3 to 20 letters or digits')).toBeVisible();
  await expect(ada.getByText('Password must be at least 6 characters')).toBeVisible();
  await ada.goto('/users');
  await ada.getByLabel('Username').fill('bob');
  await ada.getByLabel('Password').fill('longenough');
  await ada.getByRole('button', { name: 'Create user', exact: true }).click();
  await expect(ada.getByText('Username already taken')).toBeVisible();
  await expect(ada.getByRole('cell', { name: 'b!', exact: true })).toHaveCount(0);
});
