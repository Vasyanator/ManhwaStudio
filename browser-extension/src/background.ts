/*
File: browser-extension/src/background.ts

Purpose:
MV3 service-worker entry of the ManhwaStudio extension: registers the feature modules, then
starts the one bridge connection to the app and the wake-ups that keep it dialled.

Notes:
- Everything here runs at service-worker top level and synchronously registers its listeners,
  as MV3 requires for events that start a sleeping worker.
- Feature modules: one `registerModule(...)` line each, ABOVE `bridge.start()`; registration
  closes when the bridge starts (`ModuleRegistry.seal`).
- Wake-ups: browser start (`runtime.onStartup`), install/update (`runtime.onInstalled`) and a
  periodic alarm (30 s; Chrome < 120 clamps it to 60 s), because the app cannot wake the worker.
*/

import { BridgeConnection, WAKE_ALARM_NAME, chromeConnectionDeps, consoleLogger } from "./core/connection";
import { moduleRegistry, registerModule } from "./core/modules";
import { createTranslateModule } from "./modules/translate";

// --- Feature modules (registered before the bridge starts) ---------------------------------

registerModule(createTranslateModule());

// --- Bridge ---------------------------------------------------------------------------------

const bridge = new BridgeConnection(moduleRegistry, chromeConnectionDeps());

chrome.runtime.onStartup.addListener(() => bridge.wake());
chrome.runtime.onInstalled.addListener(() => bridge.wake());
chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === WAKE_ALARM_NAME) bridge.wake();
});
chrome.alarms.create(WAKE_ALARM_NAME, { periodInMinutes: 0.5 }).catch((error: unknown) => {
  consoleLogger.error("could not create the wake-up alarm; reconnects rely on backoff only", { error: String(error) });
});

bridge.start();
