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

async function readStatus(page) {
  return readJsonPre(page, "status");
}

async function readServersPayload(page) {
  return readJsonPre(page, "servers-raw");
}

async function readToolsPayload(page) {
  return readJsonPre(page, "tools-raw");
}

async function readErrorPane(page) {
  return ((await page.locator("#errors").textContent()) || "").trim();
}

async function addFixtureServer(page, fixtureServerUrl) {
  await page.fill("#server-name", "fixture");
  await page.fill("#server-url", fixtureServerUrl);
  await page.locator("#server-add-form button[type=submit]").click();
}

function resolveGambiBin() {
  const envBin = process.env.GAMBI_BIN;
  return envBin
    ? path.resolve(envBin)
    : path.resolve(__dirname, "../../..", "target/debug/gambi");
}

test.describe("admin UI", () => {
  test("renders dashboard and builtin tools in no-exec mode", async ({ page }) => {
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
        .poll(async () => readStatus(page))
        .toMatchObject({ status: "ok", exec_enabled: false });

      await expect
        .poll(async () => {
          const payload = await readServersPayload(page);
          return payload ? payload.servers : null;
        })
        .toEqual([]);

      await expect
        .poll(async () => {
          const payload = await readToolsPayload(page);
          return payload ? payload.tools : null;
        })
        .toEqual(
          expect.arrayContaining(["gambi_help", "gambi_list_servers", "gambi_list_upstream_tools"]),
        );

      await expect
        .poll(async () => {
          const payload = await readToolsPayload(page);
          return payload ? payload.tools.includes("gambi_execute") : null;
        })
        .toBe(false);

      await page.locator("#server-name").fill("fixture-local");
      await expect(page.locator("#server-name")).toBeFocused();
      const selectedBeforeRefresh = await page.evaluate(() => {
        const input = document.querySelector("#server-name");
        if (!(input instanceof HTMLInputElement)) return null;
        input.focus();
        input.setSelectionRange(0, Math.min(6, input.value.length));
        return [input.selectionStart || 0, input.selectionEnd || 0];
      });
      expect(selectedBeforeRefresh).toEqual([0, 6]);
      await page.waitForTimeout(3500);
      await expect(page.locator("#server-name")).toBeFocused();
      await expect(page.locator("#server-name")).toHaveValue("fixture-local");
      const selectedAfterRefresh = await page.evaluate(() => {
        const input = document.querySelector("#server-name");
        if (!(input instanceof HTMLInputElement)) return null;
        return [input.selectionStart || 0, input.selectionEnd || 0];
      });
      expect(selectedAfterRefresh).toEqual(selectedBeforeRefresh);

      await page.getByRole("button", { name: "Effective" }).click();
      await expect(page.locator("#tab-effective")).toBeVisible();
      await expect(page.locator("#effective-view")).toContainText(
        "initialize.instructions",
      );
      await expect
        .poll(async () => {
          const payload = await page.evaluate(async () => {
            const response = await fetch("/effective");
            if (!response.ok) return null;
            return response.json();
          });
          return payload ? payload.mcp_tools_list?.tools?.length : null;
        })
        .toBeGreaterThanOrEqual(3);
    } finally {
      await gambi.stop();
      await fs.rm(configHome, { recursive: true, force: true });
    }
  });

  test("supports tool defaults, per-tool activation toggles, and description overrides", async ({
    page,
  }) => {
    const binPath = resolveGambiBin();
    if (!(await ensurePathExists(binPath))) {
      throw new Error(
        `gambi binary not found at ${binPath}; run 'cargo build --bin gambi' first`,
      );
    }

    const configHome = await fs.mkdtemp(
      path.join(os.tmpdir(), "gambi-admin-ui-tool-activation-"),
    );
    let gambi = await startGambi({ binPath, configHome, execEnabled: false });
    const overrideText = "Fixture echo custom description from admin test";
    try {
      await page.goto(gambi.baseUrl, { waitUntil: "domcontentloaded" });
      await expect(page.getByRole("heading", { name: "gambi admin" })).toBeVisible();

      await page.selectOption("#server-tool-default-add", "none");
      await addFixtureServer(page, gambi.fixtureServerUrl);

      await expect
        .poll(async () => {
          const payload = await readServersPayload(page);
          if (!payload) return null;
          return payload.server_tool_activation_modes?.fixture || "all";
        })
        .toBe("none");

      await page.click('.tab-btn[data-tab="tools"]');

      const activationToggle = page.locator(
        'button[data-action="toggle-tool-enabled"][data-server="fixture"][data-tool="fixture_echo"]',
      );
      await expect(activationToggle).toBeVisible();
      await expect(activationToggle).toHaveText("inactive");

      await activationToggle.click();
      await expect
        .poll(async () => {
          const payload = await readServersPayload(page);
          if (!payload) return null;
          return payload.tool_activation_overrides?.fixture?.fixture_echo ?? null;
        })
        .toBe(true);
      await expect(
        page.locator(
          'button[data-action="toggle-tool-enabled"][data-server="fixture"][data-tool="fixture_echo"]',
        ),
      ).toHaveText("active");

      await page.click('[data-action="edit-desc"][data-key="fixture::fixture_echo"]');
      await page.fill("#edit-desc-text", overrideText);
      await page.click(
        'button[data-action="save-desc"][data-server="fixture"][data-tool="fixture_echo"]',
      );

      await expect
        .poll(async () => {
          const payload = await readServersPayload(page);
          if (!payload) return null;
          return payload.tool_description_overrides?.fixture?.fixture_echo ?? null;
        })
        .toBe(overrideText);

      await expect
        .poll(async () => {
          const payload = await readToolsPayload(page);
          if (!payload || !Array.isArray(payload.tool_details)) return null;
          const tool = payload.tool_details.find((item) => item.name === "fixture:fixture_echo");
          return tool ? tool.description : null;
        })
        .toBe(overrideText);

      await gambi.stop();
      gambi = await startGambi({ binPath, configHome, execEnabled: false });
      await page.goto(`${gambi.baseUrl}#tools`, { waitUntil: "domcontentloaded" });
      await expect(page.getByRole("heading", { name: "gambi admin" })).toBeVisible();

      await expect
        .poll(async () => {
          const payload = await readServersPayload(page);
          if (!payload) return null;
          return {
            defaultMode: payload.server_tool_activation_modes?.fixture || "all",
            fixtureEchoActive: payload.tool_activation_overrides?.fixture?.fixture_echo ?? null,
            fixtureEchoDesc:
              payload.tool_description_overrides?.fixture?.fixture_echo ?? null,
          };
        })
        .toEqual({
          defaultMode: "none",
          fixtureEchoActive: true,
          fixtureEchoDesc: overrideText,
        });
    } finally {
      await gambi.stop();
      await fs.rm(configHome, { recursive: true, force: true });
    }
  });

  test("supports server instruction overrides with save/restore flow", async ({ page }) => {
    const binPath = resolveGambiBin();
    if (!(await ensurePathExists(binPath))) {
      throw new Error(
        `gambi binary not found at ${binPath}; run 'cargo build --bin gambi' first`,
      );
    }

    const configHome = await fs.mkdtemp(
      path.join(os.tmpdir(), "gambi-admin-ui-server-instruction-"),
    );
    const gambi = await startGambi({ binPath, configHome, execEnabled: false });
    const overrideText = "Use fixture MCP for test-only diagnostics";
    try {
      await page.goto(gambi.baseUrl, { waitUntil: "domcontentloaded" });
      await expect(page.getByRole("heading", { name: "gambi admin" })).toBeVisible();

      await addFixtureServer(page, gambi.fixtureServerUrl);
      const fixtureCard = page.locator("#tab-servers .server-card", {
        hasText: "fixture",
      });
      await expect(fixtureCard).toBeVisible();
      await expect
        .poll(async () => {
          const payload = await readServersPayload(page);
          if (!payload) return null;
          const hasServer = Array.isArray(payload.servers)
            ? payload.servers.some((server) => server && server.name === "fixture")
            : false;
          return {
            hasServer,
            source: payload.server_instruction_sources?.fixture ?? "none",
          };
        })
        .toMatchObject({ hasServer: true });

      await fixtureCard.locator('[data-action="edit-server-instruction"]').click();
      const instructionEditor = page.locator("#edit-server-instruction-text");
      await instructionEditor.fill("Draft preserved across refresh");
      await instructionEditor.focus();
      await expect(instructionEditor).toBeFocused();
      await page.waitForTimeout(3500);
      await expect(instructionEditor).toBeFocused();
      await expect(instructionEditor).toHaveValue("Draft preserved across refresh");

      await page.fill("#edit-server-instruction-text", "   ");
      await expect(page.locator('[data-action="save-server-instruction"]')).toBeDisabled();

      await page.fill("#edit-server-instruction-text", overrideText);
      const saveInstructionButton = fixtureCard.locator(
        '[data-action="save-server-instruction"][data-server="fixture"]',
      );
      await expect(saveInstructionButton).toBeEnabled();
      await saveInstructionButton.dispatchEvent("click");
      await expect(fixtureCard.locator("#edit-server-instruction-text")).toHaveCount(0);

      await expect
        .poll(async () => {
          const payload = await readServersPayload(page);
          if (!payload) return null;
          return {
            override: payload.server_instruction_overrides?.fixture ?? null,
            effective: payload.effective_server_instructions?.fixture ?? null,
            source: payload.server_instruction_sources?.fixture ?? null,
          };
        })
        .toEqual({
          override: overrideText,
          effective: overrideText,
          source: "override",
        });

      await fixtureCard
        .locator('[data-action="restore-server-instruction"][data-server="fixture"]')
        .dispatchEvent("click");
      await expect
        .poll(async () => {
          const payload = await readServersPayload(page);
          if (!payload) return null;
          return {
            hasOverride: Boolean(payload.server_instruction_overrides?.fixture),
            effective: payload.effective_server_instructions?.fixture ?? null,
            source: payload.server_instruction_sources?.fixture ?? null,
          };
        })
        .toMatchObject({ hasOverride: false });

      const restoredPayload = await readServersPayload(page);
      const restoredInstructionSource =
        restoredPayload?.server_instruction_sources?.fixture ?? "none";
      const restoredInstruction =
        restoredPayload?.effective_server_instructions?.fixture ?? null;
      expect(["upstream", "none"]).toContain(restoredInstructionSource);
      expect(restoredInstruction).not.toBe(overrideText);
    } finally {
      await gambi.stop();
      await fs.rm(configHome, { recursive: true, force: true });
    }
  });

  test("surfaces validation errors while keeping admin state stable", async ({ page }) => {
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

      await expect.poll(async () => readErrorPane(page)).toBe("(none)");
      await expect
        .poll(async () => {
          const payload = await readServersPayload(page);
          return payload ? payload.servers : null;
        })
        .toEqual([]);

      await page.fill("#server-name", "invalid-server");
      await page.fill("#server-url", "ftp://example.com/not-supported");
      await page.locator("#server-add-form button[type=submit]").click();

      await expect.poll(async () => readErrorPane(page)).toContain("add server failed");
      await expect.poll(async () => readErrorPane(page)).toContain(
        "unsupported server url scheme",
      );
      await expect
        .poll(async () => {
          const payload = await readServersPayload(page);
          return payload ? payload.servers : null;
        })
        .toEqual([]);
    } finally {
      await gambi.stop();
      await fs.rm(configHome, { recursive: true, force: true });
    }
  });

  test("config import/export roundtrip updates admin state", async ({ page, request }) => {
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
        server_tool_activation_modes: {
          fixture: "none",
        },
        tool_activation_overrides: {
          fixture: {
            fixture_echo: true,
          },
        },
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
      await page.click('.tab-btn[data-tab="tools"]');

      await expect
        .poll(async () => {
          const payload = await readServersPayload(page);
          if (!payload) return null;
          return {
            servers: payload.servers?.map((server) => server.name) || [],
            defaultMode: payload.server_tool_activation_modes?.fixture || "all",
            fixtureEchoActive: payload.tool_activation_overrides?.fixture?.fixture_echo ?? null,
          };
        })
        .toEqual({
          servers: ["fixture"],
          defaultMode: "none",
          fixtureEchoActive: true,
        });

      await expect
        .poll(async () => {
          const payload = await readToolsPayload(page);
          if (!payload || !Array.isArray(payload.tool_details)) return null;
          const tool = payload.tool_details.find((item) => item.name === "fixture:fixture_echo");
          return tool ? tool.description : null;
        })
        .toBe("Imported override description");
    } finally {
      await gambi.stop();
      await fs.rm(configHome, { recursive: true, force: true });
    }
  });
});
