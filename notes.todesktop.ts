import { defineConfig } from '@todesktop/v2-config';

// Acme Notes: a small app with a window, built by .github/workflows/notes.yml.
export default defineConfig({
  appId: 'acme-notes',
  description: 'A small notes app (test app for ToDesktop v2 smoke tests)',
  homepage: 'https://acme.example/notes',
  maintainer: 'Acme <support@acme.example>',
  productName: 'Acme Notes',
  linux: {
    apps: { arm64: 'dist/notes-linux-arm64', x64: 'dist/notes-linux-x64' },
    executable: 'acme-notes',
    formats: ['deb', 'rpm', 'pacman', 'tar.gz', 'appimage'],
  },
});
