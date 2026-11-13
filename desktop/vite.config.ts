import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  envPrefix: ['VITE_', 'TAURI_ENV_*'],
  server: { watch: { ignored: ['**/src-tauri/**'] } },
  test: { environment: 'jsdom', setupFiles: ['./src/test-setup.ts'] },
});
