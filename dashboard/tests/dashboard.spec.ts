import { test, expect, request } from "@playwright/test";

test("profile cockpit explains the current task and drills into node lanes", async ({
  page,
}) => {
  await page.goto("/");
  await expect(page).toHaveTitle("Distributed Workbench");
  await expect(page.getByText("Profile 任务驾驶舱 · 只读")).toBeVisible();
  await expect(page.getByText("每个 Profile，现在进行到哪里？")).toBeVisible();
  const profile = page.getByRole("button", { name: /profile-a/ });
  await expect(profile).toContainText("已阻塞");
  await expect(profile).toContainText("等待依赖：等待上游构建产物");
  await profile.click();
  await expect(page).toHaveURL(/\/profiles\/profile-a$/);
  await expect(page.getByText("阶段与节点泳道")).toBeVisible();
  await expect(
    page.locator(".lane-label").getByText("devbox-a", { exact: true }),
  ).toBeVisible();
  await expect(
    page.locator(".lane-label").getByText("主 Agent", { exact: true }),
  ).toBeVisible();
  await expect(page.getByText("任务被阻塞")).toBeVisible();
  await expect(page.locator(".stage.current")).toContainText("构建");
  await page.getByRole("button", { name: "查看证据", exact: true }).click();
  await expect(page.getByText("STRUCTURED EVIDENCE")).toBeVisible();
  await expect(
    page.getByText("原始 prompt、token、完整命令和未授权日志不会出现在此处。"),
  ).toBeVisible();
  await page.getByRole("button", { name: "关闭证据" }).click();
  await page.getByRole("button", { name: "← 返回所有 Profile" }).click();
  await expect(page).toHaveURL(/\/$/);
  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.goto("/profiles/profile-a");
  await expect(page.getByText("交付示例功能")).toBeVisible();
  await expect(page.locator(".stage.current")).toContainText("构建");
  await page.route("**/api/workspaces", (route) =>
    route.fulfill({
      json: {
        generatedAt: Date.now(),
        nodes: [{ id: "MacBook-Pro-rust", health: "ready" }],
        workspaces: [],
        tunnels: [
          {
            id: "bear-4105",
            executorId: "MacBook-Pro-rust",
            workspaceSessionId: "profile-a",
            sshHost: "cndevbox",
            direction: "local-forward",
            source: { host: "127.0.0.1", port: 4105 },
            destination: { host: "127.0.0.1", port: 4105 },
            desiredState: "running",
            observedState: "ready",
            lastProbeAt: Date.now(),
          },
        ],
      },
    }),
  );
  await page.goto("/");
  await expect(page.getByText("节点间 Tunnel")).toBeVisible();
  await expect(
    page.locator(".tunnel-row").filter({ hasText: "bear-4105" }),
  ).toContainText("MacBook-Pro-rust");
  await expect(
    page.locator(".tunnel-row").filter({ hasText: "bear-4105" }),
  ).toContainText("127.0.0.1:4105");
  await expect(page.getByText("ssh -N")).toHaveCount(0);
});

test("API and repeat bootstrap reject a client without operator cookie", async () => {
  const client = await request.newContext();
  const workspace = await client.get("http://127.0.0.1:19918/api/workspaces", {
    headers: { Host: "127.0.0.1:19918" },
  });
  expect(workspace.status()).toBe(401);
  await client.dispose();
  const freshClient = await request.newContext();
  const root = await freshClient.get("http://127.0.0.1:19918/", {
    headers: { Host: "127.0.0.1:19918", "Sec-Fetch-Dest": "document" },
  });
  expect(root.status()).toBe(401);
  await freshClient.dispose();
});
