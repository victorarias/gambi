const fs = require("node:fs/promises");
const os = require("node:os");
const path = require("node:path");

const { test, expect } = require("@playwright/test");

const { ensurePathExists, startGambi } = require("./helpers/gambi");

async function readJsonPre(page, id) {
  const text = (await page.locator(`#${id}`).textContent()) || "";
  const trimmed = text.trim();
  if (!trimmed || trimmed === "loading...") {
    return null;
  }
  try {
    return JSON.parse(trimmed);
  } catch {
    return null;
  }
}

async function readToolsPayload(page) {
  return readJsonPre(page, "tools");
}

async function readErrorPane(page) {
  return ((await page.locator("#errors").textContent()) || "").trim();
}

async function getToolDescription(page, name) {
  const payload = await readToolsPayload(page);
  if (!payload || !Array.isArray(payload.tool_details)) {
    return null;
  }
  const detail = payload.tool_details.find((item) => item.name === name);
  return detail ? detail.description : null;
}

async function selectOptions(page, selector) {
  return page.locator(selector).evaluate((element) => {
    return Array.from(element.options).map((option) => option.value);
  });
}

function resolveGambiBin() {
  const envBin = process.env.GAMBI_BIN;
  return envBin
    ? path.resolve(envBin)
    : path.resolve(__dirname, "../../..", "target/debug/gambi");
}

