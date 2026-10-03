import { expect, test } from '@playwright/test';
import { openHome } from './support/e2e';

test('REQ-1: Hello World Page - Scenario 1', async ({ page }) => {
  await openHome(page);
  await expect(page.getByText('Hello, world', { exact: true }).first()).toBeVisible();
});
