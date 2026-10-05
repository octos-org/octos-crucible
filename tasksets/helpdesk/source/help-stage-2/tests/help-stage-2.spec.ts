import { test, expect, Page } from '@playwright/test';
import { uid, as, newTicket, row } from './support/help';

const btn = (p: Page, name: string) => p.getByRole('button', { name, exact: true });
async function count(p: Page, label: string): Promise<number> {
  // The count may share an element with other text ("Open: 3 In progress: 1"): read it from the page text.
  const re = new RegExp(`(?:^|[^A-Za-z])${label}:\\s*(\\d+)`);
  await expect(p.locator('body')).toContainText(new RegExp(`${label}:\\s*\\d+`));
  return Number((await p.locator('body').innerText()).match(re)![1]);
}

test('REQ-1..3 still work: tickets, roles and 403 pages', async ({ browser }) => {
  const cy = await as(browser, 'cy');
  const title = `Still works ${uid()}`;
  await newTicket(cy, title, { category: 'Billing', priority: 'Low' });
  for (const t of ['Status: Open', 'Category: Billing', 'Priority: Low', 'Created by: cy']) await expect(cy.getByText(t)).toBeVisible();
  await cy.goto('/');
  await expect(cy.getByRole('link', { name: 'New ticket', exact: true })).toBeVisible();
  expect((await cy.goto('/users'))?.status()).toBe(403);
  const ada = await as(browser, 'ada');
  await expect(ada.getByRole('link', { name: 'Users', exact: true })).toBeVisible();
  await ada.goto('/tickets');
  await expect(row(ada, title)).toHaveCount(1);
});

test('REQ-1..3 still work: search and priority filter', async ({ browser }) => {
  const cy = await as(browser, 'cy'), bob = await as(browser, 'bob');
  const k = uid(), a = `Laptop broken ${k}`, b = `Laptop slow ${k}`;
  await newTicket(cy, a, { priority: 'High' });
  await newTicket(cy, b, { priority: 'Low' });
  await bob.goto('/tickets');
  await bob.getByLabel('Search').fill(`laptop BROKEN ${k}`);
  await bob.getByLabel('Priority filter').selectOption('All');
  await btn(bob, 'Apply').click();
  await expect(row(bob, a)).toHaveCount(1);
  await expect(row(bob, b)).toHaveCount(0);
});

test('REQ-4-1: staff assign tickets; customers only see the assignee', async ({ browser }) => {
  const cy = await as(browser, 'cy'), ada = await as(browser, 'ada');
  const url = await newTicket(cy, `Assign me ${uid()}`);
  await expect(cy.getByText('Assigned to: nobody')).toBeVisible();
  await ada.goto(url);
  await expect(ada.getByText('Assigned to: nobody')).toBeVisible();
  await ada.getByLabel('Assignee').selectOption('bob');
  await btn(ada, 'Assign').click();
  await expect(ada.getByText('Assigned to: bob')).toBeVisible();
  await cy.goto(url);
  await expect(cy.getByText('Assigned to: bob')).toBeVisible();
  await expect(btn(cy, 'Assign')).toHaveCount(0);
  await expect(cy.getByLabel('Assignee')).toHaveCount(0);
});

test('REQ-4-1: the assignee list holds agents only', async ({ browser }) => {
  const cy = await as(browser, 'cy'), bob = await as(browser, 'bob');
  const url = await newTicket(cy, `Who can take this ${uid()}`);
  await bob.goto(url);
  const opts = await bob.getByLabel('Assignee').locator('option').allTextContents();
  const names = opts.map((o) => o.trim());
  expect(names).toContain('bob');
  for (const n of ['ada', 'cy', 'dee']) expect(names).not.toContain(n);
});

test('REQ-4-2: only the allowed moves are offered, through to Closed', async ({ browser }) => {
  const cy = await as(browser, 'cy'), bob = await as(browser, 'bob');
  const url = await newTicket(cy, `Workflow ${uid()}`);
  await expect(btn(cy, 'Close')).toHaveCount(0);
  await expect(btn(cy, 'Start progress')).toHaveCount(0);
  await bob.goto(url);
  await expect(btn(bob, 'Resolve')).toHaveCount(0);
  await expect(btn(bob, 'Reopen')).toHaveCount(0);
  await btn(bob, 'Start progress').click();
  await expect(bob.getByText('Status: In progress')).toBeVisible();
  await expect(btn(bob, 'Start progress')).toHaveCount(0);
  await expect(btn(bob, 'Reopen')).toHaveCount(0);
  await btn(bob, 'Resolve').click();
  await expect(bob.getByText('Status: Resolved')).toBeVisible();
  await expect(btn(bob, 'Close')).toHaveCount(0);
  await cy.goto(url);
  await btn(cy, 'Close').click();
  await expect(cy.getByText('Status: Closed')).toBeVisible();
  for (const n of ['Close', 'Start progress', 'Resolve', 'Reopen']) await expect(btn(cy, n)).toHaveCount(0);
  await bob.goto(url);
  for (const n of ['Close', 'Start progress', 'Resolve', 'Reopen']) await expect(btn(bob, n)).toHaveCount(0);
});

test('REQ-4-2: a resolved ticket can be reopened', async ({ browser }) => {
  const cy = await as(browser, 'cy'), ada = await as(browser, 'ada');
  const url = await newTicket(cy, `Reopen me ${uid()}`);
  await ada.goto(url);
  await btn(ada, 'Start progress').click();
  await btn(ada, 'Resolve').click();
  await btn(ada, 'Reopen').click();
  await expect(ada.getByText('Status: Open')).toBeVisible();
  await expect(btn(ada, 'Start progress')).toBeVisible();
});

