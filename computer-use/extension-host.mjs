// Adapter only: Pi loads the unmodified, lockfile-pinned computer-use extension.
import path from 'node:path';
import os from 'node:os';
import { mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { Value } from 'typebox/value';
import { configureEnvironment } from './environment.mjs';
import { configureLinuxRuntime } from './linux-runtime.mjs';

export const packageRoot = path.dirname(fileURLToPath(import.meta.url));
export function configurePaths(stateDir) {
  const dataHome = process.env.XDG_DATA_HOME || (process.platform === 'win32'
    ? process.env.LOCALAPPDATA : path.join(os.homedir(), '.local', 'share'));
  const helpers = path.join(dataHome, 'distributed-workbench', 'computer-use');
  process.env.PI_CODING_AGENT_DIR = path.join(stateDir, 'pi');
  process.env.PI_COMPUTER_USE_WINDOWS_HELPER_PATH = path.join(helpers, 'windows-bridge.exe');
  process.env.PI_COMPUTER_USE_LINUX_HELPER_PATH = path.join(helpers, 'linux-bridge');
  process.env.PI_COMPUTER_USE_HELPER_APP_PATH = path.join(helpers, 'pi-computer-use.app');
  // Keep the upstream macOS socket default: it owns LaunchServices startup.
  process.env.PI_COMPUTER_USE_CURSOR_OVERLAY = 'false';
  return helpers;
}

export async function loadHost(stateDir) {
  await configureEnvironment(stateDir);
  configurePaths(stateDir);
  await mkdir(stateDir, { recursive: true, mode: 0o700 });
  const restoreRuntime = await configureLinuxRuntime(stateDir, process.env.PI_COMPUTER_USE_LINUX_HELPER_PATH);
  try {
    const sdk = await import('@earendil-works/pi-coding-agent');
    const extensionPath = path.join(packageRoot, 'node_modules', '@injaneity', 'pi-computer-use', 'extensions', 'computer-use.ts');
    const loaded = await sdk.discoverAndLoadExtensions([extensionPath], stateDir, process.env.PI_CODING_AGENT_DIR);
    if (loaded.errors.length) throw new Error(JSON.stringify(loaded.errors));
    if (loaded.extensions.length !== 1) throw new Error('Expected exactly the pinned computer-use extension');
    const models = await sdk.ModelRuntime.create({ authPath: path.join(stateDir, 'unused-auth.json'), modelsPath: null, refreshOnCreate: false, allowModelNetwork: false });
    const runner = new sdk.ExtensionRunner(loaded.extensions, loaded.runtime, stateDir,
      sdk.SessionManager.inMemory(stateDir), new sdk.ModelRegistry(models));
    loaded.runtime.getActiveTools = () => runner.getAllRegisteredTools().map(t => t.definition.name);
    await runner.emit({ type: 'session_start' });
    const tools = new Map(sdk.wrapRegisteredTools(runner.getAllRegisteredTools(), runner).map(t => [t.name, t]));
    return {
      tools: () => [...tools.values()].map(({name, description, parameters}) => ({name, description, inputSchema: parameters})),
      async call(name, args, id, signal) {
        const tool = tools.get(name);
        if (!tool) throw new Error(`Unknown computer-use tool: ${name}`);
        if (!Value.Check(tool.parameters, args)) throw new Error(`Invalid arguments for ${name}: ${JSON.stringify([...Value.Errors(tool.parameters, args)])}`);
        return await tool.execute(id, args, signal);
      },
      async close() {
        try { await runner.emit({ type: 'session_shutdown', reason: 'exit' }); }
        finally { restoreRuntime(); }
      },
    };
  } catch (error) {
    restoreRuntime();
    throw error;
  }
}
