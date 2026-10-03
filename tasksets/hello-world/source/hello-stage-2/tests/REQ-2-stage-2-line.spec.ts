import { expect, test } from '@playwright/test';
import { openHome } from './support/e2e';

test('REQ-2: Stage 2 Line - Scenario 1', async ({ page }) => {
  await openHome(page);
  await expect(page.getByText('Hello, world', { exact: true }).first()).toBeVisible();
  await expect(page.getByText('Stage 2', { exact: true }).first()).toBeVisible();
});