test('REQ-4-3: comments in order; internal notes hidden from customers', async ({ browser }) => {
  const cy = await as(browser, 'cy'), bob = await as(browser, 'bob');
  const url = await newTicket(cy, `Talk to me ${uid()}`);
  await cy.getByLabel('Comment').fill('Any news?');
  await btn(cy, 'Add comment').click();
  await expect(cy.getByRole('listitem').filter({ hasText: 'cy: Any news?' })).toHaveCount(1);
  await bob.goto(url);
  await bob.getByLabel('Comment').fill('Customer seems upset');
  await bob.getByLabel('Internal note').check();
  await btn(bob, 'Add comment').click();
  await expect(bob.getByRole('listitem').filter({ hasText: 'bob: Customer seems upset (internal)' })).toHaveCount(1);
  await bob.getByLabel('Comment').fill('Working on it');
  await btn(bob, 'Add comment').click();
  await expect(bob.getByRole('listitem').filter({ hasText: 'bob: Working on it' })).toHaveCount(1);
  const items = (await bob.getByRole('listitem').allInnerTexts()).map((t) => t.trim());
  const i = (s: string) => items.findIndex((t) => t.includes(s));
  expect(i('cy: Any news?')).toBeGreaterThanOrEqual(0);
  expect(i('cy: Any news?')).toBeLessThan(i('bob: Customer seems upset'));
  expect(i('bob: Customer seems upset')).toBeLessThan(i('bob: Working on it'));
  expect(items[i('bob: Working on it')]).not.toContain('(internal)');
  await cy.goto(url);
  await expect(cy.getByRole('listitem').filter({ hasText: 'bob: Working on it' })).toHaveCount(1);
  await expect(cy.getByText('Customer seems upset')).toHaveCount(0);
});

test('REQ-4-3: empty comments are refused; customers have no internal notes', async ({ browser }) => {
  const cy = await as(browser, 'cy');
  await newTicket(cy, `Quiet ticket ${uid()}`);
  await expect(cy.getByLabel('Internal note')).toHaveCount(0);
  await cy.getByLabel('Comment').fill('   ');
  await btn(cy, 'Add comment').click();
  await expect(cy.getByText('Comment cannot be empty')).toBeVisible();
  await expect(cy.getByRole('listitem').filter({ hasText: 'cy:' })).toHaveCount(0);
});

test('REQ-5-1: status filter combines with search', async ({ browser }) => {
  const cy = await as(browser, 'cy'), bob = await as(browser, 'bob');
  const k = uid(), a = `Status one ${k}`, b = `Status two ${k}`;
  const ua = await newTicket(cy, a);
  await newTicket(cy, b);
  await bob.goto(ua);
  await btn(bob, 'Start progress').click();
  await expect(bob.getByText('Status: In progress')).toBeVisible();
  await bob.goto('/tickets');
  await bob.getByLabel('Search').fill(k);
  await bob.getByLabel('Status filter').selectOption('In progress');
  await btn(bob, 'Apply').click();
  await expect(row(bob, a)).toHaveCount(1);
  await expect(row(bob, a)).toContainText('In progress');
  await expect(row(bob, b)).toHaveCount(0);
  await bob.getByLabel('Status filter').selectOption('Open');
  await btn(bob, 'Apply').click();
  await expect(row(bob, b)).toHaveCount(1);
  await expect(row(bob, a)).toHaveCount(0);
  await expect(bob.getByLabel('Status filter')).toHaveValue('Open');
});

test('REQ-5-2: dashboard counts follow new and assigned tickets', async ({ browser }) => {
  const cy = await as(browser, 'cy'), bob = await as(browser, 'bob'), ada = await as(browser, 'ada');
  await bob.getByRole('link', { name: 'Dashboard', exact: true }).click();
  await expect(bob).toHaveURL(/\/dashboard$/);
  await expect(bob.getByRole('heading', { level: 1, name: 'Dashboard', exact: true })).toBeVisible();
  const open = await count(bob, 'Open'), mine = await count(bob, 'Assigned to me');
  for (const l of ['In progress', 'Resolved', 'Closed']) await count(bob, l);
  const url = await newTicket(cy, `Count me ${uid()}`);
  await ada.goto(url);
  await ada.getByLabel('Assignee').selectOption('bob');
  await btn(ada, 'Assign').click();
  await expect(ada.getByText('Assigned to: bob')).toBeVisible();
  await bob.reload();
  expect(await count(bob, 'Open')).toBe(open + 1);
  expect(await count(bob, 'Assigned to me')).toBe(mine + 1);
});

test('REQ-5-2: dashboard is for staff only', async ({ browser, page }) => {
  await page.goto('/dashboard');
  await expect(page).toHaveURL(/\/login/);
  const cy = await as(browser, 'cy');
  await expect(cy.getByRole('link', { name: 'Dashboard', exact: true })).toHaveCount(0);
  const res = await cy.goto('/dashboard');
  expect(res?.status()).toBe(403);
  await expect(cy.getByText('Forbidden').first()).toBeVisible();
  const ada = await as(browser, 'ada');
  await expect(ada.getByRole('link', { name: 'Dashboard', exact: true })).toBeVisible();
});
