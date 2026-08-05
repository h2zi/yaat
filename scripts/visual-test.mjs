import { spawn } from "node:child_process";
import { mkdir } from "node:fs/promises";
import path from "node:path";

import { chromium } from "playwright";

const output = process.argv[2] ?? "/output";
const url = "http://127.0.0.1:4173";

await mkdir(output, { recursive: true });

const server = spawn(
  "pnpm",
  ["run", "preview", "--host", "127.0.0.1", "--port", "4173", "--strictPort"],
  { stdio: "inherit", env: { ...process.env, VITE_YAAT_PREVIEW: "1" } },
);

async function waitForServer() {
  for (let attempt = 0; attempt < 80; attempt += 1) {
    try {
      const response = await fetch(url);
      if (response.ok) return;
    } catch {
      // The Vite preview process is still starting.
    }
    await new Promise((resolve) => setTimeout(resolve, 150));
  }
  throw new Error("Vite preview server did not become ready");
}

let browser;
try {
  await waitForServer();
  browser = await chromium.launch({ headless: true });
  const context = await browser.newContext({
    viewport: { width: 1180, height: 760 },
    deviceScaleFactor: 1,
    timezoneId: "Pacific/Honolulu",
  });
  const page = await context.newPage();
  await page.clock.install({ time: new Date("2026-08-03T16:30:00Z") });
  const consoleErrors = [];
  page.on("console", (message) => {
    if (message.type() === "error") consoleErrors.push(message.text());
  });
  page.on("pageerror", (error) => consoleErrors.push(error.message));

  await page.goto(url, { waitUntil: "networkidle" });
  await page.evaluate(() => document.fonts.ready);
  await page.getByRole("heading", { name: "账号与 Provider" }).waitFor();
  await page.screenshot({
    path: path.join(output, "accounts-light.png"),
    fullPage: true,
  });

  await page.getByRole("button", { name: "启动", exact: true }).first().click();
  await page.getByRole("dialog").getByText("启动项目会话").waitFor();
  await page.getByLabel("项目目录（绝对路径）").fill("/workspace");
  await page.screenshot({
    path: path.join(output, "launch-project-dialog.png"),
    fullPage: true,
  });
  await page.keyboard.press("Escape");

  await page.getByRole("button", { name: "全局切换", exact: true }).click();
  await page
    .getByRole("button", { name: "停止全局管理", exact: true })
    .waitFor();
  const globalMode = page.getByRole("button", {
    name: "全局切换",
    exact: true,
  });
  const reapply = page.getByRole("button", { name: "重新应用", exact: true });
  await reapply.waitFor();
  if (await reapply.isDisabled())
    throw new Error("The active global provider must remain re-applicable");
  await reapply.click();
  const switchDialog = page.getByRole("alertdialog");
  await switchDialog.getByRole("button", { name: "切换", exact: true }).click();
  await switchDialog.waitFor({ state: "hidden" });
  await page
    .getByRole("button", { name: "停止全局管理", exact: true })
    .waitFor();
  if (!(await globalMode.getAttribute("class"))?.includes("bg-muted")) {
    throw new Error(
      "A completed global switch must not reset the selected activation mode",
    );
  }
  if (
    (await page
      .getByRole("button", { name: "重新登录", exact: true })
      .count()) === 0
  ) {
    throw new Error(
      "Ready official accounts must expose a reauthentication action",
    );
  }
  await page.screenshot({
    path: path.join(output, "accounts-global-light.png"),
    fullPage: true,
  });

  await page.getByRole("combobox").first().click();
  await page.getByRole("option", { name: "Claude Desktop" }).click();
  await page.waitForTimeout(250);
  await page.getByText("Desktop Personal").waitFor();
  if ((await page.getByRole("tab", { name: "使用统计" }).count()) !== 0) {
    throw new Error(
      "Claude Desktop must not expose unsupported local usage statistics",
    );
  }
  await page.screenshot({
    path: path.join(output, "claude-desktop-light.png"),
    fullPage: true,
  });

  await page.getByRole("combobox").first().click();
  await page.getByRole("option", { name: "Codex" }).click();
  await page.waitForTimeout(250);

  await page.getByRole("tab", { name: "使用统计" }).click();
  await page.getByText("每日使用趋势").waitFor();
  await page.locator(".recharts-responsive-container").waitFor();
  const dateInputs = page.locator('input[type="date"]');
  if ((await dateInputs.nth(1).inputValue()) !== "2026-08-04") {
    throw new Error(
      "Usage presets must use the configured Asia/Taipei calendar date",
    );
  }
  const usageTrendCard = page
    .getByRole("heading", { name: "每日使用趋势" })
    .locator('xpath=ancestor::*[@data-slot="card"]');
  const rangeIndicator = page.locator("[data-usage-range-indicator]");
  const beforeRangeSwitch = await usageTrendCard.boundingBox();
  const beforeTransform = await rangeIndicator.evaluate(
    (element) => getComputedStyle(element).transform,
  );
  await page.getByRole("button", { name: "30 天", exact: true }).click();
  await page.waitForFunction(
    () => document.querySelector('input[type="date"]')?.value === "2026-07-06",
  );
  await page.waitForTimeout(250);
  const afterRangeSwitch = await usageTrendCard.boundingBox();
  const afterTransform = await rangeIndicator.evaluate(
    (element) => getComputedStyle(element).transform,
  );
  if (
    !beforeRangeSwitch ||
    !afterRangeSwitch ||
    Math.abs(beforeRangeSwitch.y - afterRangeSwitch.y) > 1
  ) {
    throw new Error("Usage range switches must not shift the dashboard layout");
  }
  if (beforeTransform === afterTransform) {
    throw new Error("Usage range selection must animate its active indicator");
  }
  await page.screenshot({
    path: path.join(output, "usage-light.png"),
    fullPage: true,
  });

  await page.getByRole("tab", { name: "账号管理" }).click();
  if ((await dateInputs.first().inputValue()) !== "2026-07-06") {
    throw new Error(
      "Usage filters must remain mounted while another tab is active",
    );
  }
  await page.getByRole("tab", { name: "使用统计" }).click();
  await page.getByRole("heading", { name: "每日使用趋势" }).waitFor();
  if ((await dateInputs.first().inputValue()) !== "2026-07-06") {
    throw new Error(
      "Usage filters must survive account and usage tab switches",
    );
  }
  await page.getByRole("tab", { name: "账号管理" }).click();
  await page.getByRole("button", { name: "添加账号" }).first().click();
  const createProviderDialog = page.getByRole("dialog");
  await createProviderDialog.waitFor();
  const officialCredential = createProviderDialog.getByLabel("官方账号凭据");
  await officialCredential.waitFor();
  await page.screenshot({
    path: path.join(output, "add-account-dialog.png"),
    fullPage: true,
  });

  await createProviderDialog.getByLabel("显示名称").fill("Pasted official");
  await officialCredential.fill(
    JSON.stringify({
      format: "yaat.official-credential",
      version: 1,
      platform: "codex",
      storageKind: "codex.auth-json.v1",
      accountLabel: "copied@example.com",
      credential: { auth_mode: "chatgpt", tokens: { access_token: "copied" } },
    }),
  );
  await createProviderDialog
    .getByRole("button", { name: "创建", exact: true })
    .click();
  await createProviderDialog.waitFor({ state: "hidden" });
  const pastedCard = page
    .getByRole("heading", { name: "Pasted official" })
    .locator('xpath=ancestor::*[@data-slot="card"]');
  await pastedCard.getByText("可用", { exact: true }).waitFor();

  const personalCard = page
    .getByRole("heading", { name: "Personal", exact: true })
    .locator('xpath=ancestor::*[@data-slot="card"]');
  await personalCard.getByRole("button", { name: "更多" }).click();
  await page.getByRole("menuitem", { name: "编辑" }).click();
  const editProviderDialog = page.getByRole("dialog");
  const revealedCredential = editProviderDialog.getByLabel("官方账号凭据");
  await revealedCredential.waitFor();
  await page.waitForFunction(() =>
    document
      .querySelector('textarea[id="official-credential"]')
      ?.value.includes("yaat.official-credential"),
  );
  if (
    await editProviderDialog
      .getByRole("button", { name: "复制凭据" })
      .isDisabled()
  ) {
    throw new Error("Saved official credentials must be copyable from edit");
  }
  await page.screenshot({
    path: path.join(output, "edit-official-credential-dialog.png"),
    fullPage: true,
  });
  await page.keyboard.press("Escape");

  await page.getByRole("button", { name: "设置" }).click();
  await page.getByRole("dialog").waitFor();
  await page.getByRole("switch", { name: "Codex 统一会话历史" }).click();
  await page.getByText("38 份已同步").waitFor();
  await page.getByRole("button", { name: "扫描账号" }).click();
  await page.getByLabel("目标账号 / 组织").click();
  await page.getByRole("option", { name: /当前登录/ }).click();
  await page
    .getByRole("switch", { name: "Claude Desktop Code 会话统一" })
    .click();
  await page.getByText("9 份已同步").waitFor();
  const desktopHistorySwitch = await page
    .getByRole("switch", { name: "Claude Desktop Code 会话统一" })
    .boundingBox();
  const scanAccountsButton = await page
    .getByRole("button", { name: "扫描账号" })
    .boundingBox();
  if (
    !desktopHistorySwitch ||
    !scanAccountsButton ||
    Math.abs(
      desktopHistorySwitch.y +
        desktopHistorySwitch.height / 2 -
        (scanAccountsButton.y + scanAccountsButton.height / 2),
    ) > 1
  ) {
    throw new Error("Claude Desktop history controls must be center-aligned");
  }
  await page.screenshot({
    path: path.join(output, "settings-history-light.png"),
    fullPage: true,
  });

  await page.getByLabel("主题").click();
  await page.getByRole("option", { name: "深色" }).click();
  await page.waitForTimeout(250);
  if (
    await page
      .getByRole("option", { name: "深色" })
      .isVisible()
      .catch(() => false)
  ) {
    await page.keyboard.press("Escape");
  }
  await page.screenshot({
    path: path.join(output, "settings-dark.png"),
    fullPage: true,
  });

  if (consoleErrors.length > 0) {
    throw new Error(`Browser console errors:\n${consoleErrors.join("\n")}`);
  }
} finally {
  await browser?.close();
  server.kill("SIGTERM");
}