test.describe("admin UI", () => {
  test("renders dashboard status and builtin tools", async ({ page }) => {
    const binPath = resolveGambiBin();
    if (!(await ensurePathExists(binPath))) {
      throw new Error(
        `gambi binary not found at ${binPath}; run 'cargo build --bin gambi' first`,
      );
    }

    const configHome = await fs.mkdtemp(
      path.join(os.tmpdir(), "gambi-admin-ui-baseline-"),
    );
    const gambi = await startGambi({ binPath, configHome, execEnabled: false });
    try {
      await page.goto(gambi.baseUrl, { waitUntil: "domcontentloaded" });
      await expect(page.getByRole("heading", { name: "gambi admin" })).toBeVisible();

      await expect
        .poll(async () => readJsonPre(page, "status"), {
          message: "status endpoint should load in UI",
        })
        .toMatchObject({ status: "ok", exec_enabled: false });

      await expect
        .poll(async () => readJsonPre(page, "servers"), {
          message: "servers list should be loaded",
        })
        .toEqual([]);

      await expect
        .poll(async () => readJsonPre(page, "overrides"), {
          message: "overrides map should be loaded",
        })
        .toEqual({});

      await expect
        .poll(async () => {
          const payload = await readToolsPayload(page);
          return payload ? payload.tools : null;
        })
        .toEqual(
          expect.arrayContaining(["gambi_list_servers", "gambi_list_upstream_tools"]),
        );

      await expect
        .poll(async () => {
          const payload = await readToolsPayload(page);
          return payload ? payload.tools.includes("gambi_execute") : null;
        })
        .toBe(false);
    } finally {
      await gambi.stop();
      await fs.rm(configHome, { recursive: true, force: true });
    }
  });

  test("manages servers and description overrides with restart persistence", async ({
    page,
  }) => {
    const binPath = resolveGambiBin();
    if (!(await ensurePathExists(binPath))) {
      throw new Error(
        `gambi binary not found at ${binPath}; run 'cargo build --bin gambi' first`,
      );
    }

    const customDescription = "Custom fixture echo description from admin UI";
    const configHome = await fs.mkdtemp(
      path.join(os.tmpdir(), "gambi-admin-ui-override-"),
    );
    let gambi = await startGambi({ binPath, configHome, execEnabled: false });
    try {
      await page.goto(gambi.baseUrl, { waitUntil: "domcontentloaded" });
      await expect(page.getByRole("heading", { name: "gambi admin" })).toBeVisible();

      await page.fill("#server-name", "fixture");
      await page.fill("#server-url", gambi.fixtureServerUrl);
      await page.locator("#server-add-form button[type=submit]").click();

      await expect
        .poll(async () => {
          const servers = await readJsonPre(page, "servers");
          return Array.isArray(servers)
            ? servers.map((server) => server.name).sort()
            : null;
        })
        .toEqual(["fixture"]);

      await expect.poll(async () => selectOptions(page, "#server-remove-name")).toContain(
        "fixture",
      );
      await expect.poll(async () => selectOptions(page, "#override-server")).toContain(
        "fixture",
      );
      await expect
        .poll(async () => selectOptions(page, "#override-remove-server"))
        .toContain("fixture");

      await page.selectOption("#override-server", "fixture");
      await page.fill("#override-tool", "fixture_echo");
      await page.fill("#override-description", customDescription);
      await page.locator("#override-set-form button[type=submit]").click();

      await expect
        .poll(async () => readJsonPre(page, "overrides"), {
          message: "override map should include custom description",
        })
        .toEqual({
          fixture: {
            fixture_echo: customDescription,
          },
        });

      await expect
        .poll(async () => getToolDescription(page, "fixture:fixture_echo"))
        .toBe(customDescription);

      await gambi.stop();
      gambi = await startGambi({ binPath, configHome, execEnabled: false });
      await page.goto(gambi.baseUrl, { waitUntil: "domcontentloaded" });

      await expect
        .poll(async () => {
          const servers = await readJsonPre(page, "servers");
          return Array.isArray(servers)
            ? servers.map((server) => server.name).sort()
            : null;
        })
        .toEqual(["fixture"]);

      await expect
        .poll(async () => readJsonPre(page, "overrides"))
        .toEqual({
          fixture: {
            fixture_echo: customDescription,
          },
        });

      await expect
        .poll(async () => getToolDescription(page, "fixture:fixture_echo"))
        .toBe(customDescription);

      await page.selectOption("#override-remove-server", "fixture");
      await page.fill("#override-remove-tool", "fixture_echo");
      await page.locator("#override-remove-form button[type=submit]").click();

      await expect.poll(async () => readJsonPre(page, "overrides")).toEqual({});
      await expect
        .poll(async () => getToolDescription(page, "fixture:fixture_echo"))
        .toBe("Echo structured arguments for execution-bridge tests");

      await page.selectOption("#server-remove-name", "fixture");
      await page.locator("#server-remove-form button[type=submit]").click();

      await expect.poll(async () => readJsonPre(page, "servers")).toEqual([]);
      await expect.poll(async () => readJsonPre(page, "overrides")).toEqual({});
      await expect
        .poll(async () => getToolDescription(page, "fixture:fixture_echo"))
        .toBe(null);
    } finally {
      await gambi.stop();
      await fs.rm(configHome, { recursive: true, force: true });
    }
  });

  test("surfaces validation errors and keeps admin state unchanged", async ({ page }) => {
    const binPath = resolveGambiBin();
    if (!(await ensurePathExists(binPath))) {
      throw new Error(
        `gambi binary not found at ${binPath}; run 'cargo build --bin gambi' first`,
      );
    }

    const configHome = await fs.mkdtemp(
      path.join(os.tmpdir(), "gambi-admin-ui-errors-"),
    );
    const gambi = await startGambi({ binPath, configHome, execEnabled: false });
    try {
      await page.goto(gambi.baseUrl, { waitUntil: "domcontentloaded" });
      await expect(page.getByRole("heading", { name: "gambi admin" })).toBeVisible();

      await expect.poll(async () => readJsonPre(page, "servers")).toEqual([]);
      await expect.poll(async () => readJsonPre(page, "overrides")).toEqual({});
      await expect.poll(async () => readErrorPane(page)).toBe("(none)");
      await expect(page.locator("#server-remove-name")).toBeDisabled();
      await expect(
        page.locator("#server-remove-form button[type=submit]"),
      ).toBeDisabled();
      await expect(page.locator("#override-server")).toBeDisabled();
      await expect(
        page.locator("#override-set-form button[type=submit]"),
      ).toBeDisabled();
      await expect(page.locator("#override-remove-server")).toBeDisabled();
      await expect(
        page.locator("#override-remove-form button[type=submit]"),
      ).toBeDisabled();

      await page.fill("#server-name", "invalid-server");
      await page.fill("#server-url", "ftp://example.com/not-supported");
      await page.locator("#server-add-form button[type=submit]").click();

      await expect.poll(async () => readErrorPane(page)).toContain("add server failed");
      await expect.poll(async () => readErrorPane(page)).toContain(
        "unsupported server url scheme",
      );
      await expect.poll(async () => readJsonPre(page, "servers")).toEqual([]);
      await expect(page.locator("#server-remove-name")).toBeDisabled();
      await expect(page.locator("#override-server")).toBeDisabled();
      await expect(page.locator("#override-remove-server")).toBeDisabled();

      await page.fill("#server-name", "fixture");
      await page.fill("#server-url", gambi.fixtureServerUrl);
      await page.locator("#server-add-form button[type=submit]").click();

      await expect
        .poll(async () => {
          const servers = await readJsonPre(page, "servers");
          return Array.isArray(servers) ? servers.map((server) => server.name) : null;
        })
        .toEqual(["fixture"]);
      await expect.poll(async () => readErrorPane(page)).toBe("(none)");

      await page.selectOption("#override-remove-server", "fixture");
      await page.fill("#override-remove-tool", "missing_tool");
      await page.locator("#override-remove-form button[type=submit]").click();

      await expect.poll(async () => readErrorPane(page)).toContain("remove override failed");
      await expect.poll(async () => readErrorPane(page)).toContain("override not found");
      await expect.poll(async () => readJsonPre(page, "overrides")).toEqual({});
    } finally {
      await gambi.stop();
      await fs.rm(configHome, { recursive: true, force: true });
    }
  });

  test("loads auth/log panels and applies config import-export roundtrip", async ({
    page,
    request,
  }) => {
    const binPath = resolveGambiBin();
    if (!(await ensurePathExists(binPath))) {
      throw new Error(
        `gambi binary not found at ${binPath}; run 'cargo build --bin gambi' first`,
      );
    }

    const configHome = await fs.mkdtemp(
      path.join(os.tmpdir(), "gambi-admin-ui-config-roundtrip-"),
    );
    const gambi = await startGambi({ binPath, configHome, execEnabled: false });
    try {
      await page.goto(gambi.baseUrl, { waitUntil: "domcontentloaded" });
      await expect(page.getByRole("heading", { name: "gambi admin" })).toBeVisible();

      await expect
        .poll(async () => readJsonPre(page, "auth"), {
          message: "auth panel should load JSON payload",
        })
        .toMatchObject({ statuses: expect.any(Array) });

      await expect
        .poll(async () => {
          const logs = (await page.locator("#logs").textContent()) || "";
          return logs.trim();
        })
        .not.toBe("loading...");

      const exported = await request.get(`${gambi.baseUrl}/config/export`);
      expect(exported.ok()).toBeTruthy();
      const exportedJson = await exported.json();
      expect(exportedJson.servers).toEqual([]);

      const importPayload = {
        servers: [
          {
            name: "fixture",
            url: gambi.fixtureServerUrl,
          },
        ],
        tool_description_overrides: {
          fixture: {
            fixture_echo: "Imported override description",
          },
        },
      };

      const imported = await request.post(`${gambi.baseUrl}/config/import`, {
        data: importPayload,
      });
      expect(imported.ok()).toBeTruthy();

      await page.reload({ waitUntil: "domcontentloaded" });
      await expect
        .poll(async () => {
          const servers = await readJsonPre(page, "servers");
          return Array.isArray(servers) ? servers.map((server) => server.name) : null;
        })
        .toEqual(["fixture"]);
      await expect.poll(async () => readJsonPre(page, "overrides")).toEqual({
        fixture: {
          fixture_echo: "Imported override description",
        },
      });
      await expect
        .poll(async () => getToolDescription(page, "fixture:fixture_echo"))
        .toBe("Imported override description");
    } finally {
      await gambi.stop();
      await fs.rm(configHome, { recursive: true, force: true });
    }
  });
});
