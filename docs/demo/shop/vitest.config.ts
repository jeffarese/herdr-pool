import { defineConfig } from "vitest/config";

// One file at a time, like a suite that shares a database: the run is as long
// as all its files together, which is what splitting it over machines saves.
export default defineConfig({ test: { fileParallelism: false } });
