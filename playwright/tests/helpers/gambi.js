const fs = require("node:fs/promises");
const path = require("node:path");
const { spawn } = require("node:child_process");
const { once } = require("node:events");

async function ensurePathExists(filePath) {
  try {
    await fs.access(filePath);
    return true;
  } catch {
    return false;
  }
}

async function ensureConfigScaffold(configHome) {
  const gambiDir = path.join(configHome, "gambi");
  await fs.mkdir(gambiDir, { recursive: true });

  const configPath = path.join(gambiDir, "config.json");
  const tokensPath = path.join(gambiDir, "tokens.json");

  if (!(await ensurePathExists(configPath))) {
    await fs.writeFile(configPath, "{\n  \"servers\": []\n}\n", "utf8");
  }
  if (!(await ensurePathExists(tokensPath))) {
    await fs.writeFile(tokensPath, "{}\n", "utf8");
  }

  if (process.platform !== "win32") {
    await fs.chmod(configPath, 0o600);
    await fs.chmod(tokensPath, 0o600);
  }
}

async function waitForAdminPortFile(portFile, child, stderrLines, timeoutMs = 20_000) {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    if (child.exitCode !== null) {
      const stderr = stderrLines.slice(-20).join("\n");
      throw new Error(
        `gambi exited before admin port file was written (code=${child.exitCode}). stderr:\n${stderr}`,
      );
    }
    try {
      const raw = await fs.readFile(portFile, "utf8");
      const parsed = Number.parseInt(raw.trim(), 10);
      if (!Number.isNaN(parsed) && parsed > 0 && parsed <= 65535) {
        return parsed;
      }
    } catch (error) {
      if (error && error.code !== "ENOENT") {
        throw error;
      }
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`admin port file did not become ready: ${portFile}`);
}

async function waitForHealthy(baseUrl, child, stderrLines, timeoutMs = 20_000) {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    if (child.exitCode !== null) {
      const stderr = stderrLines.slice(-20).join("\n");
      throw new Error(
        `gambi exited before health became ready (code=${child.exitCode}). stderr:\n${stderr}`,
      );
    }
    try {
      const response = await fetch(`${baseUrl}/health`);
      if (response.ok) {
        return;
      }
    } catch {
      // server not ready yet
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }

  const stderr = stderrLines.slice(-20).join("\n");
  throw new Error(
    `gambi health endpoint did not become ready in time. stderr tail:\n${stderr}`,
  );
}

async function stopChildProcess(child) {
  if (!child || child.exitCode !== null) {
    return;
  }

  child.kill("SIGTERM");
  const exitResult = await Promise.race([
    once(child, "exit").then(() => "exited"),
    new Promise((resolve) => setTimeout(() => resolve("timeout"), 5_000)),
  ]);

  if (exitResult === "timeout" && child.exitCode === null) {
    child.kill("SIGKILL");
    await once(child, "exit");
  }
}

async function startGambi({ binPath, configHome, execEnabled = false }) {
  await ensureConfigScaffold(configHome);

  const portFile = path.join(configHome, "gambi-admin-port.txt");
  await fs.rm(portFile, { force: true });
  const args = ["serve", "--admin-port", "0", "--admin-port-file", portFile];
  if (!execEnabled) {
    args.push("--no-exec");
  }

  const stderrLines = [];
  const child = spawn(binPath, args, {
    env: {
      ...process.env,
      XDG_CONFIG_HOME: configHome,
      RUST_LOG: process.env.RUST_LOG || "error",
    },
    stdio: ["pipe", "pipe", "pipe"],
  });

  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk) => {
    for (const line of chunk.split(/\r?\n/)) {
      if (!line) {
        continue;
      }
      stderrLines.push(line);
      if (stderrLines.length > 200) {
        stderrLines.shift();
      }
    }
  });

  const adminPort = await waitForAdminPortFile(portFile, child, stderrLines);
  const baseUrl = `http://127.0.0.1:${adminPort}`;
  await waitForHealthy(baseUrl, child, stderrLines);

  return {
    baseUrl,
    fixtureServerUrl: `stdio:///${binPath}?arg=__fixture_progress_server`,
    async stop() {
      await stopChildProcess(child);
    },
  };
}

module.exports = {
  ensurePathExists,
  startGambi,
};
