import { spawn } from "node:child_process";
import { mkdir } from "node:fs/promises";
import path from "node:path";

import { chromium } from "playwright";

const output = process.argv[2] ?? path.resolve("output");
const url = "http://127.0.0.1:4173";

await mkdir(output, { recursive: true });

const viteCli = path.resolve("node_modules/vite/bin/vite.js");
const server = spawn(
  process.execPath,
  [viteCli, "--host", "127.0.0.1", "--port", "4173", "--strictPort"],
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

  await page.getByRole("button", { name: "导入当前账号" }).click();
  const importDialog = page
    .getByRole("dialog")
    .filter({ hasText: "导入当前账号与配置" });
  await importDialog.getByText("当前生效").waitFor();
  await importDialog.getByText("已发现的官方登录").waitFor();
  await page.screenshot({
    path: path.join(output, "provider-import-preview.png"),
    fullPage: true,
  });
  await importDialog.getByRole("button", { name: "检查配置" }).first().click();
  const reviewDialog = page
    .getByRole("dialog")
    .filter({ hasText: "检查导入配置" });
  await reviewDialog
    .locator('input[value="https://gateway.example.com/v1"]')
    .waitFor();
  await reviewDialog.getByText("凭据", { exact: true }).waitFor();
  await page.screenshot({
    path: path.join(output, "provider-import-review.png"),
    fullPage: true,
  });
  await page.keyboard.press("Escape");
  await reviewDialog.waitFor({ state: "hidden" });
  await page.keyboard.press("Escape");
  await importDialog.waitFor({ state: "hidden" });

  await page.getByRole("combobox").first().click();
  await page.getByRole("option", { name: "Claude Code" }).click();
  await page.getByText("Claude Max").waitFor();
  await page.getByText("CC", { exact: true }).waitFor();
  await page.getByRole("button", { name: "导入当前账号" }).click();
  const claudeImportDialog = page
    .getByRole("dialog")
    .filter({ hasText: "导入当前账号与配置" });
  await claudeImportDialog
    .getByRole("button", { name: "检查配置" })
    .first()
    .click();
  const claudeReviewDialog = page
    .getByRole("dialog")
    .filter({ hasText: "检查导入配置" });
  const fableMapping = claudeReviewDialog.getByLabel("Fable");
  await fableMapping.waitFor();
  if ((await fableMapping.inputValue()) !== "claude-fable-4-5") {
    throw new Error("Claude Code import must preserve the Fable model mapping");
  }
  await page.screenshot({
    path: path.join(output, "claude-code-provider-import-review.png"),
    fullPage: true,
  });
  await page.keyboard.press("Escape");
  await claudeReviewDialog.waitFor({ state: "hidden" });
  await page.keyboard.press("Escape");
  await claudeImportDialog.waitFor({ state: "hidden" });
  await page.getByRole("combobox").first().click();
  await page.getByRole("option", { name: "Codex" }).click();
  await page.waitForTimeout(250);

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
  const dateRangeButton = page.getByRole("button", {
    name: /2026-07-29 — 2026-08-04/,
  });
  const modelFilter = page
    .getByRole("combobox")
    .filter({ hasText: "全部模型" });
  const [dateControl, modelControl] = await Promise.all([
    dateRangeButton.evaluate((element) => {
      const style = getComputedStyle(element);
      return {
        height: element.getBoundingClientRect().height,
        borderRadius: style.borderRadius,
        borderWidth: style.borderWidth,
        borderStyle: style.borderStyle,
        backgroundColor: style.backgroundColor,
        boxShadow: style.boxShadow,
      };
    }),
    modelFilter.evaluate((element) => {
      const style = getComputedStyle(element);
      return {
        height: element.getBoundingClientRect().height,
        borderRadius: style.borderRadius,
        borderWidth: style.borderWidth,
        borderStyle: style.borderStyle,
        backgroundColor: style.backgroundColor,
        boxShadow: style.boxShadow,
      };
    }),
  ]);
  if (JSON.stringify(dateControl) !== JSON.stringify(modelControl)) {
    throw new Error(
      `Usage date and model controls must share the same frame style: ${JSON.stringify({ dateControl, modelControl })}`,
    );
  }
  await dateRangeButton.click();
  await page.waitForTimeout(200);
  if (
    (await page
      .locator('[data-date="2026-08-04"][data-range-end="true"]')
      .count()) !== 1
  ) {
    throw new Error(
      "Usage presets must use the configured Asia/Taipei calendar date",
    );
  }
  await page.screenshot({
    path: path.join(output, "usage-date-picker-light.png"),
    fullPage: true,
  });
  await page.getByRole("button", { name: "30 天", exact: true }).click();
  await page.waitForFunction(() =>
    Array.from(document.querySelectorAll("button")).some((button) =>
      button.textContent?.includes("2026-07-06 — 2026-08-04"),
    ),
  );
  await page.waitForTimeout(250);
  await page.screenshot({
    path: path.join(output, "usage-light.png"),
    fullPage: true,
  });

  await page.getByRole("tab", { name: "账号管理" }).click();
  if ((await page.getByText("2026-07-06 — 2026-08-04").count()) === 0) {
    throw new Error(
      "Usage filters must remain mounted while another tab is active",
    );
  }
  await page.getByRole("tab", { name: "使用统计" }).click();
  await page.getByRole("heading", { name: "每日使用趋势" }).waitFor();
  if ((await page.getByText("2026-07-06 — 2026-08-04").count()) === 0) {
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

  await page.getByRole("button", { name: "添加账号" }).first().click();
  const thirdPartyDialog = page.getByRole("dialog");
  await thirdPartyDialog.getByLabel("Provider 类型").click();
  await page.getByRole("option", { name: "第三方" }).click();
  await thirdPartyDialog.getByLabel("显示名称").fill("Custom Responses");
  await thirdPartyDialog
    .getByLabel("Base URL")
    .fill("https://api.example.test/v1");
  await thirdPartyDialog.getByLabel("凭据", { exact: true }).fill("sk-preview");
  await thirdPartyDialog.getByLabel("默认模型").fill("gpt-custom");
  if (
    await thirdPartyDialog
      .getByRole("button", { name: "获取模型" })
      .isDisabled()
  ) {
    throw new Error(
      "Model discovery must enable after Base URL and credential are set",
    );
  }
  await page.screenshot({
    path: path.join(output, "third-party-provider-top.png"),
    fullPage: true,
  });
  await thirdPartyDialog.getByRole("button", { name: "添加请求头" }).click();
  await thirdPartyDialog.getByPlaceholder("X-Custom-Header").fill("X-Tenant");
  await thirdPartyDialog.getByPlaceholder("请求头值").fill("tenant-a");
  await thirdPartyDialog.getByLabel("User-Agent").fill("YAAT-Preview/1");
  await thirdPartyDialog.getByLabel("User-Agent").scrollIntoViewIfNeeded();
  await page.screenshot({
    path: path.join(output, "third-party-provider-headers.png"),
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
